use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::protocol::{Caps, ClientMessage, ErrorCode, PeerInfo, ServerMessage};

/// A socket that never sends a valid `Register` is closed after this — keeps
/// unauthenticated/idle sockets from accumulating.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on concurrent connections (a leaked secret shouldn't let one peer
/// exhaust the relay's sockets/memory).
const MAX_CONNECTIONS: usize = 1024;
/// Per-connection inbound message budget per second (generous enough for an ICE
/// trickle burst; low enough to stop a flood).
const MAX_MSGS_PER_SEC: u32 = 200;
/// Bounded outbound queue per connection — a stalled reader can't grow server
/// memory without limit (messages are dropped once it's this far behind).
const OUTBOUND_BOUND: usize = 256;

pub struct AppState {
    secret: String,
    ice: crate::turn::IceConfig,
    devices: Mutex<HashMap<String, Device>>,
    conn_counter: AtomicU64,
    conns: AtomicUsize,
}

struct Device {
    name: String,
    caps: Caps,
    conn_id: u64,
    tx: mpsc::Sender<ServerMessage>,
}

/// Decrements the live-connection count on drop, however `handle_socket` exits.
struct ConnGuard(Arc<AppState>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Ordering::Relaxed);
    }
}

impl AppState {
    /// Convenience for tests: STUN-only, no TURN.
    pub fn new(secret: String) -> Arc<Self> {
        Self::with_ice(
            secret,
            crate::turn::IceConfig {
                stun_urls: vec!["stun:stun.l.google.com:19302".into()],
                turn_urls: Vec::new(),
                turn_secret: None,
                turn_ttl: 86_400,
            },
        )
    }

    pub fn with_ice(secret: String, ice: crate::turn::IceConfig) -> Arc<Self> {
        Arc::new(AppState {
            secret,
            ice,
            devices: Mutex::new(HashMap::new()),
            conn_counter: AtomicU64::new(1),
            conns: AtomicUsize::new(0),
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/ws", any(ws_upgrade))
        .with_state(state)
}

async fn ws_upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    // Connection cap (RAII-decremented). Refuse and drop past the limit.
    state.conns.fetch_add(1, Ordering::Relaxed);
    let _guard = ConnGuard(Arc::clone(&state));
    if state.conns.load(Ordering::Relaxed) > MAX_CONNECTIONS {
        warn!("connection cap reached; refusing socket");
        return;
    }

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(OUTBOUND_BOUND);

    // Outbound pump: serialize queued messages; a Replaced error is terminal
    // (a newer connection took this device_id), so close after sending it.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let is_kick = matches!(
                &msg,
                ServerMessage::Error {
                    code: ErrorCode::Replaced,
                    ..
                }
            );
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if sink.send(WsMessage::Text(json.into())).await.is_err() || is_kick {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let conn_id = state.conn_counter.fetch_add(1, Ordering::Relaxed);
    let mut registered_id: Option<String> = None;
    let mut window_start = Instant::now();
    let mut in_window: u32 = 0;

    loop {
        // Bound how long an unregistered socket may sit idle. Registered
        // devices legitimately wait (silently) for offers, so only pre-Register
        // reads are deadlined.
        let next = if registered_id.is_none() {
            match tokio::time::timeout(REGISTER_TIMEOUT, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    debug!("closing socket that never registered");
                    break;
                }
            }
        } else {
            stream.next().await
        };
        let Some(Ok(msg)) = next else { break };

        // Per-connection inbound rate limit: a flood (even past the secret) is
        // dropped at the door rather than amplified into broadcasts.
        if window_start.elapsed() >= Duration::from_secs(1) {
            window_start = Instant::now();
            in_window = 0;
        }
        in_window += 1;
        if in_window > MAX_MSGS_PER_SEC {
            warn!("per-connection message rate exceeded; closing socket");
            break;
        }

        let WsMessage::Text(text) = msg else {
            match msg {
                WsMessage::Close(_) => break,
                _ => continue, // ping/pong handled by axum
            }
        };
        let parsed: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.try_send(ServerMessage::Error {
                    code: ErrorCode::BadMessage,
                    message: format!("unparseable message: {e}"),
                });
                continue;
            }
        };
        match parsed {
            ClientMessage::Register {
                device_id,
                name,
                caps,
                auth,
            } => {
                // Constant-time compare (consistent with the host's token /
                // pairing-proof checks) to avoid leaking the secret by timing.
                if !bool::from(auth.as_bytes().ct_eq(state.secret.as_bytes())) {
                    warn!(%device_id, "registration with bad secret refused");
                    let _ = tx.try_send(ServerMessage::Error {
                        code: ErrorCode::BadAuth,
                        message: "bad secret".into(),
                    });
                    break;
                }
                let mut devices = state.devices.lock().await;
                if let Some(old) = devices.insert(
                    device_id.clone(),
                    Device {
                        name: name.clone(),
                        caps,
                        conn_id,
                        tx: tx.clone(),
                    },
                ) {
                    info!(%device_id, "replacing stale registration");
                    let _ = old.tx.try_send(ServerMessage::Error {
                        code: ErrorCode::Replaced,
                        message: "a newer connection registered this device id".into(),
                    });
                }
                registered_id = Some(device_id.clone());
                info!(%device_id, %name, ?caps, "registered");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let ice_servers = state.ice.ice_servers_for(&device_id, now);
                let _ = tx.try_send(ServerMessage::Registered { ice_servers });
                broadcast_peers(&devices);
            }
            ClientMessage::Offer { target, sdp } => {
                let Some(from) = registered_id.clone() else {
                    let _ = not_registered(&tx);
                    continue;
                };
                let devices = state.devices.lock().await;
                // Directory-level capability enforcement (gate criterion):
                // only controllers may offer, only hosts may be offered to.
                if !devices
                    .get(&from)
                    .map(|d| d.caps.can_control)
                    .unwrap_or(false)
                {
                    let _ = tx.try_send(ServerMessage::Error {
                        code: ErrorCode::NotController,
                        message: "this device cannot control".into(),
                    });
                    continue;
                }
                match devices.get(&target) {
                    None => {
                        let _ = unknown_target(&tx, &target);
                    }
                    Some(t) if !t.caps.can_host => {
                        let _ = tx.try_send(ServerMessage::Error {
                            code: ErrorCode::TargetNotHost,
                            message: format!("{target} cannot host"),
                        });
                    }
                    Some(t) => {
                        debug!(%from, %target, "relaying offer");
                        let _ = t.tx.try_send(ServerMessage::Offer { from, sdp });
                    }
                }
            }
            ClientMessage::Answer { target, sdp } => {
                let Some(from) = registered_id.clone() else {
                    let _ = not_registered(&tx);
                    continue;
                };
                let devices = state.devices.lock().await;
                match devices.get(&target) {
                    None => {
                        let _ = unknown_target(&tx, &target);
                    }
                    Some(t) => {
                        debug!(%from, %target, "relaying answer");
                        let _ = t.tx.try_send(ServerMessage::Answer { from, sdp });
                    }
                }
            }
            ClientMessage::Ice { target, candidate } => {
                let Some(from) = registered_id.clone() else {
                    let _ = not_registered(&tx);
                    continue;
                };
                let devices = state.devices.lock().await;
                if let Some(t) = devices.get(&target) {
                    let _ = t.tx.try_send(ServerMessage::Ice { from, candidate });
                }
                // unknown target for trickle ICE: drop silently (candidates
                // can race a peer's disconnect; erroring is just noise)
            }
        }
    }

    // Deregister only if the map still points at *this* connection; a
    // replacement registration must not be wiped by the stale socket's exit.
    if let Some(device_id) = registered_id {
        let mut devices = state.devices.lock().await;
        if devices.get(&device_id).map(|d| d.conn_id) == Some(conn_id) {
            devices.remove(&device_id);
            info!(%device_id, "deregistered");
            broadcast_peers(&devices);
        }
    }
    send_task.abort();
}

fn broadcast_peers(devices: &HashMap<String, Device>) {
    let peers: Vec<PeerInfo> = devices
        .iter()
        .map(|(id, d)| PeerInfo {
            device_id: id.clone(),
            name: d.name.clone(),
            caps: d.caps,
        })
        .collect();
    for device in devices.values() {
        let _ = device.tx.try_send(ServerMessage::Peers {
            peers: peers.clone(),
        });
    }
}

fn not_registered(
    tx: &mpsc::Sender<ServerMessage>,
) -> Result<(), mpsc::error::TrySendError<ServerMessage>> {
    tx.try_send(ServerMessage::Error {
        code: ErrorCode::NotRegistered,
        message: "register first".into(),
    })
}

fn unknown_target(
    tx: &mpsc::Sender<ServerMessage>,
    target: &str,
) -> Result<(), mpsc::error::TrySendError<ServerMessage>> {
    tx.try_send(ServerMessage::Error {
        code: ErrorCode::UnknownTarget,
        message: format!("{target} is not online"),
    })
}
