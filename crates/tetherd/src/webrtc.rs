//! WebRTC host session: register with the signal server, answer offers,
//! bridge the data channels onto the same `ServerState` the LAN transport
//! uses.
//!
//! Channel layout (mirrors controller/src/webrtc.ts):
//!   "tether-ctl"   reliable + ordered    Hello, Resolution, InputEvent
//!   "tether-media" unordered, no retx    FrameData, chunked (see below)
//!
//! A new offer replaces any active peer session: with the shared-secret floor
//! that means a reconnecting controller gets in immediately instead of
//! waiting out ICE disconnect timers. (Within one secret this allows takeover
//! — logged in deferred.md.)

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use tether_protocol::{ClipboardData, Decoded, FrameData, Message, Resolution};
use tether_signal::protocol::{Caps, ClientMessage, ErrorCode, ServerMessage};
use tracing::{debug, info, warn};

use crate::capture::EncodedFrame;
use crate::server::ServerState;
use crate::session::{host_hello, validate_controller_hello};

// ---------------------------------------------------------------------------
// Frame chunking — byte-identical to controller/src/chunks.ts:
//   [ u32 LE frame_seq ][ u16 LE chunk_idx ][ u16 LE chunk_count ][ slice ]
// over slices of the complete tether wire message.

pub const CHUNK_PAYLOAD: usize = 64 * 1024 - 8;
const CHUNK_HEADER: usize = 8;

pub fn chunk_frame(frame_seq: u32, wire: &[u8]) -> Vec<Bytes> {
    let count = wire.len().div_ceil(CHUNK_PAYLOAD).max(1);
    let mut chunks = Vec::with_capacity(count);
    for idx in 0..count {
        let slice = &wire[idx * CHUNK_PAYLOAD..(idx * CHUNK_PAYLOAD + CHUNK_PAYLOAD).min(wire.len())];
        let mut chunk = Vec::with_capacity(CHUNK_HEADER + slice.len());
        chunk.extend_from_slice(&frame_seq.to_le_bytes());
        chunk.extend_from_slice(&(idx as u16).to_le_bytes());
        chunk.extend_from_slice(&(count as u16).to_le_bytes());
        chunk.extend_from_slice(slice);
        chunks.push(Bytes::from(chunk));
    }
    chunks
}

/// Latest-wins reassembler (the host only sends in Phase 2; this lives here
/// for symmetry, the e2e test, and the future host-receives path).
pub struct FrameReassembler {
    seq: Option<u32>,
    count: usize,
    received: usize,
    parts: Vec<Option<Bytes>>,
}

impl Default for FrameReassembler {
    fn default() -> Self {
        FrameReassembler { seq: None, count: 0, received: 0, parts: Vec::new() }
    }
}

impl FrameReassembler {
    pub fn on_chunk(&mut self, bytes: &[u8]) -> Option<Bytes> {
        if bytes.len() < CHUNK_HEADER {
            return None;
        }
        let seq = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let idx = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        if count == 0 || idx >= count {
            return None;
        }
        if self.seq != Some(seq) {
            if let Some(current) = self.seq {
                if seq_older(seq, current) {
                    return None;
                }
            }
            self.seq = Some(seq);
            self.count = count;
            self.received = 0;
            self.parts = vec![None; count];
        }
        if count != self.count || self.parts[idx].is_some() {
            return None;
        }
        self.parts[idx] = Some(Bytes::copy_from_slice(&bytes[CHUNK_HEADER..]));
        self.received += 1;
        if self.received < self.count {
            return None;
        }
        let mut wire = Vec::with_capacity(self.parts.iter().map(|p| p.as_ref().unwrap().len()).sum());
        for part in self.parts.drain(..) {
            wire.extend_from_slice(&part.unwrap());
        }
        self.seq = None;
        Some(Bytes::from(wire))
    }
}

fn seq_older(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

// ---------------------------------------------------------------------------
// Signal client + peer session

#[derive(Debug, Clone)]
pub struct RtcConfig {
    /// ws:// or wss:// URL of the signal server's /ws endpoint.
    pub signal_url: String,
    pub secret: String,
    pub device_id: String,
    pub device_name: String,
    pub stun: Vec<String>,
}

/// Register as a host and serve WebRTC sessions until the process exits.
/// Reconnects to the signal server with backoff; never returns under normal
/// operation.
pub async fn run_host(config: RtcConfig, state: ServerState) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = tokio::time::Instant::now();
        match signal_session(&config, &state).await {
            Ok(()) => info!("signal connection closed, reconnecting"),
            Err(e) => warn!(error = %e, "signal connection failed"),
        }
        // A session that survived a while means the server was healthy.
        if started.elapsed() > Duration::from_secs(10) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

struct ActivePeer {
    controller_id: String,
    pc: Arc<RTCPeerConnection>,
}

async fn signal_session(config: &RtcConfig, state: &ServerState) -> anyhow::Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(&config.signal_url)
        .await
        .context("connecting to signal server")?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    // Writer task: peer-connection callbacks queue messages here.
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = signal_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else { continue };
            if ws_sink.send(WsMessage::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    signal_tx.send(ClientMessage::Register {
        device_id: config.device_id.clone(),
        name: config.device_name.clone(),
        caps: Caps { can_host: true, can_control: true },
        auth: config.secret.clone(),
    })?;

    let active: Arc<Mutex<Option<ActivePeer>>> = Arc::new(Mutex::new(None));

    let result = async {
        while let Some(msg) = ws_stream.next().await {
            let msg = msg.context("signal stream error")?;
            let WsMessage::Text(text) = msg else {
                if matches!(msg, WsMessage::Close(_)) {
                    return Ok(());
                }
                continue;
            };
            let parsed: ServerMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "unparseable signal message");
                    continue;
                }
            };
            match parsed {
                ServerMessage::Registered => {
                    info!(device_id = %config.device_id, "registered with signal server");
                }
                ServerMessage::Offer { from, sdp } => {
                    info!(%from, "received offer, starting peer session");
                    let mut slot = active.lock().await;
                    if let Some(old) = slot.take() {
                        info!(old = %old.controller_id, "replacing active peer session");
                        let _ = old.pc.close().await;
                    }
                    match answer_offer(config, state, &signal_tx, from.clone(), sdp).await {
                        Ok(pc) => *slot = Some(ActivePeer { controller_id: from, pc }),
                        Err(e) => warn!(error = %e, "failed to answer offer"),
                    }
                }
                ServerMessage::Ice { from, candidate } => {
                    let slot = active.lock().await;
                    match slot.as_ref() {
                        Some(peer) if peer.controller_id == from => {
                            match serde_json::from_str::<RTCIceCandidateInit>(&candidate) {
                                Ok(init) => {
                                    if let Err(e) = peer.pc.add_ice_candidate(init).await {
                                        debug!(error = %e, "add_ice_candidate failed");
                                    }
                                }
                                Err(e) => debug!(error = %e, "bad ice candidate json"),
                            }
                        }
                        _ => debug!(%from, "ice for unknown peer, dropping"),
                    }
                }
                ServerMessage::Error { code, message } => {
                    if code == ErrorCode::Replaced {
                        anyhow::bail!("another tetherd registered this device id: {message}");
                    }
                    warn!(?code, %message, "signal server error");
                }
                ServerMessage::Peers { .. } | ServerMessage::Answer { .. } => {}
            }
        }
        Ok(())
    }
    .await;

    if let Some(peer) = active.lock().await.take() {
        let _ = peer.pc.close().await;
    }
    writer.abort();
    result
}

async fn answer_offer(
    config: &RtcConfig,
    state: &ServerState,
    signal_tx: &mpsc::UnboundedSender<ClientMessage>,
    from: String,
    offer_sdp: String,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let api = APIBuilder::new().build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: config.stun.clone(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await?,
    );

    // Trickle our candidates to the controller.
    {
        let signal_tx = signal_tx.clone();
        let target = from.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let signal_tx = signal_tx.clone();
            let target = target.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    if let Ok(init) = c.to_json() {
                        if let Ok(json) = serde_json::to_string(&init) {
                            let _ = signal_tx.send(ClientMessage::Ice {
                                target,
                                candidate: json,
                            });
                        }
                    }
                }
            })
        }));
    }

    // The controller creates both channels; wire them up as they arrive.
    {
        let state = state.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let state = state.clone();
            Box::pin(async move {
                match dc.label() {
                    "tether-ctl" => wire_ctl_channel(dc, state),
                    "tether-media" => wire_media_channel(dc, state.frames.clone()),
                    other => debug!(label = %other, "ignoring unexpected data channel"),
                }
            })
        }));
    }

    pc.set_remote_description(RTCSessionDescription::offer(offer_sdp)?).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer.clone()).await?;
    signal_tx.send(ClientMessage::Answer { target: from, sdp: answer.sdp })?;
    Ok(pc)
}

/// Control channel: Hello handshake, Resolution announcements, input events.
fn wire_ctl_channel(dc: Arc<RTCDataChannel>, state: ServerState) {
    let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(256);
    dc.on_message(Box::new(move |msg| {
        let in_tx = in_tx.clone();
        Box::pin(async move {
            let _ = in_tx.send(msg.data).await;
        })
    }));

    tokio::spawn(async move {
        // Handshake: the controller speaks first, and only after the channel
        // opens, so replies are safe to send from here on.
        let Some(first) = in_rx.recv().await else { return };
        let hello = match Message::decode(&first) {
            Ok(Decoded::Message { message: Message::Hello(h), .. }) => h,
            other => {
                warn!(?other, "expected Hello on ctl channel");
                let _ = dc.close().await;
                return;
            }
        };
        if let Err(reason) = validate_controller_hello(&hello) {
            warn!(%reason, "rejecting controller");
            let _ = dc.close().await;
            return;
        }
        if dc.send(&host_hello().encode()).await.is_err() {
            return;
        }
        let mut resolution = state.resolution.clone();
        let current: Resolution = *resolution.borrow_and_update();
        if dc.send(&Message::Resolution(current).encode()).await.is_err() {
            return;
        }
        // current host clipboard, so paste works before the next copy
        let mut clipboard = state.clipboard_out.clone();
        let current_clip = clipboard.borrow_and_update().clone();
        if let Some(text) = current_clip {
            let msg = Message::ClipboardData(ClipboardData { text });
            if dc.send(&msg.encode()).await.is_err() {
                return;
            }
        }
        info!("webrtc controller connected");

        loop {
            tokio::select! {
                changed = resolution.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let current: Resolution = *resolution.borrow_and_update();
                    if dc.send(&Message::Resolution(current).encode()).await.is_err() {
                        break;
                    }
                }
                changed = clipboard.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let text = clipboard.borrow_and_update().clone();
                    if let Some(text) = text {
                        let msg = Message::ClipboardData(ClipboardData { text });
                        if dc.send(&msg.encode()).await.is_err() {
                            break;
                        }
                    }
                }
                incoming = in_rx.recv() => {
                    let Some(bytes) = incoming else { break };
                    match Message::decode(&bytes) {
                        Ok(Decoded::Message { message: Message::InputEvent(ev), .. }) => {
                            let _ = state.input_tx.send(ev).await;
                        }
                        Ok(Decoded::Message { message: Message::ClipboardData(c), .. }) => {
                            let _ = state.clipboard_in.send(c.text);
                        }
                        Ok(Decoded::Unknown { msg_type, .. }) => {
                            debug!(msg_type, "ignoring unknown message type");
                        }
                        other => warn!(?other, "unexpected ctl message"),
                    }
                }
            }
        }
        info!("webrtc controller disconnected");
    });
}

/// Media channel: pump encoded frames out as chunks, latest-wins under
/// backpressure (skip frames while the SCTP buffer is deep).
fn wire_media_channel(dc: Arc<RTCDataChannel>, frames: watch::Receiver<Option<EncodedFrame>>) {
    const MAX_BUFFERED: usize = 1_000_000;

    let opened = Arc::new(tokio::sync::Notify::new());
    {
        let opened = opened.clone();
        dc.on_open(Box::new(move || {
            opened.notify_one();
            Box::pin(async {})
        }));
    }

    tokio::spawn(async move {
        opened.notified().await;
        let mut frames = frames;
        frames.mark_changed(); // a mid-stream joiner gets the current frame
        while frames.changed().await.is_ok() {
            let Some(frame) = frames.borrow_and_update().clone() else { continue };
            if dc.buffered_amount().await > MAX_BUFFERED {
                debug!("media channel backed up, dropping frame");
                continue;
            }
            let wire = Message::FrameData(FrameData {
                codec: frame.codec,
                seq: frame.seq,
                timestamp_micros: frame.timestamp_micros,
                payload: frame.payload,
            })
            .encode();
            for chunk in chunk_frame(frame.seq, &wire) {
                if dc.send(&chunk).await.is_err() {
                    return; // channel closed
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_header_matches_ts_format() {
        // Pinned against controller/src/chunks.ts: u32 LE seq, u16 LE idx,
        // u16 LE count.
        let chunks = chunk_frame(0x01020304, &[0xAA; 100]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0][..8], &[0x04, 0x03, 0x02, 0x01, 0, 0, 1, 0]);
        assert_eq!(&chunks[0][8..], &[0xAA; 100]);
    }

    #[test]
    fn chunk_and_reassemble_round_trip() {
        let wire: Vec<u8> = (0..(CHUNK_PAYLOAD * 2 + 500)).map(|i| (i % 251) as u8).collect();
        let chunks = chunk_frame(7, &wire);
        assert_eq!(chunks.len(), 3);
        let mut r = FrameReassembler::default();
        assert!(r.on_chunk(&chunks[0]).is_none());
        assert!(r.on_chunk(&chunks[2]).is_none()); // out of order
        assert_eq!(r.on_chunk(&chunks[1]).unwrap(), Bytes::from(wire));
    }

    #[test]
    fn newer_frame_discards_partial_older() {
        let old = chunk_frame(1, &[1u8; CHUNK_PAYLOAD + 1]);
        let new_wire = vec![9u8; 100];
        let mut r = FrameReassembler::default();
        assert!(r.on_chunk(&old[0]).is_none());
        assert_eq!(r.on_chunk(&chunk_frame(2, &new_wire)[0]).unwrap(), Bytes::from(new_wire));
        assert!(r.on_chunk(&old[1]).is_none(), "straggler must not resurrect old frame");
    }
}
