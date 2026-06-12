# Phase 2 Plan — WebRTC transport + signaling

**Goal:** Devices connect peer-to-peer via WebRTC with a self-hosted signaling
server for pairing, replacing "hardcoded IP on the same LAN" as the only
connection path. The Phase 1 wire protocol rides WebRTC data channels
unchanged — that was the design bet, and Phase 2 cashes it.

**Out of scope** (unchanged from the brief): clipboard (P3), touch UX (P4),
adaptive bitrate / multi-monitor / TURN relay hardening (P5).

---

## 1. Key decisions

### 1.1 Video stays on the data channel; no RTP media tracks in Phase 2

Two ways to ship video over WebRTC:

- **Media track (RTP)**: proper congestion control and jitter buffering, but
  requires H.264 RTP packetization, ties the pipeline to RTP's codec
  assumptions, and abandons the codec-agnostic `FrameData` protocol we just
  gate-verified.
- **Data channel**: the brief's own words — "lift it onto WebRTC data channels
  in Phase 2 without redesign." Same bytes as Phase 1, byte-vector tests stay
  valid, JPEG keeps working on day one.

Choice: **data channel**, configured *unordered + bounded retransmit* so a
lost frame doesn't head-of-line-block newer ones (protocol already tolerates
`seq` gaps by design). Media tracks are re-evaluated in Phase 5 where adaptive
bitrate lives anyway.

### 1.2 Host WebRTC stack: `webrtc` crate (webrtc-rs)

The only full-featured, maintained, pure-Rust WebRTC implementation (peer
connection, DTLS/SCTP, data channels, ICE). Alternative `str0m` is leaner but
sans-IO — we'd hand-roll the driver loop for no Phase 2 benefit. The browser
side is native `RTCPeerConnection`; zero new controller dependencies.

### 1.3 Signaling: small Rust server, JSON over WebSocket

New crate `tether-signal` (axum + WS). JSON, not our binary protocol: signaling
is low-rate, and debuggability beats compactness here. Versioned message set:

```
register   { device_id, name, caps {can_host, can_control}, auth }
peers      { [ {device_id, name, caps, online} ] }        (server → client)
connect    { target_device_id }                            (controller initiates)
offer/answer { sdp }                                       (relayed verbatim)
ice        { candidate }                                   (relayed, trickle)
error      { code, message }
```

The server is a dumb relay + presence directory: it never sees media, only
SDP/ICE. Capability flags ride along so a control-only device can never be
offered as a host — same invariant as `Hello`, enforced at the directory too.

### 1.4 Pairing/auth floor (not full auth — that's later hardening)

Phase 1's floor was bind-validation + `--allow`. Phase 2's equivalent:

- Signaling requires a **pre-shared secret** (`--secret` on tetherd and the
  signal server, entered once in the controller UI, stored in localStorage).
  Sent as a bearer token on register; wrong token → connection refused.
- WebRTC itself gives DTLS encryption with SDP-exchanged fingerprints, so
  media is E2E-encrypted given honest signaling. Device-pair verification UX
  (short codes, TOFU pinning) is deferred and logged.
- The LAN WebSocket path from Phase 1 **stays** as a fallback transport
  (still useful, still tested, zero maintenance).

### 1.5 NAT traversal: STUN only

Configurable STUN (default: Google's public server). No TURN in Phase 2 —
symmetric-NAT pairs simply fail to connect and the controller says so.
Self-hosted coturn is a Phase 5 item (logged in deferred.md).

### 1.6 H.264 enters here (module 4), still inside `FrameData`

JPEG at 29 fps ≈ 15 MB/s — fine on LAN, unusable over most WAN uplinks. So
Phase 2 takes the deferred VideoToolbox swing, but on our terms:

- Host: `VtH264Encoder` implementing the existing `FrameEncoder` trait
  (VTCompressionSession via the `objc2-video-toolbox` bindings), hardware
  encode, `codec = h264` in FrameData, keyframe on session start/reconnect.
- Controller: WebCodecs `VideoDecoder` path in the viewer (Safari + Chrome
  both hardware-decode H.264), keyed off the codec byte; JPEG path remains as
  automatic fallback.
- **Timebox**: if VideoToolbox rabbit-holes again, Phase 2 still ships with
  JPEG + a quality/scale knob for WAN, and H.264 rolls into Phase 5. The
  transport work (modules 1–3) does not depend on it.

---

## 2. Repo changes

```
crates/
├── tether-signal/        # NEW: signaling server bin (axum, JSON/WS relay)
├── tetherd/
│   └── src/webrtc.rs     # NEW: peer connection + data-channel session
│                         #      (bridges the same ServerState channels)
└── tether-protocol/      # untouched — that's the point
controller/src/
├── signaling.ts          # NEW: signal-server client
├── webrtc.ts             # NEW: RTCPeerConnection wrapper, same events
│                         #      interface as connection.ts (viewer/input untouched)
└── decode.ts             # M4: WebCodecs H.264 path beside JPEG
```

`tetherd` gains `--signal <wss://host>`, `--device-name`, `--secret` (LAN
`--bind/--allow` mode unchanged). The session layer already consumes
watch/mpsc channels, so a WebRTC data channel is just a second producer/
consumer of `ServerState` — no changes to capture, encode, or injection.

## 3. New dependencies

| Dependency | Where | Justification |
|---|---|---|
| `webrtc` | tetherd | Only maintained full Rust WebRTC stack; data channels + ICE + DTLS. |
| `axum` | tether-signal | De facto tokio web framework; WS support built in. |
| `serde` / `serde_json` | tether-signal, tetherd | JSON signaling messages. |
| `objc2-video-toolbox` (M4) | tetherd | Maintained autogenerated VideoToolbox bindings for VTCompressionSession. |

Build note: webrtc-rs is a heavy dependency tree; check disk headroom before
module 3 (Phase 1 already showed ENOSPC once).

## 4. Build order (compile + smoke test + review gate per module)

**M1 — `tether-signal`.** Axum WS server: register/auth, presence, relay
offer/answer/ice between a device pair. In-memory state only. *Tests: two fake
clients register, exchange SDP blobs through the relay; bad secret rejected;
control-only device refused as a connect target.*

**M2 — Controller WebRTC path.** `signaling.ts` + `webrtc.ts`: register,
pick host, offer, data channel up, then feed the existing decode/render/input
modules through the same events interface as `connection.ts`. UI grows a
transport selector (LAN / signaled). *Smoke: vitest for signaling message
codec + state machine; live test arrives with M3.*

**M3 — tetherd WebRTC session.** webrtc-rs peer connection: register with the
signal server, answer offers, open data channel, bridge to `ServerState`
(frames out, input in — reusing the single-session gate). *Smoke: full live
loop on this machine through a local signal server: browser ↔ data channel ↔
tetherd, gate-parity check (fps, input, reconnect). Then a cross-network
sanity test if available.*

**M4 — H.264 (timeboxed).** `VtH264Encoder` + WebCodecs decode path as in
§1.6. *Smoke: encode smoke test (fps, bitrate vs JPEG), live viewer at both
codecs, bandwidth measured.*

**M5 — Hardening + gate.** Data-channel reconnect (re-offer without daemon
restart), signal-server restart tolerance, latency/fps measurement over the
signaled path, gate write-up.

## 5. Phase 2 gate criteria (proposed — your call)

1. Controller connects to host via the signaling server with **no IP
   configuration** on the controller beyond the signal server address.
2. Stream + full input parity with Phase 1 gates over the data channel
   (≥15 fps native, exact mouse mapping, modifiers, <150 ms LAN).
3. Clean reconnect: controller can drop and re-establish the WebRTC session
   without restarting tetherd or the signal server.
4. Wrong-secret and can_host=false targets are refused at signaling.
5. (If M4 lands) H.264 stream ≤ 3 Mbps at usable quality, decoded in hardware
   in Safari and Chrome.

## 6. Risks

- **webrtc-rs API friction** — biggest unknown; mitigation: data-channel-only
  usage is its best-trodden path, and examples cover exactly this shape.
- **VideoToolbox** — the known rabbit hole; explicitly timeboxed and
  non-blocking (§1.6).
- **NAT reality** — STUN-only fails for symmetric NAT pairs; accepted and
  surfaced in UI (deferred: TURN).
- **Browser autoplay/WebCodecs quirks** — Safari's WebCodecs H.264 support is
  good but config-sensitive (annex-b vs avcc); JPEG fallback contains it.

---

**Status: awaiting architect approval (gate 1). No Phase 2 code written.**
