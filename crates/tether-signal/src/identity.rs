//! Per-device identity for the signal directory.
//!
//! The shared `--secret` is a coarse admission gate: it keeps strangers off the
//! server, but every peer past it is equally privileged, so any of them could
//! `Register` someone else's `device_id` and evict the live host. Pairing
//! defeats *impersonation* of a media session; it does nothing about that
//! availability hit (documented in docs/deferred.md since Phase 5).
//!
//! So registration is additionally proved with an Ed25519 signature over a
//! server-issued nonce, and the server pins `device_id -> pubkey` on first use.
//! A later registration for a pinned id with a different key is refused instead
//! of evicting the incumbent.
//!
//! **Trust on first use** is the honest description of the guarantee: the very
//! first registration for a fresh `device_id` is taken on faith, exactly like
//! SSH's known_hosts. What it buys is that a squatter must win the race to the
//! *first* registration rather than being able to evict at any time — and once
//! an operator has started their host once, the window is closed for good.
//!
//! Hosts must always carry an identity. Controllers may omit one *unless* their
//! id is already pinned — without that exception, omitting the key would be a
//! trivial bypass of the whole mechanism.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Domain separation: a signature produced here can never be replayed as a
/// signature over anything else tether might sign in future.
const REGISTER_CONTEXT: &[u8] = b"tether-signal-register-v1";

/// Bytes both sides sign/verify for a registration. Binding the `device_id`
/// (not just the nonce) is what stops a signature captured for one device from
/// being replayed to claim another within the same connection.
pub fn register_payload(nonce: &str, device_id: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(REGISTER_CONTEXT.len() + nonce.len() + device_id.len() + 2);
    v.extend_from_slice(REGISTER_CONTEXT);
    v.push(0);
    v.extend_from_slice(nonce.as_bytes());
    v.push(0);
    v.extend_from_slice(device_id.as_bytes());
    v
}

/// A fresh per-connection nonce, hex encoded. 32 bytes, so a client's signature
/// can't be precomputed or replayed onto another connection.
pub fn new_nonce() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("system RNG must not fail");
    hex::encode(buf)
}

/// Why a registration's identity was refused. Each maps to a distinct
/// `ErrorCode` so the client can tell "you need a key" from "your key is wrong".
#[derive(Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// Identity is mandatory here (a host, or an already-pinned id) and absent.
    Missing,
    /// Malformed hex, wrong length, or not a valid curve point.
    Malformed(&'static str),
    /// The signature does not verify under the presented key.
    BadSignature,
    /// The presented key differs from the one pinned for this `device_id`.
    Mismatch,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "this device id requires a signed registration"),
            Self::Malformed(what) => write!(f, "malformed identity: {what}"),
            Self::BadSignature => write!(f, "registration signature did not verify"),
            Self::Mismatch => write!(
                f,
                "device id is pinned to a different identity key; refusing to replace it"
            ),
        }
    }
}

/// Verify a presented key + signature against the connection's nonce. Pure:
/// pinning is the caller's job, so this stays trivially testable.
fn verify_signature(
    pubkey_hex: &str,
    sig_hex: &str,
    nonce: &str,
    device_id: &str,
) -> Result<VerifyingKey, IdentityError> {
    let key_bytes: [u8; 32] = hex::decode(pubkey_hex)
        .map_err(|_| IdentityError::Malformed("pubkey is not hex"))?
        .try_into()
        .map_err(|_| IdentityError::Malformed("pubkey is not 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| IdentityError::Malformed("pubkey is not a valid Ed25519 point"))?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|_| IdentityError::Malformed("signature is not hex"))?
        .try_into()
        .map_err(|_| IdentityError::Malformed("signature is not 64 bytes"))?;
    let sig = Signature::from_bytes(&sig_bytes);
    key.verify_strict(&register_payload(nonce, device_id), &sig)
        .map_err(|_| IdentityError::BadSignature)?;
    Ok(key)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pinned {
    /// device_id -> hex public key.
    devices: HashMap<String, String>,
}

/// The pinned `device_id -> pubkey` directory, persisted so a restart does not
/// reopen the first-use window for every device.
pub struct IdentityStore {
    pinned: Pinned,
    path: Option<PathBuf>,
}

impl IdentityStore {
    /// Load from `path`, or start empty if it does not exist yet. A corrupt or
    /// unreadable store is fatal rather than silently ignored: continuing with
    /// an empty map would silently un-pin every device, which is precisely the
    /// state an attacker wants.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let pinned = if path.exists() {
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
        } else {
            Pinned::default()
        };
        Ok(Self {
            pinned,
            path: Some(path.to_path_buf()),
        })
    }

    /// An in-memory store — pinning holds for the process lifetime only. Used by
    /// tests and by a server started without `--identity-store`.
    pub fn ephemeral() -> Self {
        Self {
            pinned: Pinned::default(),
            path: None,
        }
    }

    pub fn len(&self) -> usize {
        self.pinned.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pinned.devices.is_empty()
    }

    /// Check a registration and pin the key if this id is new.
    ///
    /// `requires_identity` is true for hosts; an id that is already pinned
    /// always requires one regardless, or omitting the key would bypass the pin.
    pub fn authorize(
        &mut self,
        device_id: &str,
        nonce: &str,
        pubkey: Option<&str>,
        sig: Option<&str>,
        requires_identity: bool,
    ) -> Result<(), IdentityError> {
        let known = self.pinned.devices.get(device_id).cloned();
        let (Some(pubkey), Some(sig)) = (pubkey, sig) else {
            return if known.is_some() || requires_identity {
                Err(IdentityError::Missing)
            } else {
                Ok(())
            };
        };

        // Verify before comparing against the pin: an unverified key is just an
        // attacker-supplied string, and comparing it first would let a squatter
        // learn whether an id is pinned without holding any key at all.
        verify_signature(pubkey, sig, nonce, device_id)?;

        match known {
            Some(existing) if existing != pubkey => {
                warn!(
                    %device_id,
                    "refused registration: device id is pinned to a different identity key"
                );
                Err(IdentityError::Mismatch)
            }
            Some(_) => Ok(()),
            None => {
                info!(%device_id, "pinning new device identity (trust on first use)");
                self.pinned
                    .devices
                    .insert(device_id.to_owned(), pubkey.to_owned());
                if let Err(e) = self.persist() {
                    // Non-fatal: the pin holds in memory for this process, so
                    // refusing the registration would trade an availability
                    // outage for a durability problem. Loud, though.
                    warn!(error = %e, "could not persist identity store");
                }
                Ok(())
            }
        }
    }

    fn persist(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_vec_pretty(&self.pinned)?;
        // tmp + rename in the same directory, so a crash mid-write can't leave a
        // truncated store that would un-pin every device on restart.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn signed(k: &SigningKey, nonce: &str, device_id: &str) -> (String, String) {
        let pubkey = hex::encode(k.verifying_key().to_bytes());
        let sig = hex::encode(k.sign(&register_payload(nonce, device_id)).to_bytes());
        (pubkey, sig)
    }

    #[test]
    fn pins_on_first_use_and_accepts_the_same_key_again() {
        let mut store = IdentityStore::ephemeral();
        let k = key(1);
        let (pk, sig) = signed(&k, "nonce-a", "mac");
        assert_eq!(
            store.authorize("mac", "nonce-a", Some(&pk), Some(&sig), true),
            Ok(())
        );
        assert_eq!(store.len(), 1);

        // Same key, a fresh connection nonce.
        let (pk2, sig2) = signed(&k, "nonce-b", "mac");
        assert_eq!(
            store.authorize("mac", "nonce-b", Some(&pk2), Some(&sig2), true),
            Ok(())
        );
        assert_eq!(store.len(), 1, "re-registering must not add a second pin");
    }

    /// The squatting case this whole module exists for.
    #[test]
    fn refuses_a_different_key_for_a_pinned_id() {
        let mut store = IdentityStore::ephemeral();
        let (pk, sig) = signed(&key(1), "n", "mac");
        store
            .authorize("mac", "n", Some(&pk), Some(&sig), true)
            .unwrap();

        let (evil_pk, evil_sig) = signed(&key(2), "n", "mac");
        assert_eq!(
            store.authorize("mac", "n", Some(&evil_pk), Some(&evil_sig), true),
            Err(IdentityError::Mismatch)
        );
    }

    /// Omitting the key must not be a way around the pin.
    #[test]
    fn pinned_id_cannot_drop_back_to_no_identity() {
        let mut store = IdentityStore::ephemeral();
        let (pk, sig) = signed(&key(1), "n", "phone");
        store
            .authorize("phone", "n", Some(&pk), Some(&sig), false)
            .unwrap();
        assert_eq!(
            store.authorize("phone", "n", None, None, false),
            Err(IdentityError::Missing)
        );
    }

    #[test]
    fn hosts_must_present_an_identity_but_fresh_controllers_need_not() {
        let mut store = IdentityStore::ephemeral();
        assert_eq!(
            store.authorize("mac", "n", None, None, true),
            Err(IdentityError::Missing)
        );
        assert_eq!(store.authorize("phone", "n", None, None, false), Ok(()));
        assert!(store.is_empty(), "an identity-less peer must not be pinned");
    }

    /// A signature is bound to both the nonce and the device id.
    #[test]
    fn signature_does_not_transfer_across_nonce_or_device() {
        let k = key(3);
        let (pk, sig) = signed(&k, "nonce-a", "mac");

        let mut store = IdentityStore::ephemeral();
        assert_eq!(
            store.authorize("mac", "nonce-b", Some(&pk), Some(&sig), true),
            Err(IdentityError::BadSignature),
            "replaying onto another connection's nonce must fail"
        );
        assert_eq!(
            store.authorize("laptop", "nonce-a", Some(&pk), Some(&sig), true),
            Err(IdentityError::BadSignature),
            "reusing a signature to claim another device id must fail"
        );
    }

    #[test]
    fn malformed_identities_are_rejected_not_panicked_on() {
        let mut store = IdentityStore::ephemeral();
        let (pk, sig) = signed(&key(4), "n", "mac");
        for (pubkey, signature) in [
            ("zz", sig.as_str()),
            (pk.as_str(), "zz"),
            ("00", sig.as_str()),
            (pk.as_str(), "00"),
            // 32 valid hex bytes that are not a curve point.
            (&"ff".repeat(32), sig.as_str()),
        ] {
            assert!(matches!(
                store.authorize("mac", "n", Some(pubkey), Some(signature), true),
                Err(IdentityError::Malformed(_)) | Err(IdentityError::BadSignature)
            ));
        }
        assert!(store.is_empty(), "nothing may be pinned by a bad attempt");
    }

    /// Pinned against controller/src/identity.test.ts. The controller signs
    /// these exact bytes, so a change on either side must break both.
    #[test]
    fn register_payload_cross_implementation_vector() {
        assert_eq!(
            hex::encode(register_payload("abc", "mac")),
            "7465746865722d7369676e616c2d72656769737465722d763100616263006d6163"
        );
    }

    /// The separators are what stop `(nonce="a", id="bc")` and
    /// `(nonce="ab", id="c")` from signing the same bytes.
    #[test]
    fn payload_is_unambiguous_across_the_nonce_device_boundary() {
        assert_ne!(register_payload("a", "bc"), register_payload("ab", "c"));
    }

    #[test]
    fn store_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("tether-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identities.json");
        let _ = std::fs::remove_file(&path);

        let (pk, sig) = signed(&key(5), "n", "mac");
        let mut store = IdentityStore::load(&path).unwrap();
        store
            .authorize("mac", "n", Some(&pk), Some(&sig), true)
            .unwrap();

        let mut reloaded = IdentityStore::load(&path).unwrap();
        assert_eq!(reloaded.len(), 1);
        let (evil_pk, evil_sig) = signed(&key(6), "n", "mac");
        assert_eq!(
            reloaded.authorize("mac", "n", Some(&evil_pk), Some(&evil_sig), true),
            Err(IdentityError::Mismatch),
            "a restart must not reopen the first-use window"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
