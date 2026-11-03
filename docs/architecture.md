# Tether architecture

A self-hosted remote-desktop system: a host daemon on a macOS or Windows
machine streams its screen (and optionally its system audio) to a browser
controller and injects the controller's input, over the LAN directly or across
networks via WebRTC. Files move both ways. No cloud, no media relay.

## Components

| Component | Crate / dir | Role |
|---|---|---|
| **Wire protocol** | `crates/tether-protocol` | Length-prefixed binary message types shared by host + controller. Pure, no I/O. Mirrored byte-for-byte in `controller/src/protocol.ts` (cross-pinned test vectors in both). |
| **Host daemon** | `crates/tetherd` | Screen capture, video encode, input injection, clipboard, system audio, file transfer, device-pairing auth, and both transports. macOS (Apple frameworks) and Windows (DXGI / `SendInput` / WASAPI). |
| **Signal server** | `crates/tether-signal` | A presence directory + SDP/ICE relay for WebRTC setup, and minting of ephemeral TURN credentials. Never carries media. Runs on macOS or Linux. |
| **Controller** | `controller/` | Browser viewer/controller (TypeScript + Vite, no framework), installable as a PWA. Machine list, canvas rendering, input capture, pairing, file transfer, audio playback. |

## Wire protocol

`[u32 LE total_len][u8 msg_type][payload]`. Types: `0x01` Hello, `0x02`
Resolution, `0x03` FrameData, `0x04` InputEvent, `0x05` ClipboardData, `0x06`
TextInput, `0x07–0x0A` pairing/auth, `0x0B` Displays, `0x0C` SelectDisplay,
`0x0D` HostInfo, `0x0E–0x12` file transfer (FileOffer / FileAccept /
FileChunk / FileEnd / FileRequest). Hello capability bits: can_host,
can_control, audio, files. Key events' modifier byte carries
shift/ctrl/alt/meta in bits 0–3 and the controller's Caps Lock state in
bits 4–5. Unknown types are length-skipped (forward-compat).
Full spec: [protocol.md](protocol.md).

## Host pipeline (tetherd)

```
ScreenCaptureKit / ─► capture thread ─► encoder ─► frames watch ─┐
DXGI Desktop Dup.      (pipeline.rs)    (JPEG /     (broadcast)   │
 (IOSurface pixel                        H.264)                   ▼
  buffers / BGRA)                                        per-session sender
controller input ◄─ injector thread ◄── input mpsc ◄── (LAN ws / WebRTC dc)
 (CGEvent / SendInput) (input/*.rs)

SCK audio-only stream / ─► audio thread ─► Opus ─► broadcast ─► per-peer WebRTC track
WASAPI loopback             (audio.rs: resample → 48 kHz stereo, 20 ms frames)
```

- **Capture + encode** run on one dedicated thread (`pipeline.rs`); platform
  handles aren't `Send`. Frames are published on a `watch` channel — latest
  wins, so a slow consumer drops frames rather than buffering. Display switching
  and adaptive-bitrate retuning happen on this thread between frames. A
  capturer that can't be built (no display, no Screen Recording grant) or fails
  mid-stream is retried with backoff 1 → 3 s rather than ending the daemon.
  Until capture works, the published resolution is 0×0. JPEG re-sends an
  unchanged image (an idle repeat) at most once a second. H.264 encodes every
  repeat, which carries its keyframe cadence.
- **Frames** are `RawFrame { width, height, pixels: Pixels, … }`
  (`capture.rs`), shared rather than copied so an idle repeat costs nothing.
  On macOS, `Pixels::PixelBuffer` is the IOSurface-backed `CVPixelBuffer`
  that ScreenCaptureKit delivered, retained as-is (`capture/pixel_buffer.rs`):
  NV12 (`420v`) when the encoder is H.264, BGRA for JPEG. Windows frames stay
  `Pixels::Bgra` bytes.
- **Capturers** implement `ScreenCapturer` (`capture.rs`):
  `capture/macos.rs` (ScreenCaptureKit) and `capture/windows.rs` (DXGI Desktop
  Duplication). Neither source yields frames while the screen is static, so
  both re-emit the last image every 250 ms. New controllers and H.264
  decoders always get a frame.
  - **macOS frames:** SCK pushes frames into a latest-wins mailbox (queue
    depth 6, at most 3 buffers retained at once).
  - **macOS recovery:** a stream error reported to the SCK delegate rebuilds
    the stream in place with backoff 250 ms → 2 s, falling back to the main
    display if the captured one is gone. The last image keeps repeating
    meanwhile. The display mode is re-read every 1 s and applied with
    `update_configuration`. A watchdog (armed at start and after every
    rebuild) rebuilds a stream that sends no frame of the configured size for
    3 s.
  - **macOS displays:** `active_display()` reports which display is really
    being captured. The pipeline republishes the display list whenever it
    changes, so a fallback never leaves input aimed at the old display. If
    the user clicks Stop Sharing in the macOS recording indicator, capture
    pauses (black frames, no rebuild) until a viewer joins or a display is
    picked.
  - **macOS size cap:** for H.264, displays larger than the hardware
    encoder's 4096×2304 (`encode::h264::MAX_SIZE`) are scaled down by SCK,
    keeping the aspect ratio (`fit_within`). Pointer mapping is unaffected,
    because input is normalised onto the display bounds.
  - **Windows:** the capturer composites the cursor itself
    (`capture/windows/cursor.rs`), re-emits the same frame `Arc` while nothing
    changes, and rebuilds itself on mode changes, the secure desktop and
    access loss. `win.rs` holds shared Windows helpers (display ids = GDI
    device numbers, agreed on by capture and input).
- **Encoders** implement `FrameEncoder` (`capture.rs`). Both H.264 paths emit
  identical Annex B output.
  - **JPEG** (turbojpeg, both platforms) reads a BGRA pixel buffer in place.
  - **Hardware H.264 on macOS** (VideoToolbox, `encode/h264.rs`): the NV12
    IOSurface goes straight into `VTCompressionSessionEncodeFrame`, with no
    CPU copy. Keyframes are by time: `MaxKeyFrameIntervalDuration` 2 s plus a
    forced-IDR backstop.
  - **macOS SPS and teardown:** BT.709 is signalled in the SPS.
    `encode/sps.rs` also rewrites the SPS to declare no frame reordering, so
    decoders output each frame at once. Sessions are invalidated before
    they're dropped and frames are tagged, so a late output is discarded.
  - **Software H.264 on Windows** (OpenH264, `encode/h264_soft.rs`), fed by
    the BGRA → I420 converter `encode/yuv.rs`.
- **Injection** runs on its own thread; all transports funnel
  `InjectCommand`s through one mpsc to it.
  - **macOS keys:** `input/macos.rs` uses `CGEvent` (`CGEventSource` is
    thread-affine). Modifier flags come from the set of keys it holds, all
    released on disconnect. Arrows, F-keys, Home/End/PgUp/PgDn,
    forward-delete and the keypad carry the SecondaryFn/NumericPad flags.
    Media and volume keys are NSEvent `systemDefined` (subtype 8) events.
  - **macOS Caps Lock** is a HID lock state, synced from modifier bits 4/5
    with `IOHIDSetModifierLockState` (see [protocol.md](protocol.md)).
  - **macOS pointer and power:** display bounds are cached for at most
    250 ms and re-read on mouse-down and display switch. Input declares one
    reused IOPM user-activity assertion, so it wakes the display.
  - **Windows:** `input/windows.rs` uses `SendInput` (virtual-desktop
    absolute mouse, scan-code keys, `KEYEVENTF_UNICODE` text).
- **Clipboard**: `clipboard.rs` polls a platform board — `clipboard/macos.rs`
  (NSPasteboard, each call in an autorelease pool) or `clipboard/windows.rs`
  (`CF_UNICODETEXT` on its own message-loop thread, CRLF ↔ LF at the
  boundary).
- **System audio** (`audio.rs`, opt-in `--audio`). LAN sessions get no audio.
  - **Windows:** `audio/windows.rs` captures the default output device via
    WASAPI loopback, following device changes.
  - **macOS:** `audio/macos.rs` runs an audio-only ScreenCaptureKit stream: a
    2×2, 1 fps video nothing consumes plus the Audio output, with its own
    process excluded. It needs the Screen Recording grant and no microphone
    permission. It interleaves SCK's Float32 planar buffers. Its blocking SCK
    calls run on helper threads with deadlines, and a 5 s no-buffer watchdog
    rebuilds a stuck stream.
  - **Encode:** a dedicated thread resamples to 48 kHz stereo, gap-fills
    silence to keep the clock continuous, encodes Opus and publishes on a
    `broadcast` channel. A capturer that dies within 10 s of starting backs
    off 2 → 16 s.
  - **Delivery:** each authenticated WebRTC peer subscribes and pumps it into
    its own audio track; laggards skip packets.
- **File transfer** (`files.rs`): one `FileTransfers` engine per controller
  session. Uploads land in the upload dir under a sanitised name (temp file,
  renamed only when complete); downloads stream any host path the controller
  names, backpressured by a bounded chunk queue.
- **`ServerState`** (`server.rs`) is the shared spine every session clones:
  frame/resolution/clipboard/display `watch` receivers, the input sender, the
  pairing auth, the host name (for `HostInfo`), the file config, the audio
  feed, and a `Semaphore` capping concurrent controllers across both
  transports.

## Running as a service

- **`config.rs`**: every flag can also live in `config.toml` in the state
  directory; command-line flags override the file, the file overrides
  defaults. `tetherd setup` writes the signal server + secret (owner-readable
  only), so a service never carries the secret on its command line.
  `tetherd devices list|revoke` edits the allowlist.
- **`control.rs`**: a local control channel to the running daemon —
  `tetherd pair` arms a pairing code and prints it, `tetherd status` reports
  transports, controllers and displays. A named pipe
  (`\\.\pipe\tether-control-<user>`, user + SYSTEM DACL, no remote clients) on
  Windows; `<state dir>/control.sock` (0600, peer-uid checked) on Unix. One
  JSON request line, one JSON response line.
- **`bin/tetherd-agent.rs`**: the Windows logon launcher. The "Tether Host"
  Scheduled Task (`packaging/windows/install.ps1`) runs this windowless
  GUI-subsystem program at sign-in, in the user's desktop session; it starts
  `tetherd.exe` with no console, logs to `%LOCALAPPDATA%\Tether\tetherd.log`,
  and restarts it with backoff. macOS uses a LaunchAgent
  (`packaging/macos`) that restarts tetherd only after a failure, at most
  every 30 s.
- **`macos_perms.rs`**: startup check of the Screen Recording and
  Accessibility grants.
  - **Requests:** missing grants are requested with
    `CGRequestScreenCaptureAccess`, `AXIsProcessTrustedWithOptions` (with the
    prompt option) and `CGRequestPostEventAccess`. These show the system
    prompts and list tetherd in the panes. Then the Accessibility pane opens,
    and a watcher logs when that grant lands.
  - **Frequency:** it prompts **once per (binary fingerprint, launch
    context)**. The launch contexts are launchd, ssh, a terminal, an app or a
    job, because TCC charges a request to the responsible process.
  - **Marker:** `<state dir>/macos-permissions-prompted`, falling back to
    `~/Library/Caches/tether`, then `$TMPDIR/tether-<uid>`; symlinks refused.
    It is written *before* prompting, so a LaunchAgent restart loop can't
    become a prompt loop. If no marker can be written, it doesn't prompt at
    all.
  - **Status:** `tetherd status` reports both grants and whether the upload
    folder is usable (probed at startup on a background thread; cached 30 s,
    one probe in flight).
  - **Info.plist:** `build.rs` embeds one (`com.tether.daemon`, usage
    strings) in the binary, so the prompts and panes have a name to show.

## Transports

Both speak the same protocol over the same `ServerState`; only the byte pipe
differs.

- **LAN WebSocket** (`session.rs`): controller → `ws://host:7878`. Hello
  handshake, auth gate, then frames out / input in. File messages share the
  one socket. IP-allowlisted; one accept loop, one task per session holding a
  permit.
- **WebRTC** (`webrtc.rs` + `tether-signal`): the host registers with the signal
  server and answers offers. Three data channels — `tether-ctl` (Hello/auth/
  Resolution/HostInfo/input, reliable), `tether-media` (frames, chunked at
  16 KiB because webrtc-rs drops messages >64 KiB), `tether-bulk` (clipboard
  and file transfer, chunked, one writer so each message's chunks go out back
  to back). Plus an Opus audio track when the host runs with `--audio` and the
  controller's offer has its `recvonly` audio m-line. Media is
  DTLS/SRTP-encrypted end to end; the signal server sees only SDP/ICE. The
  session ends when the peer connection fails or closes (~30 s after a
  controller vanishes), releasing its controller slot.

Signaling flow: controller and host both `Register` with the signal server (one
shared `--secret`; hosts also send their `os`, echoed in the `Peers`
directory); the controller sends an `Offer` addressed to the host's
`device_id`; the server relays Offer/Answer/ICE between them; once the data
channels open the signal path is no longer needed. Separately, the controller
holds a second, long-lived registration under `<id>-dir` purely to watch the
directory (see below).

## Authentication & pairing

After Hello, every controller passes an auth gate (`auth.rs`, shared by both
transports via `handle_auth_message`):

1. Controller sends `Auth` with a stored token; if valid → proceed.
2. Else the host waits for a `PairRequest`. The user types the host's one-time
   pairing code (`tetherd pair`, or `--pair` at startup); the controller proves
   knowledge via `HMAC(code, channel_binding)` where `channel_binding =
   SHA256(sorted DTLS fingerprints)` — a malicious relay derives a different
   binding and fails.
3. On success the host mints `token = HMAC(host_key, device_id || ":" ||
   paired_at)` and adds the device to the allowlist.

An armed pairing code turns the gate on, even for a host with no paired
devices yet. A refused controller gets 90 s to enter the code on either
transport.

The media/bulk pumps and the audio track stay gated until the control channel
authenticates, so an unpaired peer never receives frames, clipboard, audio or
files, and cannot write to the clipboard or disk.

## Persisted state

- **State directory**: `%APPDATA%\tether` on Windows, `~/.config/tether` on
  macOS. Holds:
  - `host.key` — 32 random bytes, owner-only. Signs tokens; never transmitted.
  - `paired.json` — the device allowlist. `tetherd devices revoke`, or delete
    an entry (or the file), to revoke.
  - `config.toml` — settings written by `tetherd setup` (or by hand).
- **Logs** (as a service): `%LOCALAPPDATA%\Tether\tetherd.log` on Windows,
  `~/Library/Logs/Tether/tetherd.log` on macOS.
- **Uploads**: the user's Downloads folder, or `--upload-dir`.
- Browser `localStorage` — the controller's settings (signal server, secret,
  name), saved machines, per-host device tokens and identity.

## Controller (browser)

`main.ts` is the app shell: two views over one small state object — the
**machine list** (saved machines with live online/offline state, one tap to
connect, "connect by address" for LAN, ⚙ Settings for signal server + secret,
the server defaulting to the page's own origin) and the **session** (remote
screen plus display picker, clipboard, files, keyboard, pointer mode,
fullscreen, back).

- `directory.ts` keeps a persistent signal registration under `<id>-dir` for
  as long as the app is open, reconnecting with backoff and on visibility /
  network changes, so the list knows who is online without connecting.
- `machines.ts` reconciles saved machines (localStorage) with the live
  directory into list rows (pure, unit-tested).
- A session wires a `Transport` (`connection.ts` for WS, `webrtc.ts` for
  WebRTC, which also adds the `recvonly` audio transceiver) to a
  `ProtocolSession` (`session.ts`, the transport-agnostic handshake/dispatch),
  plus:
  - the `Viewer` (`viewer.ts`, canvas render + pinch-zoom);
  - the input adapter: `input.ts` + the clock-injected gesture engine
    `gestures.ts`, with momentum scrolling and edge auto-pan;
  - the soft keyboard `keyboard.ts`, which takes the keys iOS reports only as
    `keydown` and sends autocorrect as a grapheme-aware diff against a
    rolling context;
  - clipboard (`clipboard.ts`, including the touch Paste button and its
    fallback sheet) and file transfer (`files.ts`: streamed uploads,
    downloads to disk via the File System Access API or an in-memory Blob);
  - frame decode via WebCodecs/`<img>` (`decode.ts`). Every new connection
    gets a fresh H.264 decoder that waits for a keyframe, and so does a gap in
    the host's frame numbering.
- `remap.ts` is the per-host-OS modifier policy: when an Apple client drives
  a Windows host (from `HostInfo.os`), Cmd becomes Ctrl at the single point
  key events leave the controller.
- Touch-device layout:
  - `extrakeys.ts` is the special-keys toolbar: Esc, Tab, arrows, F1–F12,
    and sticky Ctrl/⌥/⌘/⇧ with explicit chord presses. Its modifiers name
    host keys, so its chords skip the remap.
  - `viewport.ts` mirrors `visualViewport` into CSS variables, so the session
    letterboxes above the on-screen keyboard.
  - `relayout.ts` coalesces rotation, resize and keyboard changes into one
    re-fit of the zoom pan and cursor.
  - `sessionbar.ts` switches to a one-row bar with a ⋯ sheet on a coarse
    pointer or ≤600 px. It has an optional auto-hide overlay whose timing is
    the pure `autohide.ts`.
- App lifecycle:
  - `lifecycle.ts` spots resumes (timer gaps, visibility, `online`,
    `pageshow`). From the transport's last receive time plus a 2.5 s grace
    probe, it decides whether the session survived or reconnects.
  - `wakelock.ts` holds a Screen Wake Lock while connected. It is
    re-requested on user activation, the only time iOS grants it.
  - `save.ts` hands finished downloads to the iOS share sheet ("Save…") or an
    `<a download>`.
  - `pwa.ts` covers iOS home-screen concerns: the install hint, and
    `navigator.storage.persist()` after pairing.
- `controller/public` holds the PWA manifest (`manifest.webmanifest`,
  standalone display) and icons, so the controller installs to a home screen.

## Testing

- Rust: `cargo test` — protocol vectors, auth, the signal relay, the file
  engine, an in-process WebRTC end-to-end (`tests/webrtc_e2e.rs`), and a
  fake-capture pipeline e2e. Tests that touch real hardware (key presses,
  ScreenCaptureKit audio, the loopback device, benchmarks) are `#[ignore]`d.
  CI is in `.github/workflows/ci.yml`:
  - **macOS job:** builds and lints every target. Runs tetherd's GUI-free
    unit tests by module allowlist, plus the `connection` and `webrtc_e2e`
    integration tests.
  - **Not in the macOS job:** VideoToolbox (`encode::h264::`) and pasteboard
    tests need a logged-in GUI session and run locally. `e2e_fake`'s fps gate
    is too CPU-bound for the hosted macOS VM in a debug build.
  - **Windows job:** builds, lints and tests the whole workspace.
- Controller: `npm test` (vitest) — protocol parity vectors, input/gesture
  mapping, soft-keyboard diffing, the special-keys toolbar, lifecycle and wake
  lock, pairing crypto, signaling JSON, machine reconciliation, modifier
  remap, file transfer.
- `crates/tetherd/examples/fake_host.rs` serves the LAN transport with a
  synthetic screen and logs input as JSON instead of injecting it, for driving
  the controller (e.g. from the iOS Simulator) without touching the machine.
- Per-phase gate results and deferred decisions live in `docs/`.
