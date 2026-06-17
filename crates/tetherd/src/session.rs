use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tether_protocol::{
    CAP_CAN_CONTROL, CAP_CAN_HOST, ClipboardData, Decoded, FrameData, Hello, Message,
    PROTOCOL_VERSION, Role,
};
use tracing::{debug, info, warn};

use crate::input::InjectCommand;
use crate::server::ServerState;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

type Ws = WebSocketStream<TcpStream>;

/// Drive one controller session: WS upgrade, Hello handshake, then relay
/// frames out / input events in until the peer disconnects.
pub async fn run(stream: TcpStream, peer: SocketAddr, state: ServerState) -> anyhow::Result<()> {
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .context("websocket upgrade failed")?;

    match handshake(&mut ws).await {
        Ok(hello) => {
            info!(%peer, caps = hello.capabilities, "controller connected");
        }
        Err(e) => {
            // Polite close so the controller can show a reason; ignore failures.
            let _ = ws.close(None).await;
            return Err(e);
        }
    }

    let mut frames = state.frames.clone();
    let mut resolution = state.resolution.clone();
    let current_resolution = *resolution.borrow_and_update();
    send(&mut ws, &Message::Resolution(current_resolution)).await?;
    // A controller connecting mid-stream should get the current frame
    // immediately rather than waiting for the next capture.
    frames.mark_changed();
    // ...and the current host clipboard, so paste works before the next copy.
    let mut clipboard = state.clipboard_out.clone();
    let current_clip = clipboard.borrow_and_update().clone();
    if let Some(text) = current_clip {
        send(&mut ws, &Message::ClipboardData(ClipboardData { text })).await?;
    }

    loop {
        tokio::select! {
            changed = frames.changed() => {
                if changed.is_err() {
                    bail!("frame source closed");
                }
                let frame = frames.borrow_and_update().clone();
                if let Some(f) = frame {
                    let msg = Message::FrameData(FrameData {
                        codec: f.codec,
                        seq: f.seq,
                        timestamp_micros: f.timestamp_micros,
                        payload: f.payload,
                    });
                    send(&mut ws, &msg).await?;
                }
            }
            changed = resolution.changed() => {
                if changed.is_err() {
                    bail!("resolution source closed");
                }
                let current_resolution = *resolution.borrow_and_update();
                send(&mut ws, &Message::Resolution(current_resolution)).await?;
            }
            changed = clipboard.changed() => {
                if changed.is_err() {
                    bail!("clipboard source closed");
                }
                let text = clipboard.borrow_and_update().clone();
                if let Some(text) = text {
                    send(&mut ws, &Message::ClipboardData(ClipboardData { text })).await?;
                }
            }
            incoming = ws.next() => {
                match incoming {
                    None | Some(Ok(WsMessage::Close(_))) => {
                        info!(%peer, "controller disconnected");
                        return Ok(());
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => handle_incoming(&bytes, &state).await,
                    Some(Ok(_)) => {} // text/ping/pong: ignore (tungstenite answers pings)
                    Some(Err(e)) => {
                        info!(%peer, error = %e, "connection error, ending session");
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn handle_incoming(bytes: &[u8], state: &ServerState) {
    match Message::decode(bytes) {
        Ok(Decoded::Message { message: Message::InputEvent(ev), .. }) => {
            debug!(?ev, "input event");
            // If the injector is gone the daemon is shutting down; drop silently.
            let _ = state.input_tx.send(InjectCommand::Event(ev)).await;
        }
        Ok(Decoded::Message { message: Message::TextInput(t), .. }) => {
            let _ = state.input_tx.send(InjectCommand::Text(t.text)).await;
        }
        Ok(Decoded::Message { message: Message::ClipboardData(c), .. }) => {
            let _ = state.clipboard_in.send(c.text);
        }
        Ok(Decoded::Unknown { msg_type, .. }) => {
            debug!(msg_type, "ignoring unknown message type");
        }
        Ok(other) => warn!(?other, "unexpected message from controller"),
        Err(e) => warn!(error = %e, "undecodable message from controller"),
    }
}

/// Shared by both transports (WS here, data channel in webrtc.rs).
pub fn validate_controller_hello(hello: &Hello) -> Result<(), String> {
    if hello.version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch: controller speaks v{}, host speaks v{PROTOCOL_VERSION}",
            hello.version
        ));
    }
    if hello.role != Role::Controller {
        return Err(format!("peer Hello has role {:?}, expected Controller", hello.role));
    }
    if hello.capabilities & CAP_CAN_CONTROL == 0 {
        return Err("peer lacks can_control capability".into());
    }
    Ok(())
}

pub fn host_hello() -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        role: Role::Host,
        capabilities: CAP_CAN_HOST | CAP_CAN_CONTROL,
    })
}

async fn handshake(ws: &mut Ws) -> anyhow::Result<Hello> {
    let hello = timeout(HANDSHAKE_TIMEOUT, read_hello(ws))
        .await
        .context("handshake timed out")??;
    validate_controller_hello(&hello).map_err(anyhow::Error::msg)?;
    send(ws, &host_hello()).await?;
    Ok(hello)
}

async fn read_hello(ws: &mut Ws) -> anyhow::Result<Hello> {
    loop {
        match ws.next().await {
            None => bail!("peer closed before Hello"),
            Some(Ok(WsMessage::Binary(bytes))) => match Message::decode(&bytes)? {
                Decoded::Message { message: Message::Hello(h), .. } => return Ok(h),
                other => bail!("expected Hello, got {other:?}"),
            },
            Some(Ok(WsMessage::Close(_))) => bail!("peer closed before Hello"),
            Some(Ok(_)) => {} // ignore control frames
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn send(ws: &mut Ws, msg: &Message) -> anyhow::Result<()> {
    ws.send(WsMessage::Binary(msg.encode()))
        .await
        .context("websocket send failed")
}
