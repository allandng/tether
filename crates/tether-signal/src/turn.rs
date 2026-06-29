//! Ephemeral TURN credentials for coturn's REST API (`use-auth-secret` /
//! `static-auth-secret`). The signal server mints a short-lived credential per
//! registration so long-lived TURN passwords never live in clients.
//!
//! Format (coturn REST / draft-uberti-behave-turn-rest):
//!   username   = "<absolute_unix_expiry>:<userid>"
//!   credential = base64( HMAC-SHA1(static_secret, username) )   // raw 20 bytes
//!
//! The timestamp is an ABSOLUTE expiry (now + ttl), not a duration — coturn
//! rejects usernames whose timestamp is already in the past.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::protocol::IceServer;

type HmacSha1 = Hmac<Sha1>;

/// Mint a coturn ephemeral credential pair.
pub fn mint_turn_credential(
    static_secret: &str,
    ttl_secs: u64,
    now_unix: u64,
    userid: &str,
) -> (String, String) {
    let expiry = now_unix + ttl_secs;
    let username = format!("{expiry}:{userid}");
    let mut mac = HmacSha1::new_from_slice(static_secret.as_bytes()).expect("HMAC any key len");
    mac.update(username.as_bytes());
    let credential = B64.encode(mac.finalize().into_bytes());
    (username, credential)
}

/// Configuration for what ICE servers to advertise to peers.
#[derive(Clone, Debug)]
pub struct IceConfig {
    pub stun_urls: Vec<String>,
    pub turn_urls: Vec<String>,
    pub turn_secret: Option<String>,
    pub turn_ttl: u64,
}

impl IceConfig {
    /// Build the per-peer ICE server list: STUN entries plus, if a TURN secret
    /// is configured, one TURN entry with a freshly minted credential.
    pub fn ice_servers_for(&self, userid: &str, now_unix: u64) -> Vec<IceServer> {
        let mut servers = Vec::new();
        if !self.stun_urls.is_empty() {
            servers.push(IceServer {
                urls: self.stun_urls.clone(),
                username: None,
                credential: None,
            });
        }
        if let Some(secret) = &self.turn_secret {
            if !self.turn_urls.is_empty() {
                let (username, credential) =
                    mint_turn_credential(secret, self.turn_ttl, now_unix, userid);
                servers.push(IceServer {
                    urls: self.turn_urls.clone(),
                    username: Some(username),
                    credential: Some(credential),
                });
            }
        }
        servers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_format_matches_coturn_rest() {
        // username embeds the absolute expiry; credential is base64 of 20 raw
        // HMAC-SHA1 bytes (28 chars).
        let (username, credential) = mint_turn_credential("north", 3600, 1_000_000, "ipad");
        assert_eq!(username, "1003600:ipad");
        assert_eq!(credential.len(), 28); // base64(20 bytes)
        assert!(credential.ends_with('='));
        // deterministic for the same inputs
        let (_, c2) = mint_turn_credential("north", 3600, 1_000_000, "ipad");
        assert_eq!(credential, c2);
        // a different secret changes it
        let (_, c3) = mint_turn_credential("south", 3600, 1_000_000, "ipad");
        assert_ne!(credential, c3);
    }

    #[test]
    fn stun_only_when_no_turn_secret() {
        let cfg = IceConfig {
            stun_urls: vec!["stun:s:3478".into()],
            turn_urls: vec!["turn:t:3478".into()],
            turn_secret: None,
            turn_ttl: 86400,
        };
        let servers = cfg.ice_servers_for("dev", 0);
        assert_eq!(servers.len(), 1);
        assert!(servers[0].username.is_none());
    }

    #[test]
    fn turn_entry_minted_when_configured() {
        let cfg = IceConfig {
            stun_urls: vec!["stun:s:3478".into()],
            turn_urls: vec!["turn:t:3478?transport=udp".into(), "turns:t:5349?transport=tcp".into()],
            turn_secret: Some("sekret".into()),
            turn_ttl: 600,
        };
        let servers = cfg.ice_servers_for("dev", 100);
        assert_eq!(servers.len(), 2);
        let turn = &servers[1];
        assert_eq!(turn.urls.len(), 2);
        assert_eq!(turn.username.as_deref(), Some("700:dev"));
        assert!(turn.credential.is_some());
    }
}
