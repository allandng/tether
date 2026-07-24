//! Signaling messages: JSON over WebSocket. Low-rate control traffic, so
//! debuggability beats compactness — this is deliberately not the binary
//! media protocol. SDP and ICE payloads are relayed verbatim; the server
//! never interprets them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    pub can_host: bool,
    pub can_control: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub device_id: String,
    pub name: String,
    pub caps: Caps,
}

/// An ICE server entry shaped for a browser `RTCPeerConnection` (STUN entries
/// omit username/credential; TURN entries carry ephemeral coturn creds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub credential: Option<String>,
}

/// Client → server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Must be the first message on a connection, and must answer the
    /// `Challenge` the server sends on connect. `auth` is the pre-shared secret
    /// — a coarse admission gate only.
    ///
    /// `pubkey`/`sig` are the per-device identity (Phase 6): an Ed25519
    /// signature over [`crate::identity::register_payload`]. Mandatory for
    /// hosts and for any `device_id` the server has already pinned; optional
    /// for a controller registering an id nobody has claimed. They are
    /// `Option` so an older peer's registration still *parses* — the server
    /// then refuses it with a specific error rather than a parse failure.
    Register {
        device_id: String,
        name: String,
        caps: Caps,
        auth: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        pubkey: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        sig: Option<String>,
    },
    /// Controller → host. Refused unless the sender can control and the
    /// target is a registered host (the no-mobile-host invariant, enforced
    /// at the directory as well as in the media protocol's Hello).
    Offer { target: String, sdp: String },
    /// Host → controller.
    Answer { target: String, sdp: String },
    /// Trickle ICE, either direction.
    Ice { target: String, candidate: String },
}

/// Server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent once, immediately on connect, before anything else. The client
    /// signs this nonce (bound to its `device_id`) and answers with `Register`.
    /// A per-connection nonce is what stops a captured signature from being
    /// replayed onto a new socket.
    Challenge {
        nonce: String,
    },
    /// Registration accepted; carries the ICE servers (STUN + ephemeral TURN)
    /// the peer should use for its `RTCPeerConnection`.
    Registered {
        ice_servers: Vec<IceServer>,
    },
    /// Full directory snapshot, broadcast on every join/leave.
    Peers {
        peers: Vec<PeerInfo>,
    },
    Offer {
        from: String,
        sdp: String,
    },
    Answer {
        from: String,
        sdp: String,
    },
    Ice {
        from: String,
        candidate: String,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadAuth,
    NotRegistered,
    UnknownTarget,
    TargetNotHost,
    NotController,
    Replaced,
    BadMessage,
    /// A signed registration is required here — this peer is a host, or the
    /// `device_id` is already pinned — and none was presented.
    IdentityRequired,
    /// The presented key or signature is malformed or does not verify.
    BadIdentity,
    /// The `device_id` is pinned to a different key. The squatting refusal:
    /// the incumbent host keeps its registration.
    IdentityMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON shapes pinned against controller/src/signaling.test.ts.
    /// Change both or neither.
    #[test]
    fn cross_implementation_json_vectors() {
        // An identity-less register (a fresh controller, or an older peer) must
        // still parse: the server refuses it with a specific error, and a parse
        // failure would surface as a useless "unparseable message" instead.
        let register: ClientMessage = serde_json::from_str(
            r#"{"type":"register","device_id":"ipad","name":"iPad","caps":{"can_host":false,"can_control":true},"auth":"s3cret"}"#,
        )
        .unwrap();
        match register {
            ClientMessage::Register {
                device_id,
                caps,
                auth,
                pubkey,
                sig,
                ..
            } => {
                assert_eq!(device_id, "ipad");
                assert!(!caps.can_host);
                assert!(caps.can_control);
                assert_eq!(auth, "s3cret");
                assert_eq!(pubkey, None);
                assert_eq!(sig, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let signed: ClientMessage = serde_json::from_str(
            r#"{"type":"register","device_id":"mac","name":"Mac","caps":{"can_host":true,"can_control":true},"auth":"s3cret","pubkey":"aa","sig":"bb"}"#,
        )
        .unwrap();
        match signed {
            ClientMessage::Register { pubkey, sig, .. } => {
                assert_eq!(pubkey.as_deref(), Some("aa"));
                assert_eq!(sig.as_deref(), Some("bb"));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // Identity fields are omitted entirely when absent, so the wire shape a
        // controller without WebCrypto Ed25519 produces is exactly the legacy one.
        assert_eq!(
            serde_json::to_string(&ClientMessage::Register {
                device_id: "ipad".into(),
                name: "iPad".into(),
                caps: Caps {
                    can_host: false,
                    can_control: true
                },
                auth: "s3cret".into(),
                pubkey: None,
                sig: None,
            })
            .unwrap(),
            r#"{"type":"register","device_id":"ipad","name":"iPad","caps":{"can_host":false,"can_control":true},"auth":"s3cret"}"#
        );

        assert_eq!(
            serde_json::to_string(&ServerMessage::Challenge {
                nonce: "deadbeef".into()
            })
            .unwrap(),
            r#"{"type":"challenge","nonce":"deadbeef"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::Error {
                code: ErrorCode::IdentityMismatch,
                message: "pinned".into()
            })
            .unwrap(),
            r#"{"type":"error","code":"identity_mismatch","message":"pinned"}"#
        );

        // STUN-only registered: TURN username/credential omitted
        assert_eq!(
            serde_json::to_string(&ServerMessage::Registered {
                ice_servers: vec![IceServer {
                    urls: vec!["stun:s:3478".into()],
                    username: None,
                    credential: None,
                }],
            })
            .unwrap(),
            r#"{"type":"registered","ice_servers":[{"urls":["stun:s:3478"]}]}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::Answer {
                from: "mac".into(),
                sdp: "v=0...".into()
            })
            .unwrap(),
            r#"{"type":"answer","from":"mac","sdp":"v=0..."}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::Error {
                code: ErrorCode::BadAuth,
                message: "bad secret".into()
            })
            .unwrap(),
            r#"{"type":"error","code":"bad_auth","message":"bad secret"}"#
        );
    }
}
