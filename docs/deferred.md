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

## Phase 4 additions

| Decision | Phase 4 choice | Revisit when |
|---|---|---|
| **Android / non-iOS IME** | `beforeinput` harvesting targets iOS Safari; Android IMEs (composition-heavy) are untested. | When Android controllers matter. |
| **Autocorrect replacement accuracy** | `insertReplacementText` deletes the replaced span via `getTargetRanges()` (iOS 17+), falling back to the field length. The exact delete count under real iOS autocorrect is unverified without a device. | iPad pass; if wrong, track the pending word client-side and diff. |
| **Real-device gesture tuning** | Thresholds (tap/long-press/slop/pinch) are educated defaults, config-injectable but verified only with synthetic events. | After an iPad pass; sweep constants on hardware. |
| **Two-finger-tap centroid** | Right-click lands at the two fingers' centroid — can be on empty space if fingers are far apart. | If it bites; use the first finger's point instead. |
| **Momentum scrolling / haptics / Pencil hover** | None — scroll is 1:1 finger travel, no inertia. | Phase 5 polish. |
| **Double-tap-to-reset zoom** | Reset is by pinching back to 1× (clamped); no dedicated reset gesture/button. | If users want it; the engine has no 2-finger-double-tap. |
| **ctrl→cmd remap (carried from P3)** | Still not remapped; hardware Ctrl on a non-Mac keyboard isn't translated to Cmd. | A keyboard-settings pass. |
| **Client-drawn cursor** | Trackpad mode shows no local cursor dot; the host cursor (composited into the capture) is the only feedback, so relative-mode aiming lags by one frame. | Phase 5 (client cursor overlay, already deferred there). |
| **iPhone fullscreen** | Fullscreen API is iPad-only on iOS; the ⛶ button hides where unsupported. | If a chromeless iPhone experience is needed (add-to-home-screen PWA). |

## Phase 5 additions

| Decision | Phase 5 choice | Revisit when |
|---|---|---|
| **Live TURN relay traversal** | Code-complete (signal mints coturn creds; host + controller apply them) but unverifiable on one machine. | Real coturn + symmetric-NAT WAN test. |
| **Pairing token rotation** | Tokens are long-lived bearer credentials bound to `device_id\|paired_at`; no automatic expiry. Rotate by re-pairing (a new `paired_at` ⇒ new token). | If token theft becomes a concern; add a TTL/refresh. |
| **device_id squatting DoS** | A peer past the signal `--secret` can register someone else's `device_id` and evict them. Pairing prevents impersonation, not this availability hit. | Phase 5b — authenticate the signal directory, not just relay. |
| **Signal-server TLS** | Plain `ws://`; the relay sees SDP/ICE (not media). Pairing resists an active MITM, but signaling metadata + the coarse secret are cleartext. | Before any internet-exposed signal server: `wss://` via a reverse proxy. |
| **AIMD seed-from-last + skip-for-JPEG** | The controller re-pins to the ceiling each session and the AIMD loop runs (harmlessly) even for JPEG. | Polish: seed from the last converged target; skip the loop when codec≠h264. |
| **multi-monitor / client-drawn cursor / multiple controllers** | Carried over from the original Phase 5 scope; the secure-internet slice deferred them. | Phase 5b. |

## Phase 5b additions

| Decision | Phase 5b choice | Revisit when |
|---|---|---|
| **Live multi-display switching** | Code-complete (enumerate + live `update_content_filter` switch + active-display input mapping) but unverifiable on a one-display machine. | A real multi-monitor Mac. |
| **Concurrent-input arbitration** | With `--max-controllers >1`, all controllers' input merges into one injector and interleaves; held inputs are released on disconnect but not partitioned per controller. | If shared control gets confusing; add per-session input state or a control token. |
| **Display-switch flapping** | Any controller can switch the shared active display (last-wins); N controllers could fight. | Add a view-owner / arbitration if it bites. |
| **Switch stalls capture thread** | `switch_display` blocks the capture thread on a synchronous SCK reconfigure (rare, user-initiated); a pathological SCK hang would stall frames. | Bound/offload the reconfigure off the frame thread. |
| **displays() double-enumerate on switch** | `switch_display` and the republish each call `SCShareableContent::get()` — minor redundant work on a rare action. | If switching feels slow; return the list from `switch_display`. |

## Post-5b hardening audit (residual, documented)

A holistic audit after Phase 5b fixed the verified robustness/DoS findings (see
git history); these residuals are documented rather than fixed:

| Decision | Current behavior | Revisit when |
|---|---|---|
| **Signal TLS (`ws://`)** | Plain `ws://`; the `--secret` and SDP/ICE are cleartext on the wire. DTLS still encrypts media and the fingerprint binding still defeats an active media MITM. | Before exposing the signal server beyond a trusted tunnel/LAN: terminate `wss://` at a reverse proxy. |
| **device_id squatting** | Any holder of the signal secret can register someone else's `device_id` and evict the live host (availability only — pairing still prevents impersonation). | Authenticate the signal directory per device, not just the relay. |
| **Pairing-window interference** | The pairing lockout is host-global (it must be — per-device scoping would let an attacker rotate `device_id` to brute-force the 40-bit code). A secret-holder can therefore burn an *armed* code's attempts and trip the cooldown. Mitigated: failures only count while a code is armed, the cooldown is capped at ~8 min (was ~17 h), and it now logs loudly. | If interference is observed; consider an out-of-band confirm or narrowing who can attempt. |
| **Token lifetime / revocation UX** | Device tokens are long-lived bearer credentials in browser `localStorage`; revocation exists (`PairingAuth::revoke`) but only in-process — no CLI/IPC to list/revoke without a restart. | Add a `tetherd devices list/revoke` surface + a token TTL before broad internet exposure. |
| **Same-secret takeover** | A new WebRTC offer replaces the active peer; within one signal secret any paired device can take over a session. | If multi-controller arbitration becomes a product concern. |
