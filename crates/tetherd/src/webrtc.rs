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
use std::sync::atomic::Ordering;
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
use tether_signal::protocol::{
    Caps, ClientMessage, ErrorCode, IceServer as ServerIceServer, ServerMessage,
};
use tracing::{debug, info, warn};

use crate::capture::EncodedFrame;
use crate::input::InjectCommand;
use crate::server::ServerState;
use crate::session::{host_hello, validate_controller_hello};

// ---------------------------------------------------------------------------
// Frame chunking — byte-identical to controller/src/chunks.ts:
//   [ u32 LE frame_seq ][ u16 LE chunk_idx ][ u16 LE chunk_count ][ slice ]
// over slices of the complete tether wire message.

// 16 KiB per message is the safe interop bound for data channels (and
// webrtc-rs silently drops inbound messages at its 64 KiB buffer boundary).
pub const CHUNK_PAYLOAD: usize = 16 * 1024 - 8;
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
    // ICE servers (STUN + ephemeral TURN) supplied by the signal server on
    // Registered; defaults to the local STUN config until then.
    let ice_servers: Arc<Mutex<Vec<RTCIceServer>>> =
        Arc::new(Mutex::new(to_rtc_ice(&[], config)));

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
                ServerMessage::Registered { ice_servers: servers } => {
                    info!(
                        device_id = %config.device_id,
                        ice = servers.len(),
                        "registered with signal server"
                    );
                    *ice_servers.lock().await = to_rtc_ice(&servers, config);
                }
                ServerMessage::Offer { from, sdp } => {
                    info!(%from, "received offer, starting peer session");
                    let ice = ice_servers.lock().await.clone();
                    let mut slot = active.lock().await;
                    if let Some(old) = slot.take() {
                        info!(old = %old.controller_id, "replacing active peer session");
                        let _ = old.pc.close().await;
                    }
                    match answer_offer(config, state, &signal_tx, from.clone(), sdp, ice).await {
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

/// Convert signal-server ICE entries to webrtc-rs `RTCIceServer`s, falling back
/// to the host's local STUN config when the server advertised none.
fn to_rtc_ice(servers: &[ServerIceServer], config: &RtcConfig) -> Vec<RTCIceServer> {
    if servers.is_empty() {
        return vec![RTCIceServer { urls: config.stun.clone(), ..Default::default() }];
    }
    servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            credential: s.credential.clone().unwrap_or_default(),
            ..Default::default()
        })
        .collect()
}

async fn answer_offer(
    config: &RtcConfig,
    state: &ServerState,
    signal_tx: &mpsc::UnboundedSender<ClientMessage>,
    from: String,
    offer_sdp: String,
    ice_servers: Vec<RTCIceServer>,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let api = APIBuilder::new().build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers,
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
        let pc_for_dc = Arc::clone(&pc);
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let state = state.clone();
            let pc_for_dc = Arc::clone(&pc_for_dc);
            Box::pin(async move {
                debug!(label = %dc.label(), "data channel announced");
                match dc.label() {
                    "tether-ctl" => wire_ctl_channel(dc, state, pc_for_dc.clone()),
                    "tether-media" => wire_media_channel(dc, state.clone()),
                    "tether-bulk" => wire_bulk_channel(dc, state),
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

/// Derive the pairing channel binding from the negotiated DTLS fingerprints,
/// read from this peer's local + remote SDP. Under an honest connection both
/// ends compute the same value; a relay that swaps fingerprints to MITM yields
/// different values on each side, so the channel-bound pairing proof fails.
async fn dtls_channel_binding(pc: &RTCPeerConnection) -> [u8; 32] {
    let local = pc.local_description().await.and_then(|d| sdp_fingerprint(&d.sdp));
    let remote = pc.remote_description().await.and_then(|d| sdp_fingerprint(&d.sdp));
    match (local, remote) {
        (Some(l), Some(r)) => crate::auth::channel_binding(&l, &r),
        // If a fingerprint is somehow missing, fall back to a fixed binding;
        // pairing still requires the code but loses MITM resistance — logged.
        _ => {
            warn!("missing DTLS fingerprint; pairing MITM resistance degraded");
            crate::auth::channel_binding("tether-no-fp", "tether-no-fp")
        }
    }
}

/// Extract the `a=fingerprint:` value from an SDP (first occurrence).
fn sdp_fingerprint(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix("a=fingerprint:"))
        .map(|v| v.to_owned())
}

/// Run the auth gate over the WebRTC ctl channel. Returns true if authenticated.
async fn run_auth_gate(
    dc: &Arc<RTCDataChannel>,
    in_rx: &mut mpsc::Receiver<Bytes>,
    state: &ServerState,
    binding: &[u8; 32],
) -> bool {
    loop {
        let Some(bytes) = in_rx.recv().await else { return false };
        let msg = match Message::decode(&bytes) {
            Ok(Decoded::Message { message, .. }) => message,
            _ => continue,
        };
        let (response, decision) = {
            let mut auth = state.auth.lock().await;
            crate::session::handle_auth_message(
                &mut auth,
                state.auth_policy,
                &msg,
                binding,
                crate::auth::now_unix(),
            )
        };
        if let Some(resp) = response {
            if dc.send(&resp.encode()).await.is_err() {
                return false;
            }
        }
        match decision {
            crate::session::AuthDecision::Proceed => return true,
            crate::session::AuthDecision::Reject => return false,
            crate::session::AuthDecision::Continue => {}
        }
    }
}

/// Control channel: Hello handshake, device auth, Resolution, input events.
fn wire_ctl_channel(dc: Arc<RTCDataChannel>, state: ServerState, pc: Arc<RTCPeerConnection>) {
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

        // Device-pairing / token auth gate, bound to the DTLS fingerprints so a
        // malicious signal relay can't MITM the pairing.
        let binding = dtls_channel_binding(&pc).await;
        if !run_auth_gate(&dc, &mut in_rx, &state, &binding).await {
            let _ = dc.close().await;
            return;
        }

        let mut resolution = state.resolution.clone();
        let current: Resolution = *resolution.borrow_and_update();
        if dc.send(&Message::Resolution(current).encode()).await.is_err() {
            return;
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
                incoming = in_rx.recv() => {
                    let Some(bytes) = incoming else { break };
                    match Message::decode(&bytes) {
                        Ok(Decoded::Message { message: Message::InputEvent(ev), .. }) => {
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
                        other => warn!(?other, "unexpected ctl message"),
                    }
                }
            }
        }
        info!("webrtc controller disconnected");
    });
}

/// Bulk channel: messages too large for a single data-channel message
/// (SCTP implementations cap at ~64 KiB), carried via the same chunk framing
/// as media. Clipboard both directions today; file transfer later.
fn wire_bulk_channel(dc: Arc<RTCDataChannel>, state: ServerState) {
    // inbound: reassemble → decode → dispatch (clipboard only, for now)
    let reassembler = Arc::new(Mutex::new(FrameReassembler::default()));
    {
        let state = state.clone();
        dc.on_message(Box::new(move |msg| {
            let reassembler = reassembler.clone();
            let state = state.clone();
            Box::pin(async move {
                debug!(len = msg.data.len(), "bulk chunk received");
                let Some(wire) = reassembler.lock().await.on_chunk(&msg.data) else {
                    return;
                };
                match Message::decode(&wire) {
                    Ok(Decoded::Message { message: Message::ClipboardData(c), .. }) => {
                        let _ = state.clipboard_in.send(c.text);
                    }
                    other => debug!(?other, "ignoring non-clipboard bulk message"),
                }
            })
        }));
    }

    // outbound: host clipboard, chunked; includes the current value at open
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
        let mut clipboard = state.clipboard_out.clone();
        clipboard.mark_changed();
        let mut bulk_seq: u32 = 0;
        while clipboard.changed().await.is_ok() {
            let text = clipboard.borrow_and_update().clone();
            let Some(text) = text else { continue };
            bulk_seq = bulk_seq.wrapping_add(1);
            let wire = Message::ClipboardData(ClipboardData { text }).encode();
            for chunk in chunk_frame(bulk_seq, &wire) {
                if dc.send(&chunk).await.is_err() {
                    return; // channel closed; session is ending
                }
            }
        }
    });
}

/// Media channel: pump encoded frames out as chunks, latest-wins under
/// backpressure (skip frames while the SCTP buffer is deep).
fn wire_media_channel(dc: Arc<RTCDataChannel>, state: ServerState) {
    const MAX_BUFFERED: usize = 1_000_000;
    const BITRATE_FLOOR_KBPS: u32 = 600;

    let opened = Arc::new(tokio::sync::Notify::new());
    {
        let opened = opened.clone();
        dc.on_open(Box::new(move || {
            opened.notify_one();
            Box::pin(async {})
        }));
    }

    // Adaptive-bitrate loop: sample the send buffer and steer the shared
    // encoder bitrate via AIMD. Only meaningful for H.264 (JPEG ignores
    // set_bitrate); harmless either way.
    {
        let dc = dc.clone();
        let bitrate = state.bitrate.clone();
        let mut controller =
            crate::adaptive::BitrateController::new(state.bitrate_ceiling_kbps, BITRATE_FLOOR_KBPS);
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(crate::adaptive::SAMPLE_INTERVAL_MS));
            loop {
                tick.tick().await;
                // stop when the channel closes
                use webrtc::data_channel::data_channel_state::RTCDataChannelState;
                if dc.ready_state() == RTCDataChannelState::Closed {
                    return;
                }
                let buffered = dc.buffered_amount().await;
                let target = controller.sample(buffered);
                bitrate.store(target, Ordering::Relaxed);
            }
        });
    }

    tokio::spawn(async move {
        opened.notified().await;
        let mut frames = state.frames.clone();
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
