//! Integration tests: a real server on loopback, a real tokio-tungstenite
//! client. The allowlist *rejection* path can't be exercised over loopback
//! (can't forge a source IP), so that logic is unit-tested in config.rs and
//! these tests run with 127.0.0.1 allowed.

use std::net::SocketAddr;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tether_protocol::{
    Auth, AuthResult, CAP_CAN_CONTROL, CAP_CAN_HOST, ClipboardData, Codec, Decoded, Hello,
    InputEvent, Message, MouseButton, PROTOCOL_VERSION, PairRequest, PairResult, Resolution, Role,
    TextInput,
};
use tetherd::auth::{PairingAuth, pairing_proof};
use tetherd::capture::EncodedFrame;
use tetherd::input::InjectCommand;
use tetherd::server::{AuthPolicy, Server, ServerState};
use tetherd::session::ws_channel_binding;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;

struct TestHost {
    addr: SocketAddr,
    frames_tx: watch::Sender<Option<EncodedFrame>>,
    #[allow(dead_code)]
    resolution_tx: watch::Sender<Resolution>,
    input_rx: mpsc::Receiver<InjectCommand>,
    clipboard_out_tx: watch::Sender<Option<String>>,
    clipboard_in_rx: std::sync::mpsc::Receiver<String>,
    #[allow(dead_code)]
    displays_tx: watch::Sender<Vec<tether_protocol::DisplayInfo>>,
    select_display_rx: std::sync::mpsc::Receiver<u32>,
    auth: std::sync::Arc<tokio::sync::Mutex<PairingAuth>>,
    server_task: tokio::task::JoinHandle<()>,
}

impl Drop for TestHost {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

async fn start_host() -> TestHost {
    // gate off by default (allow_unpaired) — pairing has its own test
    start_host_with(AuthPolicy {
        require_pairing: false,
        allow_unpaired: true,
    })
    .await
}

fn temp_auth_dir() -> std::path::PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("tether-conn-test-{}", std::process::id()));
    d.push(format!("{:?}", std::time::Instant::now()).replace([' ', '.', '{', '}'], "_"));
    d
}

async fn start_host_with(policy: AuthPolicy) -> TestHost {
    start_host_slots(policy, 1).await
}

async fn start_host_slots(policy: AuthPolicy, slots: usize) -> TestHost {
    let (resolution_tx, resolution_rx) = watch::channel(Resolution {
        width: 1920,
        height: 1080,
    });
    let (frames_tx, frames_rx) = watch::channel(None);
    let (input_tx, input_rx) = mpsc::channel(64);
    let (clipboard_out_tx, clipboard_out_rx) = watch::channel(None);
    let (clipboard_in_tx, clipboard_in_rx) = std::sync::mpsc::channel();
    let (displays_tx, displays_rx) = watch::channel(Vec::new());
    let (select_display_tx, select_display_rx) = std::sync::mpsc::channel();
    let auth = std::sync::Arc::new(tokio::sync::Mutex::new(
        PairingAuth::load_or_create(&temp_auth_dir()).expect("auth"),
    ));
    let server = Server::bind(
        "127.0.0.1".parse().unwrap(),
        0, // OS-assigned port
        vec!["127.0.0.1".parse().unwrap()],
        ServerState {
            resolution: resolution_rx,
            frames: frames_rx,
            input_tx,
            clipboard_out: clipboard_out_rx,
            clipboard_in: clipboard_in_tx,
            auth: auth.clone(),
            auth_policy: policy,
            bitrate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            bitrate_ceiling_kbps: 4000,
            controller_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(slots)),
            displays: displays_rx,
            select_display: select_display_tx,
        },
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local addr");
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });
    TestHost {
        addr,
        frames_tx,
        resolution_tx,
        input_rx,
        clipboard_out_tx,
        clipboard_in_rx,
        displays_tx,
        select_display_rx,
        auth,
        server_task,
    }
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: SocketAddr) -> Client {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");
    ws
}

fn controller_hello() -> WsMessage {
    WsMessage::Binary(
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            role: Role::Controller,
            capabilities: CAP_CAN_CONTROL,
        })
        .encode(),
    )
}

async fn next_message(ws: &mut Client) -> Option<Message> {
    loop {
        match ws.next().await? {
            Ok(WsMessage::Binary(bytes)) => match Message::decode(&bytes).expect("decode") {
                Decoded::Message { message, .. } => return Some(message),
                other => panic!("unexpected decode outcome: {other:?}"),
            },
            Ok(WsMessage::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn send_msg(ws: &mut Client, m: &Message) {
    ws.send(WsMessage::Binary(m.encode())).await.expect("send");
}

async fn handshake(ws: &mut Client) {
    ws.send(controller_hello()).await.expect("send hello");
    let Some(Message::Hello(h)) = next_message(ws).await else {
        panic!("expected host Hello");
    };
    assert_eq!(h.version, PROTOCOL_VERSION);
    assert_eq!(h.role, Role::Host);
    assert_ne!(h.capabilities & CAP_CAN_HOST, 0);
    // auth gate: controller always sends Auth after Hello (token empty when the
    // gate is off, as in these tests) and gets AuthResult{ok:true}.
    send_msg(
        ws,
        &Message::Auth(Auth {
            device_id: "test".into(),
            token: String::new(),
        }),
    )
    .await;
    let Some(Message::AuthResult(ar)) = next_message(ws).await else {
        panic!("expected AuthResult after Auth");
    };
    assert!(ar.ok);
    let Some(Message::Resolution(r)) = next_message(ws).await else {
        panic!("expected Resolution after auth");
    };
    assert_eq!((r.width, r.height), (1920, 1080));
}

#[tokio::test]
async fn handshake_then_frames_then_input() {
    let mut host = start_host().await;
    let mut ws = connect(host.addr).await;
    handshake(&mut ws).await;

    // host publishes a frame -> controller receives FrameData
    host.frames_tx
        .send(Some(EncodedFrame {
            codec: Codec::Jpeg,
            seq: 1,
            timestamp_micros: 123,
            payload: Bytes::from_static(b"fakejpeg"),
        }))
        .unwrap();
    let Some(Message::FrameData(f)) = next_message(&mut ws).await else {
        panic!("expected FrameData");
    };
    assert_eq!(f.seq, 1);
    assert_eq!(&f.payload[..], b"fakejpeg");

    // controller sends input -> host receives it on the injector channel
    let ev = InputEvent::MouseDown {
        button: MouseButton::Left,
        x: 100,
        y: 200,
    };
    ws.send(WsMessage::Binary(Message::InputEvent(ev.clone()).encode()))
        .await
        .unwrap();
    let received = host.input_rx.recv().await.expect("input event");
    assert!(matches!(received, InjectCommand::Event(e) if e == ev));
}

/// Full pairing lifecycle over WS: gate refuses an unpaired controller; a
/// one-time code pairs it; the issued token authenticates a reconnect;
/// revocation locks it back out.
#[tokio::test]
async fn pairing_lifecycle() {
    let host = start_host_with(AuthPolicy {
        require_pairing: true,
        allow_unpaired: false,
    })
    .await;

    // 1. connect; Hello exchange; an empty-token Auth is refused (gate active)
    let mut ws = connect(host.addr).await;
    ws.send(controller_hello()).await.unwrap();
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Hello(_))
    ));
    send_msg(
        &mut ws,
        &Message::Auth(Auth {
            device_id: "ipad".into(),
            token: String::new(),
        }),
    )
    .await;
    assert_eq!(
        next_message(&mut ws).await,
        Some(Message::AuthResult(AuthResult { ok: false }))
    );

    // 2. arm a code on the host and pair with a channel-bound proof
    let code = host.auth.lock().await.arm(tetherd::auth::now_unix());
    let proof = pairing_proof(&code, &ws_channel_binding());
    send_msg(
        &mut ws,
        &Message::PairRequest(PairRequest {
            device_id: "ipad".into(),
            name: "iPad".into(),
            proof,
        }),
    )
    .await;
    let token = match next_message(&mut ws).await {
        Some(Message::PairResult(PairResult { ok: true, token })) => token,
        other => panic!("expected PairResult ok, got {other:?}"),
    };
    // pairing authenticates this session → Resolution follows
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Resolution(_))
    ));
    drop(ws);

    // 3. reconnect with the token → authenticated, no code needed
    let mut ws = connect(host.addr).await;
    ws.send(controller_hello()).await.unwrap();
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Hello(_))
    ));
    send_msg(
        &mut ws,
        &Message::Auth(Auth {
            device_id: "ipad".into(),
            token: token.clone(),
        }),
    )
    .await;
    assert_eq!(
        next_message(&mut ws).await,
        Some(Message::AuthResult(AuthResult { ok: true }))
    );
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Resolution(_))
    ));
    drop(ws);

    // 4. revoke → the same token is rejected
    assert!(host.auth.lock().await.revoke("ipad").unwrap());
    let mut ws = connect(host.addr).await;
    ws.send(controller_hello()).await.unwrap();
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Hello(_))
    ));
    send_msg(
        &mut ws,
        &Message::Auth(Auth {
            device_id: "ipad".into(),
            token,
        }),
    )
    .await;
    assert_eq!(
        next_message(&mut ws).await,
        Some(Message::AuthResult(AuthResult { ok: false }))
    );
}

/// A wrong pairing code is consumed on the first attempt (no brute force).
#[tokio::test]
async fn wrong_pairing_code_is_single_use() {
    let host = start_host_with(AuthPolicy {
        require_pairing: true,
        allow_unpaired: false,
    })
    .await;
    host.auth.lock().await.arm(tetherd::auth::now_unix());

    let mut ws = connect(host.addr).await;
    ws.send(controller_hello()).await.unwrap();
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Hello(_))
    ));
    // a bad proof
    send_msg(
        &mut ws,
        &Message::PairRequest(PairRequest {
            device_id: "x".into(),
            name: "x".into(),
            proof: vec![0u8; 32],
        }),
    )
    .await;
    assert_eq!(
        next_message(&mut ws).await,
        Some(Message::PairResult(PairResult {
            ok: false,
            token: String::new()
        })),
    );
    // host closes the session after a failed pairing
    assert_eq!(next_message(&mut ws).await, None);
}

#[tokio::test]
async fn text_input_relays_to_injector() {
    let mut host = start_host().await;
    let mut ws = connect(host.addr).await;
    handshake(&mut ws).await;

    ws.send(WsMessage::Binary(
        Message::TextInput(TextInput {
            text: "señor 🎯".into(),
        })
        .encode(),
    ))
    .await
    .unwrap();
    let received = host.input_rx.recv().await.expect("text input");
    assert!(matches!(received, InjectCommand::Text(t) if t == "señor 🎯"));
}

#[tokio::test]
async fn clipboard_relays_both_directions() {
    let host = start_host().await;
    let mut ws = connect(host.addr).await;
    handshake(&mut ws).await;

    // host -> controller
    host.clipboard_out_tx
        .send(Some("copied on host".into()))
        .unwrap();
    match next_message(&mut ws).await {
        Some(Message::ClipboardData(c)) => assert_eq!(c.text, "copied on host"),
        other => panic!("expected ClipboardData, got {other:?}"),
    }

    // controller -> host
    ws.send(WsMessage::Binary(
        Message::ClipboardData(ClipboardData {
            text: "copied on controller".into(),
        })
        .encode(),
    ))
    .await
    .unwrap();
    let received = tokio::task::spawn_blocking(move || {
        host.clipboard_in_rx
            .recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .expect("clipboard not relayed to host");
    assert_eq!(received, "copied on controller");
}

#[tokio::test]
async fn clean_disconnect_then_reconnect_without_restart() {
    let host = start_host().await;

    let mut ws = connect(host.addr).await;
    handshake(&mut ws).await;
    ws.close(None).await.expect("close");
    drop(ws);

    // Session teardown is async; poll until the slot frees up.
    let mut reconnected = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut ws2 = connect(host.addr).await;
        ws2.send(controller_hello()).await.expect("send hello");
        if let Some(Message::Hello(_)) = next_message(&mut ws2).await {
            reconnected = true;
            break;
        }
    }
    assert!(
        reconnected,
        "server must accept a new session after disconnect"
    );
}

#[tokio::test]
async fn second_concurrent_connection_is_rejected() {
    let host = start_host().await;
    let mut ws1 = connect(host.addr).await;
    handshake(&mut ws1).await;

    // Second connection while the first is active: dropped before/at upgrade.
    let attempt = tokio_tungstenite::connect_async(format!("ws://{}", host.addr)).await;
    let rejected = match attempt {
        Err(_) => true,
        Ok((mut ws2, _)) => {
            ws2.send(controller_hello()).await.ok();
            next_message(&mut ws2).await.is_none()
        }
    };
    assert!(rejected, "second concurrent session must be rejected");

    // ...and the first session is unaffected.
    ws1.send(WsMessage::Binary(
        Message::InputEvent(InputEvent::MouseMove { x: 1, y: 1 }).encode(),
    ))
    .await
    .expect("first session still usable");
}

#[tokio::test]
async fn select_display_routes_to_capture() {
    let host = start_host().await;
    let mut ws = connect(host.addr).await;
    handshake(&mut ws).await;

    // host announces a multi-display set → controller can pick
    host.displays_tx
        .send(vec![
            tether_protocol::DisplayInfo {
                id: 1,
                width: 1920,
                height: 1080,
                active: true,
                name: "1".into(),
            },
            tether_protocol::DisplayInfo {
                id: 7,
                width: 2560,
                height: 1440,
                active: false,
                name: "2".into(),
            },
        ])
        .unwrap();
    assert!(matches!(
        next_message(&mut ws).await,
        Some(Message::Displays(_))
    ));

    // controller selects display 7 → host routes it to the capture thread
    send_msg(
        &mut ws,
        &Message::SelectDisplay(tether_protocol::SelectDisplay { id: 7 }),
    )
    .await;
    let routed = tokio::task::spawn_blocking(move || {
        host.select_display_rx
            .recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .expect("SelectDisplay not routed to capture");
    assert_eq!(routed, 7);
}

#[tokio::test]
async fn multiple_controllers_up_to_the_cap() {
    // --max-controllers 2: two connect concurrently, a third is refused.
    let mut host = start_host_slots(
        AuthPolicy {
            require_pairing: false,
            allow_unpaired: true,
        },
        2,
    )
    .await;
    let mut a = connect(host.addr).await;
    handshake(&mut a).await;
    let mut b = connect(host.addr).await;
    handshake(&mut b).await;

    // a frame fans out to BOTH controllers
    host.frames_tx
        .send(Some(EncodedFrame {
            codec: Codec::Jpeg,
            seq: 1,
            timestamp_micros: 1,
            payload: Bytes::from_static(b"f"),
        }))
        .unwrap();
    for ws in [&mut a, &mut b] {
        assert!(matches!(
            next_message(ws).await,
            Some(Message::FrameData(_))
        ));
    }

    // a third over the cap is refused — the server drops the socket before (or
    // at) the upgrade, so either connect fails or the stream closes silently.
    let refused = match tokio_tungstenite::connect_async(format!("ws://{}", host.addr)).await {
        Err(_) => true,
        Ok((mut c, _)) => {
            c.send(controller_hello()).await.ok();
            next_message(&mut c).await.is_none()
        }
    };
    assert!(refused, "third controller must be refused");

    // input from EITHER controller reaches the injector (serialized)
    a.send(WsMessage::Binary(
        Message::InputEvent(InputEvent::MouseMove { x: 5, y: 6 }).encode(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        host.input_rx.recv().await,
        Some(InjectCommand::Event(_))
    ));

    // freeing a slot lets a new controller in
    drop(a);
    let mut admitted = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let Ok((mut ws, _)) = tokio_tungstenite::connect_async(format!("ws://{}", host.addr)).await
        else {
            continue;
        };
        ws.send(controller_hello()).await.ok();
        if matches!(next_message(&mut ws).await, Some(Message::Hello(_))) {
            admitted = true;
            break;
        }
    }
    assert!(admitted, "a freed slot must admit a new controller");
}

#[tokio::test]
async fn version_mismatch_is_rejected() {
    let host = start_host().await;
    let mut ws = connect(host.addr).await;
    ws.send(WsMessage::Binary(
        Message::Hello(Hello {
            version: PROTOCOL_VERSION + 1,
            role: Role::Controller,
            capabilities: CAP_CAN_CONTROL,
        })
        .encode(),
    ))
    .await
    .unwrap();
    assert!(
        next_message(&mut ws).await.is_none(),
        "host must close instead of answering a bad version"
    );
}

#[tokio::test]
async fn host_role_peer_is_rejected() {
    let host = start_host().await;
    let mut ws = connect(host.addr).await;
    ws.send(WsMessage::Binary(
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            role: Role::Host,
            capabilities: CAP_CAN_HOST,
        })
        .encode(),
    ))
    .await
    .unwrap();
    assert!(
        next_message(&mut ws).await.is_none(),
        "host must not accept another host as a peer"
    );
}
