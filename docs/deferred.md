# Deferred decisions

Decision points hit during Phase 1 that were out of scope (workflow rule 4):
each got the simplest choice that doesn't block Phase 2.

| Decision | Phase 1 choice | Revisit when |
|---|---|---|
| **H.264 / VideoToolbox** | Ship JPEG. Rust bindings for VideoToolbox are immature (raw FFI, no maintained safe wrapper); JPEG via turbojpeg sustains the ≥15 fps gate (verified by `tests/e2e_fake.rs` and `capture_smoke`), and the protocol already carries a codec id while `FrameEncoder` is a trait, so H.264 is a drop-in encoder + a WebCodecs `VideoDecoder` path in the viewer. | Phase 2 — WebRTC wants real video anyway; bandwidth on WAN makes it necessary, and browsers decode H.264 in hardware via WebCodecs. |
| **Multi-monitor** | Capture the main display only (`CGDisplay::main()` matched against SCK's display list). | Phase 5. |
| **Cursor rendering** | Cursor is composited into the capture by ScreenCaptureKit (`shows_cursor: true`); no client-side cursor layer. Costs nothing; slight ghosting at low fps is acceptable. | Phase 5 (client-drawn cursor removes ghosting and enables instant local feedback). |
| **Multiple controllers** | Second concurrent connection is dropped (tested). | Phase 5, if ever. |
| **Browser-reserved shortcuts** | Cmd+W / Cmd+Q / Cmd+Tab cannot be intercepted by a web page; they act on the controller's browser. All other combos forward correctly. **Phase 12:** on touch devices, the special-keys toolbar sends Esc, Tab, F1–F12 and chords with sticky modifiers. From a hardware keyboard, Cmd+W / Cmd+Q / Cmd+Tab are still the browser's. | Phase 4 — Keyboard Lock API in fullscreen (Chromium) or a native controller shell. |
| **Key auto-repeat** | Browser auto-repeat keydowns are forwarded as repeated KeyDown events (synthetic held keys don't auto-repeat on the host). Repeat *rate* is therefore the controller's, not the host's. | Fine indefinitely. |
| **Caps Lock** | ~~Forwarded as a plain key event; host caps-lock state may not toggle with synthetic events~~ **Closed for macOS hosts in Phase 12.** The controller sends its lock state in modifier bits 4/5, and the macOS host syncs it with `IOHIDSetModifierLockState` (legacy senders: see [Phase 12 additions](#sessions-and-protocol)). **Still open on Windows:** the host injects the key, so it toggles on keydown only. A Mac browser turning the lock off sends a keyup alone, which leaves a Windows host's lock on. | If the Windows case bites: honour bits 4/5 in `input/windows.rs`. |
| **Scroll direction sign** | DOM deltas negated onto CG axes (`wheel1 = -deltaY`), which matches content-follows-finger expectations in testing. macOS "natural scrolling" preference applies on top of injected deltas. **Phase 12:** touch two-finger scroll had the opposite sign to the wheel path; it now matches. | Feel on a real touch device (see real-device gesture tuning). |
| **Clock-skew latency display** | Frame-age readout in the controller status bar is only meaningful when host and controller clocks agree (same machine / NTP-synced LAN). Hidden when implausible. Gate latency criterion is subjective anyway. | Phase 2 can add an RTT echo message. |
| **Capture during display sleep / resolution change** | ~~Display sleep/hot-plug behavior unverified~~ **Handled on macOS in Phase 12.** SCK stream errors rebuild the stream in place (backoff 250 ms → 2 s), falling back to the main display if the captured one is gone. The display mode is polled every 1 s and applied with `update_configuration`. While the display sleeps, the last image repeats; capture resumes ~1.2 s after wake. Resolution changes are still re-announced per frame. | A real display-mode change and hot-unplug on hardware (not yet exercised). |
| **WS connection over HTTPS pages** | Controller is served over plain HTTP in Phase 1 (`ws://` from `http://` is fine; `wss://` would need TLS on tetherd). | Phase 2 (WebRTC removes the need). |

## Phase 2 additions

| Decision | Phase 2 choice | Revisit when |
|---|---|---|
| **Same-secret takeover** | A new offer replaces the active peer session, so a reconnecting controller gets in instantly instead of waiting out ICE timers. Within one shared secret, any device can therefore take over a session. | Real pairing/auth (per-device identity) — Phase 5 hardening. |
| **TURN relay** | STUN only; symmetric-NAT pairs fail to connect with a "peer connection failed" status. | Phase 5 (self-hosted coturn + credentials via the signal server). |
| **Codec negotiation** | Host-side `--codec` flag; the controller is not consulted. A controller without WebCodecs gets a black canvas in h264 mode. | Add a capability bit in `Hello` (e.g. `0b100 = h264-decode`) and per-session encoder choice — needs per-session pipelines first. |
| **Per-codec channel modes** | Media channel is reliable+ordered for both codecs (H.264 requires it). JPEG could ride lossy/unordered for better worst-case latency. | Phase 5, together with adaptive bitrate. |
| **Signaling TLS** | Plain `ws://` to the signal server (LAN/tunnel assumption). DTLS protects media regardless; signaling metadata and the secret are cleartext on the wire. | Before any internet-exposed signal server: `wss://` via a reverse proxy. |
| **Encoded-frame drops under extreme backpressure** | The pipeline's watch channel can skip encoded H.264 frames if a consumer stalls outright; the 2 s keyframe interval self-heals visible corruption. Proper fix is per-session lossless queues with pre-encoder backpressure. **Phase 12:** still latest-only. The controller now notices a gap in the frame numbering and waits for the next keyframe instead of decoding garbage. | Phase 5 (multi-consumer pipelines). |
| **Signaling error UX** | A bad secret surfaces as a retry loop with a generic "signaling closed" status rather than a clear "wrong secret" message. | First UI polish pass. |

## Phase 3 additions

| Decision | Phase 3 choice | Revisit when |
|---|---|---|
| **Clipboard content types** | UTF-8 text only; the wire kind byte reserves room for images/files/RTF. | When someone actually misses it (likely alongside file transfer over tether-bulk). |
| **Cross-channel paste ordering** | Clipboard rides tether-bulk, keystrokes ride tether-ctl: ordering between them is probabilistic. Small pastes are effectively safe; ≥32 KiB pastes delay the V keystroke 150 ms. | Proper fix: a clipboard-applied ack before the keystroke, or one host-side ordered command queue. |
| **Ctrl+V from non-Mac keyboards** | Forwarded raw (Ctrl+V ≠ paste in most macOS apps). The paste *sync* still happens; only the keystroke semantics differ. **Phase 12:** unchanged for hardware keyboards. The touch toolbar's Paste sends the host's own combo (Cmd+V to a Mac, Ctrl+V to Windows). | Phase 4 device UX: optional ctrl→cmd remapping. |
| **Clipboard history / multi-item** | Last-copy-wins only. | Probably never (out of product scope). |
| **iPad paste affordance** | ~~No touch-native paste gesture~~ **Closed in Phase 12.** The special-keys toolbar's Paste reads the local clipboard inside the tap (iOS shows its Paste confirmation), sends it to the host, then sends Cmd/Ctrl+V. Where the read isn't possible, a textarea sheet is the fallback. | — |
| **navigator.clipboard on HTTPS** | Auto-write requires a secure context; LAN HTTP serving means the chip is the common path on real devices. | If the controller ever ships with TLS/PWA packaging. |

## Phase 4 additions

| Decision | Phase 4 choice | Revisit when |
|---|---|---|
| **Android / non-iOS IME** | `beforeinput` harvesting targets iOS Safari; Android IMEs (composition-heavy) are untested. | When Android controllers matter. |
| **Autocorrect replacement accuracy** | ~~Delete count via `getTargetRanges()`, unverified~~ **Closed in Phase 12.** The controller keeps a rolling context of the field and sends a grapheme-aware diff. In the iOS Simulator, German autocorrect "teh" → "geh" arrived as 3× Backspace + `geh` + space. | A real keyboard on a device (still to do). |
| **Real-device gesture tuning** | Thresholds (tap/long-press/slop/pinch) are educated defaults, config-injectable but verified only with synthetic events. **Phase 12:** still open. Exercised in the iOS Simulator only, where the scroll sign and slow-drag rounding were fixed. | After an iPad pass; sweep constants on hardware. |
| **Two-finger-tap centroid** | ~~Right-click lands at the two fingers' centroid~~ **Closed in Phase 12.** It lands at the first finger (direct mode) or the cursor (trackpad mode). | — |
| **Momentum scrolling / haptics / Pencil hover** | ~~No inertia~~ **Momentum done in Phase 12** (τ 325 ms, minimum 0.05 px/ms, at most 2 s). Still no haptics or Pencil hover. | Haptics / Pencil hover: with a real-device pass. |
| **Double-tap-to-reset zoom** | ~~No dedicated reset~~ **Closed in Phase 12** with a "1×" chip that appears while zoomed. Still no reset gesture. | — |
| **ctrl→cmd remap (carried from P3)** | Still not remapped; hardware Ctrl on a non-Mac keyboard isn't translated to Cmd. | A keyboard-settings pass. |
| **Client-drawn cursor** | Trackpad mode shows no local cursor dot; the host cursor (composited into the capture) is the only feedback, so relative-mode aiming lags by one frame. | Phase 5 (client cursor overlay, already deferred there). |
| **iPhone fullscreen** | Fullscreen API is iPad-only on iOS; the ⛶ button hides where unsupported. **Largely addressed in Phase 12.** The compact bar can auto-hide into an overlay, with a pull-tab to bring it back. The machine list nudges iOS tab users to install the home-screen PWA, which runs standalone. | Standalone flows on a real iPhone. |

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
| **Concurrent-input arbitration** | With `--max-controllers >1`, all controllers' input merges into one injector and interleaves; held inputs are released on disconnect but not partitioned per controller. **Phase 12:** macOS hosts now release held keys and modifiers on disconnect too (verified with a real F19). | If shared control gets confusing; add per-session input state or a control token. |
| **Display-switch flapping** | Any controller can switch the shared active display (last-wins); N controllers could fight. | Add a view-owner / arbitration if it bites. |
| **Switch stalls capture thread** | `switch_display` blocks the capture thread on a synchronous SCK reconfigure (rare, user-initiated); a pathological SCK hang would stall frames. | Bound/offload the reconfigure off the frame thread. |
| **displays() double-enumerate on switch** | `switch_display` and the republish each call `SCShareableContent::get()` — minor redundant work on a rare action. | If switching feels slow; return the list from `switch_display`. |

## Post-5b hardening audit (residual, documented)

A holistic audit after Phase 5b fixed the verified robustness/DoS findings (see
git history); these residuals are documented rather than fixed:

| Decision | Current behavior | Revisit when |
|---|---|---|
| **Signal TLS (`ws://`)** | ~~Plain `ws://`~~ — **closed in Phase 6**: the controller derives `wss://` from the page scheme and `deploy/` terminates TLS at Caddy. | — |
| **device_id squatting** | ~~Any holder of the signal secret can register someone else's `device_id`~~ — **closed in Phase 6** by Ed25519 identity pinning at the directory. A first-use race remains (below). | — |
| **Pairing-window interference** | The pairing lockout is host-global (it must be — per-device scoping would let an attacker rotate `device_id` to brute-force the 40-bit code). A secret-holder can therefore burn an *armed* code's attempts and trip the cooldown. Mitigated: failures only count while a code is armed, the cooldown is capped at ~8 min (was ~17 h), and it now logs loudly. | If interference is observed; consider an out-of-band confirm or narrowing who can attempt. |
| **Token lifetime / revocation UX** | ~~Unexpiring tokens, in-process revocation only~~ — **closed in Phase 6**: 90-day TTL bound into the MAC, plus `tetherd devices list/revoke` with mtime-triggered reload. | — |
| **Same-secret takeover** | A new WebRTC offer replaces the active peer; within one signal secret any paired device can take over a session. | If multi-controller arbitration becomes a product concern. |

## Phase 6 additions

| Decision | Phase 6 choice | Revisit when |
|---|---|---|
| **Trust on first use** | The signal server pins `device_id -> pubkey` on the *first* registration it sees. A squatter who reaches a never-registered id first pins their own key and locks the real host out until an operator edits the store. Starting your host once closes the window permanently. | If it matters: pre-seed the identity store from the host's public key out of band. |
| **Identity optional for unpinned controllers** | Required so a browser without WebCrypto Ed25519 (below Safari 17 / Chrome 137 / Firefox 129) can still connect. A secret-holder can therefore claim an unpinned *controller* id — availability only; pairing still gates every host. | When requiring Ed25519 outright is safe. |
| **Browser identity is unrecoverable if cleared** | Clearing `localStorage` mints a new key that the server refuses for the pinned id; recovery means removing the id from `--identity-store` by hand. **Mitigated in Phase 12** by `navigator.storage.persist()` after pairing, and an install hint for iOS tab users. Safari may clear a tab's storage after seven days without a visit; the home-screen app has its own. | Phase 8 — a "forget this device" surface belongs with the machine list. |
| **`--identity-store` is optional** | Without it, pins are in-memory and every restart reopens the first-use window for every device. The server logs a warning; `deploy/` always sets it. | Consider making it mandatory when `--bind` is not loopback. |
| **`tetherd devices` has no IPC** | The CLI edits `paired.json` and the daemon picks it up via mtime. It cannot arm a pairing code — that still needs `--pair` at startup, which a background service has no terminal for. | Phase 9, which needs a real control socket for exactly this reason. |
| **Revocation is not immediate** | Takes effect at the device's next connect; an in-flight session continues. | If immediate disconnection is wanted; the session layer would have to re-check mid-stream. |
| **Token TTL forces one re-pair on upgrade** | Pre-Phase-6 tokens have no expiry segment and are refused outright rather than grandfathered. | Never — grandfathering an unexpiring credential defeats the change. |
| **`native-tls` over rustls for the host's `wss://`** | webrtc-rs already links rustls; adding a second rustls provider risks a process-level provider panic. native-tls also uses the system trust store, which is right for a Let's Encrypt cert on macOS and Windows. | If a Linux host ever appears, native-tls there needs OpenSSL. |

## Phases 7–11 additions

| Decision | Choice | Revisit when |
|---|---|---|
| **Windows capture API** | DXGI Desktop Duplication, cursor composited in software; not Windows.Graphics.Capture (no border on Win10, pull-based, no WinRT). | If a protected-content or per-window capture need appears (WGC handles both). |
| **Windows H.264** | OpenH264 (software, ~12.5 ms/frame at 1080p) instead of the Media Foundation MFT; GPU→CPU readback taken. | A 4K/60 host, or CPU budget matters: add an MF/NVENC `FrameEncoder` and a GPU-resident frame path. |
| **Login-screen / UAC access** | Not supported: logon Scheduled Task in the user session (roadmap option 1). The secure desktop freezes the stream. | If locked-machine access is genuinely missed: session-0 service + per-session host (CRD's split). |
| **Pause / NumLock keys** | Pause unmapped; NumLock sent as plain scan 0x45 (Windows' VK mapping for these two is quirky). | When someone needs them. |
| **macOS system audio** | ~~Not built~~ **Closed in Phase 12** with an audio-only ScreenCaptureKit stream. Its limits are in [Phase 12 additions](#macos-capture-and-audio). | — |
| **A/V sync** | Audio rides RTP, video a data channel; no correlation. Fine for UI sounds/music, visibly off for film. | Only if video moves to an RTP track. |
| **Windows installer format** | PowerShell install/uninstall scripts + zip, not a WiX MSI. Unsigned unless a certificate is supplied. | When distributing beyond your own machines (MSI + EV/OV signing). |
| **File-transfer resume** | A dropped transfer restarts from zero. | If large transfers over flaky links become common. |
| **In-memory download cap** | 100 MB on touch devices, 1 GB elsewhere, unless the browser can stream to disk (File System Access API). | A service-worker streaming download for Safari. |
| **PWA offline shell** | No service worker; the app needs the network anyway. | If Android install prompts or offline launch ever require one. |
| **WebRTC transfer audit label** | Logged as the controller's signal device id (identity-pinned by the signal server), not the paired-device name. | If audit needs the pairing identity: thread it from the ctl auth gate to the bulk channel. |

## Phase 12 additions

Grouped by area. Gate results: [phase12-gate-results.md](phase12-gate-results.md).

### Controller (touch UI)

| Decision | Phase 12 choice | Revisit when |
|---|---|---|
| **Share-sheet save is iOS-only** | By policy, only iOS gets the "Save…" button that opens the share sheet. On iOS, an `<a download>` outside a gesture fails silently. Everywhere else a download is an `<a download>` as before; desktop share dialogs are no substitute. | If another platform turns out to need it. |
| **Toolbar chords bypass the Cmd→Ctrl remap** | The special-keys toolbar's modifiers name the *host's* keys (⌘ on a Mac, Win on Windows), so its chords skip the remap. That is what makes the Win key reachable from an iPhone. A hardware Cmd key from an Apple client still becomes Ctrl against Windows. | If the two behaviours confuse anyone. |
| **Special-keys toolbar assumptions** | Characters typed while a toolbar modifier is latched are mapped back to US-layout key codes (TextInput carries no modifiers). Latched modifiers apply to keys only, not to taps on the remote screen: no Cmd-click from the toolbar. | A non-US layout user, or a need for modifier-clicks. |
| **No auto-repeat on held toolbar keys** | Holding an arrow (or any toolbar key) sends one press. | If someone wants to hold an arrow: a repeat timer. |
| **Newlines in soft-keyboard pastes** | The hidden input is single-line, so newlines in text pasted *into the keyboard field* are stripped. The toolbar's Paste goes through the clipboard and keeps them. | If it bites: a textarea-based field. |

### Sessions and protocol

| Decision | Phase 12 choice | Revisit when |
|---|---|---|
| **No protocol liveness ping** | Nothing in the protocol pings. On resume, the controller judges liveness from the transport's last receive time (`lastRxAt`) and gives a quiet link a 2.5 s grace probe. Current hosts keep sending on a static screen: H.264 encodes the 250 ms idle repeat, and JPEG re-sends the unchanged image once a second. An older host on a static screen sends nothing, so a desktop tab hidden long enough for its timers to stall may reconnect after 2.5 s. | If spurious reconnects show up: a ping/pong message. |
| **Caps Lock legacy-sender heuristic** | Senders that predate modifier bit 5 get the 500 ms rule ([protocol.md](protocol.md)). A Mac user turning the lock on and off within 500 ms, or a held key that hasn't auto-repeated after 500 ms, can leave the two lock states inverted until the next press. | Fades as controllers update (current ones set bit 5). |
| **A token per transport** | Tokens are bound to `paired_at`. The controller stores one per address: `host:port` on the LAN, the device id over WebRTC. Pairing the same controller over the other transport re-pairs it and invalidates the first token, so switching transports means pairing again each time. | If people mix transports: key tokens by host identity, or reuse a valid token across addresses. |
| **No keyframe on join** | A controller that joins mid-stream waits for the next periodic keyframe, up to 2 s (live: first frame 115–1674 ms over 6 WebRTC connects). | If join latency matters: force an IDR when a session subscribes. |
| **SPS rewrite is macOS-only** | `encode/sps.rs` declares no frame reordering, which took Chrome's decode → output from ~204 ms to ~3 ms. It is applied to VideoToolbox output only, not the Windows OpenH264 path. | Measure decoder latency against a Windows host; apply it there if it buffers too. |

### macOS capture and audio

| Decision | Phase 12 choice | Revisit when |
|---|---|---|
| **macOS audio needs an awake display** | ScreenCaptureKit needs a display in the stream's filter, even for audio. With the display asleep it fails ("No capture source provided") and is retried every 2 s. Locked with the display on, it still captures. | Core Audio process taps would capture with the display off. |
| **screencapturekit 7.0.1 crate bugs** | Worked around, not fixed. Dropping *any* `SCStream` clone unregisters the stream's output handlers, so streams are never cloned. The `colorMatrix` setter segfaults because the string is held unretained, so it is left unset; SCK's NV12 is already BT.709 (checked on the first frame). `frame_status()` is always `None`, so frames are recognised by carrying an image buffer. | A crate release that fixes them. |
| **Hardware H.264 size cap** | ~~No size cap on the VideoToolbox encoder~~ **Closed in Phase 12** ([capture review](phase12-gate-results.md#capture-review-fixes)). VideoToolbox silently falls back to software from 4112 px wide (5K ≈ 49 ms/frame, 6K ≈ 83 ms). So `encode::h264::MAX_SIZE` is 4096×2304 on the long side, and SCK scales larger displays down for H.264, keeping aspect and even sides. **Still open:** `DisplayInfo` reports the native pixels, not the capped size. | If a controller needs the streamed size in `DisplayInfo`; a real >4K display to confirm. |
| **Capture-review leftovers** | "Stop Sharing" in the macOS recording indicator pauses capture until a viewer joins or a display is picked. This was tested against the fake backend only, never clicked live. After a display switch, a stale in-flight frame the same size as the new display can still show briefly. | A live Stop Sharing click; the stale frame if it is ever noticed. |

### Signing and Windows builds

| Decision | Phase 12 choice | Revisit when |
|---|---|---|
| **TCC grants pinned to ad-hoc builds** | Without a certificate, tetherd is signed ad hoc (identifier `com.tether.daemon`). macOS ties grants to the exact build, so every new build asks again. | Developer ID signing (grants then survive upgrades). |
| **`._*` entries in the `.pkg` payload** | Built from this development checkout, the package payload carried `._*` AppleDouble provenance entries. Whether they matter on an installed system is unchecked. | Check on a real install. |
| **Windows `RawFrame` change unconfirmed** | `RawFrame` now carries `pixels: Pixels` on every platform (Windows stays `Pixels::Bgra`). The Windows capturer, encoders and examples were adapted but couldn't be compiled for Windows on the Mac. The capture review's `DxgiCapturer` change (re-emitting the same frame `Arc` while idle, so the JPEG idle throttle applies) was type-checked through a Windows-cfg harness only. | The next Windows CI run. |
