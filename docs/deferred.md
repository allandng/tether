# Deferred decisions

Decision points hit during Phase 1 that were out of scope (workflow rule 4):
each got the simplest choice that doesn't block Phase 2.

| Decision | Phase 1 choice | Revisit when |
|---|---|---|
| **H.264 / VideoToolbox** | Ship JPEG. Rust bindings for VideoToolbox are immature (raw FFI, no maintained safe wrapper); JPEG via turbojpeg sustains the ≥15 fps gate (verified by `tests/e2e_fake.rs` and `capture_smoke`), and the protocol already carries a codec id while `FrameEncoder` is a trait, so H.264 is a drop-in encoder + a WebCodecs `VideoDecoder` path in the viewer. | Phase 2 — WebRTC wants real video anyway; bandwidth on WAN makes it necessary, and browsers decode H.264 in hardware via WebCodecs. |
| **Multi-monitor** | Capture the main display only (`CGDisplay::main()` matched against SCK's display list). | Phase 5. |
| **Cursor rendering** | Cursor is composited into the capture by ScreenCaptureKit (`shows_cursor: true`); no client-side cursor layer. Costs nothing; slight ghosting at low fps is acceptable. | Phase 5 (client-drawn cursor removes ghosting and enables instant local feedback). |
| **Multiple controllers** | Second concurrent connection is dropped (tested). | Phase 5, if ever. |
| **Browser-reserved shortcuts** | Cmd+W / Cmd+Q / Cmd+Tab cannot be intercepted by a web page; they act on the controller's browser. All other combos forward correctly. | Phase 4 — Keyboard Lock API in fullscreen (Chromium) or a native controller shell. |
| **Key auto-repeat** | Browser auto-repeat keydowns are forwarded as repeated KeyDown events (synthetic held keys don't auto-repeat on the host). Repeat *rate* is therefore the controller's, not the host's. | Fine indefinitely. |
| **Caps Lock** | Forwarded as a plain key event; host caps-lock LED/state may not toggle reliably with synthetic events. Not tracked in modifier flags. | If it ever matters. |
| **Scroll direction sign** | DOM deltas negated onto CG axes (`wheel1 = -deltaY`), which matches content-follows-finger expectations in testing. macOS "natural scrolling" preference applies on top of injected deltas. | Verify feel during live gate testing; flip is a one-line change in `input/macos.rs`. |
| **Clock-skew latency display** | Frame-age readout in the controller status bar is only meaningful when host and controller clocks agree (same machine / NTP-synced LAN). Hidden when implausible. Gate latency criterion is subjective anyway. | Phase 2 can add an RTT echo message. |
| **Capture during display sleep / resolution change** | Resolution changes are re-announced mid-session (pipeline detects dimension change per frame). Display sleep/hot-plug behavior unverified. | Phase 5 hardening. |
| **WS connection over HTTPS pages** | Controller is served over plain HTTP in Phase 1 (`ws://` from `http://` is fine; `wss://` would need TLS on tetherd). | Phase 2 (WebRTC removes the need). |

## Phase 2 additions

| Decision | Phase 2 choice | Revisit when |
|---|---|---|
| **Same-secret takeover** | A new offer replaces the active peer session, so a reconnecting controller gets in instantly instead of waiting out ICE timers. Within one shared secret, any device can therefore take over a session. | Real pairing/auth (per-device identity) — Phase 5 hardening. |
| **TURN relay** | STUN only; symmetric-NAT pairs fail to connect with a "peer connection failed" status. | Phase 5 (self-hosted coturn + credentials via the signal server). |
| **Codec negotiation** | Host-side `--codec` flag; the controller is not consulted. A controller without WebCodecs gets a black canvas in h264 mode. | Add a capability bit in `Hello` (e.g. `0b100 = h264-decode`) and per-session encoder choice — needs per-session pipelines first. |
| **Per-codec channel modes** | Media channel is reliable+ordered for both codecs (H.264 requires it). JPEG could ride lossy/unordered for better worst-case latency. | Phase 5, together with adaptive bitrate. |
| **Signaling TLS** | Plain `ws://` to the signal server (LAN/tunnel assumption). DTLS protects media regardless; signaling metadata and the secret are cleartext on the wire. | Before any internet-exposed signal server: `wss://` via a reverse proxy. |
| **Encoded-frame drops under extreme backpressure** | The pipeline's watch channel can skip encoded H.264 frames if a consumer stalls outright; the 2 s keyframe interval self-heals visible corruption. Proper fix is per-session lossless queues with pre-encoder backpressure. | Phase 5 (multi-consumer pipelines). |
| **Signaling error UX** | A bad secret surfaces as a retry loop with a generic "signaling closed" status rather than a clear "wrong secret" message. | First UI polish pass. |

## Phase 3 additions

| Decision | Phase 3 choice | Revisit when |
|---|---|---|
| **Clipboard content types** | UTF-8 text only; the wire kind byte reserves room for images/files/RTF. | When someone actually misses it (likely alongside file transfer over tether-bulk). |
| **Cross-channel paste ordering** | Clipboard rides tether-bulk, keystrokes ride tether-ctl: ordering between them is probabilistic. Small pastes are effectively safe; ≥32 KiB pastes delay the V keystroke 150 ms. | Proper fix: a clipboard-applied ack before the keystroke, or one host-side ordered command queue. |
| **Ctrl+V from non-Mac keyboards** | Forwarded raw (Ctrl+V ≠ paste in most macOS apps). The paste *sync* still happens; only the keystroke semantics differ. | Phase 4 device UX: optional ctrl→cmd remapping. |
| **Clipboard history / multi-item** | Last-copy-wins only. | Probably never (out of product scope). |
| **iPad paste affordance** | Hardware-keyboard Cmd+V works; no touch-native paste gesture in the controller UI. | Phase 4 touch UX. |
| **navigator.clipboard on HTTPS** | Auto-write requires a secure context; LAN HTTP serving means the chip is the common path on real devices. | If the controller ever ships with TLS/PWA packaging. |
