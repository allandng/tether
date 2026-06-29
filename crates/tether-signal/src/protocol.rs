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
    /// Must be the first message on a connection. `auth` is the pre-shared
    /// secret (Phase 2 auth floor; real pairing UX is deferred).
    Register {
        device_id: String,
        name: String,
        caps: Caps,
        auth: String,
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
    /// Registration accepted; carries the ICE servers (STUN + ephemeral TURN)
    /// the peer should use for its `RTCPeerConnection`.
    Registered { ice_servers: Vec<IceServer> },
    /// Full directory snapshot, broadcast on every join/leave.
    Peers { peers: Vec<PeerInfo> },
    Offer { from: String, sdp: String },
    Answer { from: String, sdp: String },
    Ice { from: String, candidate: String },
    Error { code: ErrorCode, message: String },
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON shapes pinned against controller/src/signaling.test.ts.
    /// Change both or neither.
    #[test]
    fn cross_implementation_json_vectors() {
        let register: ClientMessage = serde_json::from_str(
            r#"{"type":"register","device_id":"ipad","name":"iPad","caps":{"can_host":false,"can_control":true},"auth":"s3cret"}"#,
        )
        .unwrap();
        match register {
            ClientMessage::Register { device_id, caps, auth, .. } => {
                assert_eq!(device_id, "ipad");
                assert!(!caps.can_host);
                assert!(caps.can_control);
                assert_eq!(auth, "s3cret");
            }
            other => panic!("wrong variant: {other:?}"),
        }

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
