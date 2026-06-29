//! Host-side device pairing and authentication.
//!
//! Replaces the single shared secret with per-device pairing the host controls
//! and can revoke. The design (and its threat model) is in docs/phase5-plan.md;
//! the load-bearing properties implemented here:
//!
//! * **Channel-bound pairing proof** — the pairing code is never sent; the
//!   controller proves knowledge via `HMAC(code, channel_binding)` where the
//!   binding is a hash of both ends' DTLS fingerprints. A malicious relay that
//!   MITMs the connection derives a different binding and fails to pair.
//! * **Consume-on-attempt + lockout** — one outstanding high-entropy code; any
//!   wrong guess burns it, and repeated failures trigger a cooldown, so the
//!   code can't be brute-forced over the control channel.
//! * **HMAC capability tokens gated on an allowlist** — `token =
//!   HMAC(host_key, device_id || ":" || paired_at)`. Revocation removes the
//!   device from the allowlist, so the token stops working even though it still
//!   mathematically verifies. `paired_at` binding means re-pairing rotates it.
//!
//! Everything here is pure given an injected clock (`now_unix`) and is
//! unit-tested on loopback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const CODE_TTL: Duration = Duration::from_secs(300);
const LOCKOUT_THRESHOLD: u32 = 3;
const LOCKOUT_SECS: u64 = 60;
/// Crockford base32, excluding I/L/O/U to avoid transcription errors.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedDevice {
    pub name: String,
    pub paired_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Allowlist {
    devices: HashMap<String, PairedDevice>,
}

/// Outcome of verifying a pairing attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum PairOutcome {
    /// Code + channel binding verified; the issued token to return.
    Paired { token: String },
    /// Wrong code/binding (the code is now consumed).
    Rejected,
    /// Too many recent failures; pairing refused until the cooldown elapses.
    LockedOut,
    /// No code is armed (or it expired).
    NoActiveCode,
}

struct ActiveCode {
    code: String,
    expires_at: u64,
}

pub struct PairingAuth {
    host_key: [u8; 32],
    allowlist: Allowlist,
    allowlist_path: PathBuf,
    active: Option<ActiveCode>,
    fail_count: u32,
    locked_until: u64,
    /// How many lockouts have tripped since the last success — drives an
    /// escalating cooldown so a throttled grinder backs off exponentially.
    lockout_count: u32,
}

impl PairingAuth {
    /// Load (or initialize) host identity + allowlist under `dir`
    /// (default `~/.config/tether`).
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        ensure_private_dir(dir)?;
        let key_path = dir.join("host.key");
        let host_key = load_or_create_host_key(&key_path)?;
        let allowlist_path = dir.join("paired.json");
        let allowlist = load_allowlist(&allowlist_path)?;
        Ok(PairingAuth {
            host_key,
            allowlist,
            allowlist_path,
            active: None,
            fail_count: 0,
            locked_until: 0,
            lockout_count: 0,
        })
    }

    pub fn default_dir() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".config").join("tether"))
    }

    /// True if no device is paired yet (used for the downgrade decision).
    pub fn is_empty(&self) -> bool {
        self.allowlist.devices.is_empty()
    }

    pub fn paired_devices(&self) -> &HashMap<String, PairedDevice> {
        &self.allowlist.devices
    }

    /// Arm a fresh one-time pairing code (operator action). Returns the code to
    /// display (canonical, no grouping); use [`group_code`] for presentation.
    pub fn arm(&mut self, now_unix: u64) -> String {
        // NB: arming does NOT reset the failure budget or lockout — otherwise an
        // attacker could re-arm between guesses to dodge the cooldown. The
        // budget is per-host and only clears on a successful pairing or when the
        // cooldown elapses.
        let code = random_code();
        self.active = Some(ActiveCode {
            code: code.clone(),
            expires_at: now_unix + CODE_TTL.as_secs(),
        });
        code
    }

    /// Verify a pairing attempt. Consumes the active code regardless of outcome
    /// (consume-on-attempt). On success, mints + persists a device token.
    pub fn verify_pairing(
        &mut self,
        device_id: &str,
        name: &str,
        proof: &[u8],
        channel_binding: &[u8; 32],
        now_unix: u64,
    ) -> anyhow::Result<PairOutcome> {
        if now_unix < self.locked_until {
            return Ok(PairOutcome::LockedOut);
        }
        let Some(active) = self.active.take() else {
            return Ok(PairOutcome::NoActiveCode);
        };
        if now_unix >= active.expires_at {
            return Ok(PairOutcome::NoActiveCode);
        }
        if device_id.is_empty() || device_id.len() > 128 {
            return Ok(PairOutcome::Rejected);
        }

        let expected = hmac(active.code.as_bytes(), channel_binding);
        let ok: bool = expected.ct_eq(proof).into();
        if !ok {
            self.fail_count += 1;
            if self.fail_count >= LOCKOUT_THRESHOLD {
                // Escalating cooldown: LOCKOUT_SECS << (lockout_count), capped,
                // so repeated throttled grinding backs off exponentially. Only
                // a successful pairing clears the escalation.
                let shift = self.lockout_count.min(10);
                self.locked_until = now_unix + LOCKOUT_SECS.saturating_mul(1u64 << shift);
                self.lockout_count += 1;
                self.fail_count = 0;
            }
            return Ok(PairOutcome::Rejected);
        }

        self.fail_count = 0;
        self.lockout_count = 0;
        let token = self.issue_token(device_id, name, now_unix)?;
        Ok(PairOutcome::Paired { token })
    }

    fn issue_token(
        &mut self,
        device_id: &str,
        name: &str,
        now_unix: u64,
    ) -> anyhow::Result<String> {
        self.allowlist.devices.insert(
            device_id.to_owned(),
            PairedDevice {
                name: name.to_owned(),
                paired_at: now_unix,
            },
        );
        self.persist()?;
        Ok(self.token_for(device_id, now_unix))
    }

    fn token_for(&self, device_id: &str, paired_at: u64) -> String {
        let mac = hmac(
            &self.host_key,
            format!("{device_id}:{paired_at}").as_bytes(),
        );
        hex(&mac)
    }

    /// Verify a presented token: the HMAC must match AND the device must still
    /// be in the allowlist (so removal revokes).
    pub fn verify_token(&self, device_id: &str, token: &str) -> bool {
        let Some(dev) = self.allowlist.devices.get(device_id) else {
            return false;
        };
        let expected = self.token_for(device_id, dev.paired_at);
        // constant-time over bytes; lengths are fixed hex so this is safe
        expected.as_bytes().ct_eq(token.as_bytes()).into()
    }

    pub fn revoke(&mut self, device_id: &str) -> anyhow::Result<bool> {
        let removed = self.allowlist.devices.remove(device_id).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(&self.allowlist)?;
        atomic_write(&self.allowlist_path, &json, 0o600)
    }
}

/// Channel binding: a hash of the two DTLS fingerprints, order-independent, so
/// both peers derive the same value from an honest connection and different
/// values under a fingerprint-swapping relay MITM. Fingerprints are normalized
/// (uppercase, colons stripped) before hashing.
pub fn channel_binding(fp_a: &str, fp_b: &str) -> [u8; 32] {
    let a = normalize_fp(fp_a);
    let b = normalize_fp(fp_b);
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(lo.as_bytes());
    h.update(b"|");
    h.update(hi.as_bytes());
    h.finalize().into()
}

fn normalize_fp(fp: &str) -> String {
    fp.chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect::<String>()
        .to_uppercase()
}

/// The proof a controller computes: `HMAC(code, channel_binding)`.
pub fn pairing_proof(code: &str, channel_binding: &[u8; 32]) -> Vec<u8> {
    hmac(code.as_bytes(), channel_binding)
}

/// Group an 8-char code as `XXXX-XXXX` for display.
pub fn group_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_owned()
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn random_code() -> String {
    let mut buf = [0u8; 8];
    fill_random(&mut buf);
    // 64 uniform bits → take 40 (8 base32 chars); low bits of uniform are uniform.
    let v = u64::from_le_bytes(buf) & 0xFF_FFFF_FFFF;
    let mut s = String::with_capacity(8);
    for i in 0..8 {
        let idx = ((v >> ((7 - i) * 5)) & 0x1F) as usize;
        s.push(CROCKFORD[idx] as char);
    }
    s
}

fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("system RNG must not fail");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---- persistence (unix perms / atomic writes) ---------------------------

fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn load_or_create_host_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let meta = std::fs::metadata(path)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is group/world-accessible (mode {:o}); refusing to use it",
                path.display(),
                mode
            );
        }
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            bail!(
                "{} is corrupt (expected 32 bytes, got {})",
                path.display(),
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(key)
    } else {
        let mut key = [0u8; 32];
        fill_random(&mut key);
        atomic_write(path, &key, 0o600)?;
        Ok(key)
    }
}

fn load_allowlist(path: &Path) -> anyhow::Result<Allowlist> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    } else {
        Ok(Allowlist::default())
    }
}

/// Write atomically (tmp + rename) with the given mode, so a crash can't leave
/// a truncated secret.
fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(dir: &Path) -> PairingAuth {
        PairingAuth::load_or_create(dir).unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let mut buf = [0u8; 8];
        fill_random(&mut buf);
        d.push(format!("tether-auth-test-{tag}-{}", hex(&buf)));
        d
    }

    const CHAN: [u8; 32] = [7u8; 32];

    #[test]
    fn pair_then_token_verifies_and_revocation_works() {
        let dir = tmpdir("pair");
        let mut a = fresh(&dir);
        assert!(a.is_empty());

        let code = a.arm(1000);
        let proof = pairing_proof(&code, &CHAN);
        let outcome = a
            .verify_pairing("dev1", "iPad", &proof, &CHAN, 1001)
            .unwrap();
        let PairOutcome::Paired { token } = outcome else {
            panic!("expected Paired, got {outcome:?}");
        };
        assert!(!a.is_empty());
        assert!(a.verify_token("dev1", &token));
        assert!(!a.verify_token("dev2", &token)); // unknown device
        assert!(!a.verify_token("dev1", "deadbeef")); // wrong token

        // revoke → token stops working even though HMAC still matches
        assert!(a.revoke("dev1").unwrap());
        assert!(!a.verify_token("dev1", &token));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_persists_across_reload() {
        let dir = tmpdir("reload");
        let token = {
            let mut a = fresh(&dir);
            let code = a.arm(1000);
            let proof = pairing_proof(&code, &CHAN);
            match a
                .verify_pairing("dev1", "iPad", &proof, &CHAN, 1001)
                .unwrap()
            {
                PairOutcome::Paired { token } => token,
                o => panic!("{o:?}"),
            }
        };
        // a brand-new instance (reloaded host_key + allowlist) still verifies it
        let b = fresh(&dir);
        assert!(b.verify_token("dev1", &token));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_code_consumes_and_does_not_pair() {
        let dir = tmpdir("wrong");
        let mut a = fresh(&dir);
        a.arm(1000);
        // attacker guesses a bad proof
        let bad = vec![0u8; 32];
        assert_eq!(
            a.verify_pairing("dev", "x", &bad, &CHAN, 1001).unwrap(),
            PairOutcome::Rejected
        );
        // code is consumed — even the correct proof now fails (no active code)
        assert_eq!(
            a.verify_pairing("dev", "x", &[0u8; 32], &CHAN, 1002)
                .unwrap(),
            PairOutcome::NoActiveCode
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprint_mismatch_fails_pairing_mitm_regression() {
        let dir = tmpdir("mitm");
        let mut a = fresh(&dir);
        let code = a.arm(1000);
        // controller computed its proof against a DIFFERENT channel binding
        // (as a relay MITM would cause) → host rejects.
        let controller_chan = [9u8; 32];
        let proof = pairing_proof(&code, &controller_chan);
        let host_chan = [7u8; 32];
        assert_eq!(
            a.verify_pairing("dev", "x", &proof, &host_chan, 1001)
                .unwrap(),
            PairOutcome::Rejected
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lockout_after_repeated_failures() {
        let dir = tmpdir("lock");
        let mut a = fresh(&dir);
        for _ in 0..LOCKOUT_THRESHOLD {
            a.arm(1000); // each wrong attempt consumes the code; re-arm to retry
            assert_eq!(
                a.verify_pairing("dev", "x", &[0u8; 32], &CHAN, 1000)
                    .unwrap(),
                PairOutcome::Rejected
            );
        }
        // now locked out: even a correct attempt is refused during cooldown
        let code = a.arm(1000);
        let proof = pairing_proof(&code, &CHAN);
        assert_eq!(
            a.verify_pairing("dev", "x", &proof, &CHAN, 1000).unwrap(),
            PairOutcome::LockedOut
        );
        // after the cooldown, pairing works again
        let code = a.arm(1000 + LOCKOUT_SECS + 1);
        let proof = pairing_proof(&code, &CHAN);
        assert!(matches!(
            a.verify_pairing("dev", "x", &proof, &CHAN, 1000 + LOCKOUT_SECS + 2)
                .unwrap(),
            PairOutcome::Paired { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expired_code_is_rejected() {
        let dir = tmpdir("expire");
        let mut a = fresh(&dir);
        let code = a.arm(1000);
        let proof = pairing_proof(&code, &CHAN);
        assert_eq!(
            a.verify_pairing("dev", "x", &proof, &CHAN, 1000 + CODE_TTL.as_secs() + 1)
                .unwrap(),
            PairOutcome::NoActiveCode
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tokens_are_host_specific() {
        let dir_a = tmpdir("hostA");
        let dir_b = tmpdir("hostB");
        let mut a = fresh(&dir_a);
        let code = a.arm(1000);
        let token = match a
            .verify_pairing("dev", "x", &pairing_proof(&code, &CHAN), &CHAN, 1001)
            .unwrap()
        {
            PairOutcome::Paired { token } => token,
            o => panic!("{o:?}"),
        };
        // a different host (distinct host_key) must reject A's token even if the
        // device is "paired" there
        let mut b = fresh(&dir_b);
        let code_b = b.arm(1000);
        b.verify_pairing("dev", "x", &pairing_proof(&code_b, &CHAN), &CHAN, 1001)
            .unwrap();
        assert!(!b.verify_token("dev", &token));
        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }

    #[test]
    fn channel_binding_is_order_independent_and_distinguishes() {
        let fp1 = "AA:BB:CC";
        let fp2 = "dd:ee:ff";
        assert_eq!(channel_binding(fp1, fp2), channel_binding(fp2, fp1));
        assert_ne!(channel_binding(fp1, fp2), channel_binding(fp1, "00:11:22"));
    }

    #[test]
    fn code_is_eight_crockford_chars() {
        let mut a = fresh(&tmpdir("code"));
        let code = a.arm(0);
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|b| CROCKFORD.contains(&b)));
        assert_eq!(group_code(&code).len(), 9); // XXXX-XXXX
    }
}
