//! Integration tests: real axum server on an ephemeral port, real WebSocket
//! clients registering and exchanging SDP/ICE through the relay.

use std::net::SocketAddr;

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tether_signal::identity::register_payload;
use tether_signal::protocol::{Caps, ClientMessage, ErrorCode, ServerMessage};
use tether_signal::server::{self, AppState};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const SECRET: &str = "test-secret";

const HOST_CAPS: Caps = Caps {
    can_host: true,
    can_control: true,
};
const CONTROLLER_CAPS: Caps = Caps {
    can_host: false,
    can_control: true,
};

async fn start_server() -> SocketAddr {
    let state = AppState::new(SECRET.into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, server::router(state)).await;
    });
    addr
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A deterministic device identity key. Two peers built from the same seed are
/// "the same device"; different seeds are the squatting case.
fn device_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// The `(pubkey, sig)` pair a device presents for this connection's nonce.
fn identity(key: &SigningKey, nonce: &str, device_id: &str) -> (String, String) {
    (
        hex::encode(key.verifying_key().to_bytes()),
        hex::encode(key.sign(&register_payload(nonce, device_id)).to_bytes()),
    )
}

/// Connect and consume the `Challenge` the server issues on every socket,
/// returning the nonce a registration on this connection must sign.
async fn connect(addr: SocketAddr) -> (Client, String) {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");
    match recv(&mut ws).await {
        Some(ServerMessage::Challenge { nonce }) => (ws, nonce),
        other => panic!("expected a Challenge first, got {other:?}"),
    }
}

async fn send(ws: &mut Client, msg: &ClientMessage) {
    ws.send(WsMessage::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .expect("send");
}

async fn recv(ws: &mut Client) -> Option<ServerMessage> {
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for server message")?
        {
            Ok(WsMessage::Text(text)) => {
                return Some(serde_json::from_str(&text).expect("valid ServerMessage"));
            }
            Ok(WsMessage::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// Receive until a non-Peers message arrives (presence broadcasts interleave
/// with directed messages and tests usually care about the latter).
async fn recv_directed(ws: &mut Client) -> Option<ServerMessage> {
    loop {
        match recv(ws).await? {
            ServerMessage::Peers { .. } => continue,
            other => return Some(other),
        }
    }
}

/// Register, signing with `key` when one is given. Hosts must always sign;
/// controllers may register bare until their id gets pinned.
async fn register_as(
    ws: &mut Client,
    nonce: &str,
    device_id: &str,
    caps: Caps,
    key: Option<&SigningKey>,
) {
    let (pubkey, sig) = match key {
        Some(k) => {
            let (p, s) = identity(k, nonce, device_id);
            (Some(p), Some(s))
        }
        None => (None, None),
    };
    send(
        ws,
        &ClientMessage::Register {
            device_id: device_id.into(),
            name: device_id.into(),
            caps,
            auth: SECRET.into(),
            pubkey,
            sig,
        },
    )
    .await;
}

/// Register and assert it was accepted. Hosts sign with a per-device-id key so
/// the common path exercises the identity check rather than skipping it.
async fn register(ws: &mut Client, nonce: &str, device_id: &str, caps: Caps) {
    let key = caps.can_host.then(|| device_key(device_id.as_bytes()[0]));
    register_as(ws, nonce, device_id, caps, key.as_ref()).await;
    match recv(ws).await {
        Some(ServerMessage::Registered { .. }) => {}
        other => panic!("expected Registered, got {other:?}"),
    }
}

#[tokio::test]
async fn offer_answer_ice_relay_round_trip() {
    let addr = start_server().await;
    let (mut host, host_nonce) = connect(addr).await;
    let (mut controller, ctl_nonce) = connect(addr).await;
    register(&mut host, &host_nonce, "mac", HOST_CAPS).await;
    register(&mut controller, &ctl_nonce, "ipad", CONTROLLER_CAPS).await;

    send(
        &mut controller,
        &ClientMessage::Offer {
            target: "mac".into(),
            sdp: "OFFER-SDP".into(),
        },
    )
    .await;
    match recv_directed(&mut host).await {
        Some(ServerMessage::Offer { from, sdp }) => {
            assert_eq!(from, "ipad");
            assert_eq!(sdp, "OFFER-SDP");
        }
        other => panic!("expected relayed offer, got {other:?}"),
    }

    send(
        &mut host,
        &ClientMessage::Answer {
            target: "ipad".into(),
            sdp: "ANSWER-SDP".into(),
        },
    )
    .await;
    match recv_directed(&mut controller).await {
        Some(ServerMessage::Answer { from, sdp }) => {
            assert_eq!(from, "mac");
            assert_eq!(sdp, "ANSWER-SDP");
        }
        other => panic!("expected relayed answer, got {other:?}"),
    }

    send(
        &mut controller,
        &ClientMessage::Ice {
            target: "mac".into(),
            candidate: "CAND-1".into(),
        },
    )
    .await;
    match recv_directed(&mut host).await {
        Some(ServerMessage::Ice { from, candidate }) => {
            assert_eq!((from.as_str(), candidate.as_str()), ("ipad", "CAND-1"));
        }
        other => panic!("expected relayed ice, got {other:?}"),
    }
}

#[tokio::test]
async fn presence_lists_both_devices() {
    let addr = start_server().await;
    let (mut host, host_nonce) = connect(addr).await;
    register(&mut host, &host_nonce, "mac", HOST_CAPS).await;
    let (mut controller, ctl_nonce) = connect(addr).await;
    register(&mut controller, &ctl_nonce, "ipad", CONTROLLER_CAPS).await;

    // host receives a Peers broadcast that includes the newly joined controller
    let mut saw_both = false;
    for _ in 0..3 {
        if let Some(ServerMessage::Peers { peers }) = recv(&mut host).await {
            let ids: Vec<_> = peers.iter().map(|p| p.device_id.as_str()).collect();
            if ids.contains(&"mac") && ids.contains(&"ipad") {
                let ipad = peers.iter().find(|p| p.device_id == "ipad").unwrap();
                assert!(!ipad.caps.can_host, "caps must travel with presence");
                saw_both = true;
                break;
            }
        }
    }
    assert!(
        saw_both,
        "host never saw a directory containing both devices"
    );
}

#[tokio::test]
async fn bad_secret_is_refused() {
    let addr = start_server().await;
    let (mut ws, _nonce) = connect(addr).await;
    send(
        &mut ws,
        &ClientMessage::Register {
            device_id: "intruder".into(),
            name: "intruder".into(),
            caps: CONTROLLER_CAPS,
            auth: "wrong".into(),
            pubkey: None,
            sig: None,
        },
    )
    .await;
    match recv(&mut ws).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::BadAuth),
        None => {} // closed without reply is also a refusal
        other => panic!("expected BadAuth, got {other:?}"),
    }
    // connection must be closed after the refusal
    assert!(
        recv(&mut ws).await.is_none(),
        "server must close after bad auth"
    );
}

#[tokio::test]
async fn offer_to_non_host_is_refused() {
    let addr = start_server().await;
    let (mut a, a_nonce) = connect(addr).await;
    let (mut b, b_nonce) = connect(addr).await;
    register(&mut a, &a_nonce, "phone-a", CONTROLLER_CAPS).await;
    register(&mut b, &b_nonce, "phone-b", CONTROLLER_CAPS).await;

    send(
        &mut a,
        &ClientMessage::Offer {
            target: "phone-b".into(),
            sdp: "X".into(),
        },
    )
    .await;
    match recv_directed(&mut a).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::TargetNotHost),
        other => panic!("expected TargetNotHost, got {other:?}"),
    }
}

#[tokio::test]
async fn offer_to_offline_target_is_refused() {
    let addr = start_server().await;
    let (mut controller, ctl_nonce) = connect(addr).await;
    register(&mut controller, &ctl_nonce, "ipad", CONTROLLER_CAPS).await;
    send(
        &mut controller,
        &ClientMessage::Offer {
            target: "ghost".into(),
            sdp: "X".into(),
        },
    )
    .await;
    match recv_directed(&mut controller).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::UnknownTarget),
        other => panic!("expected UnknownTarget, got {other:?}"),
    }
}

#[tokio::test]
async fn message_before_register_is_refused() {
    let addr = start_server().await;
    let (mut ws, _nonce) = connect(addr).await;
    send(
        &mut ws,
        &ClientMessage::Ice {
            target: "mac".into(),
            candidate: "X".into(),
        },
    )
    .await;
    match recv(&mut ws).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::NotRegistered),
        other => panic!("expected NotRegistered, got {other:?}"),
    }
}

#[tokio::test]
async fn reregistration_replaces_the_stale_connection() {
    let addr = start_server().await;
    let (mut old, old_nonce) = connect(addr).await;
    register(&mut old, &old_nonce, "mac", HOST_CAPS).await;

    // Same device id, same identity key: a legitimate host reconnect must still
    // displace its own stale socket. Only a *different* key is refused.
    let (mut new, new_nonce) = connect(addr).await;
    register(&mut new, &new_nonce, "mac", HOST_CAPS).await;

    // the old connection is told it was replaced and then closed
    let mut replaced = false;
    for _ in 0..3 {
        match recv(&mut old).await {
            Some(ServerMessage::Error {
                code: ErrorCode::Replaced,
                ..
            }) => {
                replaced = true;
                break;
            }
            Some(_) => continue,
            None => {
                replaced = true; // closed = effectively replaced
                break;
            }
        }
    }
    assert!(replaced, "old connection never learned it was replaced");

    // offers now route to the new connection
    let (mut controller, ctl_nonce) = connect(addr).await;
    register(&mut controller, &ctl_nonce, "ipad", CONTROLLER_CAPS).await;
    send(
        &mut controller,
        &ClientMessage::Offer {
            target: "mac".into(),
            sdp: "S".into(),
        },
    )
    .await;
    match recv_directed(&mut new).await {
        Some(ServerMessage::Offer { from, .. }) => assert_eq!(from, "ipad"),
        other => panic!("expected offer on the new connection, got {other:?}"),
    }
}

/// The device_id squatting hole this phase closes: before identity pinning, any
/// holder of the shared secret could register a live host's id and evict it.
#[tokio::test]
async fn squatter_cannot_evict_a_registered_host() {
    let addr = start_server().await;
    let (mut host, host_nonce) = connect(addr).await;
    register(&mut host, &host_nonce, "mac", HOST_CAPS).await;

    // A peer past the shared secret, but holding a different identity key.
    let (mut squatter, squat_nonce) = connect(addr).await;
    register_as(
        &mut squatter,
        &squat_nonce,
        "mac",
        HOST_CAPS,
        Some(&device_key(0xEE)),
    )
    .await;
    match recv(&mut squatter).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::IdentityMismatch),
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }

    // The incumbent is untouched: no Replaced, and offers still reach it.
    let (mut controller, ctl_nonce) = connect(addr).await;
    register(&mut controller, &ctl_nonce, "ipad", CONTROLLER_CAPS).await;
    send(
        &mut controller,
        &ClientMessage::Offer {
            target: "mac".into(),
            sdp: "STILL-MINE".into(),
        },
    )
    .await;
    match recv_directed(&mut host).await {
        Some(ServerMessage::Offer { sdp, .. }) => assert_eq!(sdp, "STILL-MINE"),
        other => panic!("incumbent host lost its registration: {other:?}"),
    }
}

/// Dropping the identity fields must not be a way around a pin, or the whole
/// mechanism is opt-out.
#[tokio::test]
async fn squatter_cannot_bypass_a_pin_by_omitting_the_identity() {
    let addr = start_server().await;
    let (mut host, host_nonce) = connect(addr).await;
    register(&mut host, &host_nonce, "mac", HOST_CAPS).await;

    let (mut squatter, squat_nonce) = connect(addr).await;
    register_as(&mut squatter, &squat_nonce, "mac", HOST_CAPS, None).await;
    match recv(&mut squatter).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::IdentityRequired),
        other => panic!("expected IdentityRequired, got {other:?}"),
    }
}

/// A signature is bound to the connection's nonce, so capturing one off the
/// wire and replaying it on a fresh socket gets you nothing.
#[tokio::test]
async fn a_captured_signature_does_not_replay_onto_a_new_connection() {
    let addr = start_server().await;
    let key = device_key(b'm');
    let (mut host, host_nonce) = connect(addr).await;
    register_as(&mut host, &host_nonce, "mac", HOST_CAPS, Some(&key)).await;
    match recv(&mut host).await {
        Some(ServerMessage::Registered { .. }) => {}
        other => panic!("expected Registered, got {other:?}"),
    }

    // Replay the first connection's (pubkey, sig) on a second socket, which has
    // its own nonce.
    let (pubkey, sig) = identity(&key, &host_nonce, "mac");
    let (mut replay, _fresh_nonce) = connect(addr).await;
    send(
        &mut replay,
        &ClientMessage::Register {
            device_id: "mac".into(),
            name: "mac".into(),
            caps: HOST_CAPS,
            auth: SECRET.into(),
            pubkey: Some(pubkey),
            sig: Some(sig),
        },
    )
    .await;
    match recv(&mut replay).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::BadIdentity),
        other => panic!("expected BadIdentity, got {other:?}"),
    }
}

/// Hosts must always be identified; a fresh controller id need not be.
#[tokio::test]
async fn unsigned_host_is_refused_but_unsigned_controller_is_allowed() {
    let addr = start_server().await;
    let (mut bare_host, nonce) = connect(addr).await;
    register_as(&mut bare_host, &nonce, "mac", HOST_CAPS, None).await;
    match recv(&mut bare_host).await {
        Some(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::IdentityRequired),
        other => panic!("expected IdentityRequired, got {other:?}"),
    }

    let (mut bare_ctl, ctl_nonce) = connect(addr).await;
    register_as(&mut bare_ctl, &ctl_nonce, "ipad", CONTROLLER_CAPS, None).await;
    match recv(&mut bare_ctl).await {
        Some(ServerMessage::Registered { .. }) => {}
        other => panic!("a fresh unpinned controller should register, got {other:?}"),
    }
}
