//! Integration tests: real axum server on an ephemeral port, real WebSocket
//! clients registering and exchanging SDP/ICE through the relay.

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
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

async fn connect(addr: SocketAddr) -> Client {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");
    ws
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

async fn register(ws: &mut Client, device_id: &str, caps: Caps) {
    send(
        ws,
        &ClientMessage::Register {
            device_id: device_id.into(),
            name: device_id.into(),
            caps,
            auth: SECRET.into(),
        },
    )
    .await;
    match recv(ws).await {
        Some(ServerMessage::Registered { .. }) => {}
        other => panic!("expected Registered, got {other:?}"),
    }
}

#[tokio::test]
async fn offer_answer_ice_relay_round_trip() {
    let addr = start_server().await;
    let mut host = connect(addr).await;
    let mut controller = connect(addr).await;
    register(&mut host, "mac", HOST_CAPS).await;
    register(&mut controller, "ipad", CONTROLLER_CAPS).await;

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
    let mut host = connect(addr).await;
    register(&mut host, "mac", HOST_CAPS).await;
    let mut controller = connect(addr).await;
    register(&mut controller, "ipad", CONTROLLER_CAPS).await;

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
    let mut ws = connect(addr).await;
    send(
        &mut ws,
        &ClientMessage::Register {
            device_id: "intruder".into(),
            name: "intruder".into(),
            caps: CONTROLLER_CAPS,
            auth: "wrong".into(),
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
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    register(&mut a, "phone-a", CONTROLLER_CAPS).await;
    register(&mut b, "phone-b", CONTROLLER_CAPS).await;

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
    let mut controller = connect(addr).await;
    register(&mut controller, "ipad", CONTROLLER_CAPS).await;
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
    let mut ws = connect(addr).await;
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
    let mut old = connect(addr).await;
    register(&mut old, "mac", HOST_CAPS).await;

    let mut new = connect(addr).await;
    register(&mut new, "mac", HOST_CAPS).await;

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
    let mut controller = connect(addr).await;
    register(&mut controller, "ipad", CONTROLLER_CAPS).await;
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
