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
can_control, audio, files. Unknown types are length-skipped (forward-compat).
Full spec: [protocol.md](protocol.md).

## Host pipeline (tetherd)

```
ScreenCaptureKit / ─► capture thread ─► encoder ─► frames watch ─┐
DXGI Desktop Dup.      (pipeline.rs)    (JPEG /     (broadcast)   │
   (BGRA frames)                         H.264)                   ▼
                                                         per-session sender
controller input ◄─ injector thread ◄── input mpsc ◄── (LAN ws / WebRTC dc)
 (CGEvent / SendInput) (input/*.rs)

WASAPI loopback ─► audio thread ─► Opus ─► broadcast ─► per-peer WebRTC track
                    (audio.rs: resample → 48 kHz stereo, 20 ms frames)
```

- **Capture + encode** run on one dedicated thread (`pipeline.rs`); platform
  handles aren't `Send`. Frames are published on a `watch` channel — latest
  wins, so a slow consumer drops frames rather than buffering. Display switching
  and adaptive-bitrate retuning happen on this thread between frames.
- **Capturers** implement `ScreenCapturer` (`capture.rs`):
  `capture/macos.rs` (ScreenCaptureKit) and `capture/windows.rs` (DXGI Desktop
  Duplication). Duplication only yields a frame when the screen changes, so
  the Windows capturer composites the cursor itself (`capture/windows/
  cursor.rs`) and re-emits the last image while idle, so new controllers and
  H.264 decoders always get a frame; it rebuilds itself on mode changes, the
  secure desktop and access loss. `win.rs` holds shared Windows helpers
  (display ids = GDI device numbers, agreed on by capture and input).
- **Encoders** implement `FrameEncoder` (`capture.rs`): JPEG (turbojpeg, both
  platforms), hardware H.264 on macOS (VideoToolbox, `encode/h264.rs`), and
  software H.264 on Windows (OpenH264, `encode/h264_soft.rs`, fed by the
  BGRA → I420 converter `encode/yuv.rs`). Both H.264 paths emit identical
  Annex B output.
- **Injection** runs on its own thread; all transports funnel
  `InjectCommand`s through one mpsc to it. `input/macos.rs` uses `CGEvent`
  (`CGEventSource` is thread-affine); `input/windows.rs` uses `SendInput`
  (virtual-desktop absolute mouse, scan-code keys, `KEYEVENTF_UNICODE` text).
- **Clipboard**: `clipboard.rs` polls a platform board — `clipboard/macos.rs`
  (NSPasteboard) or `clipboard/windows.rs` (`CF_UNICODETEXT` on its own
  message-loop thread, CRLF ↔ LF at the boundary).
- **System audio** (`audio.rs`, opt-in `--audio`, Windows only so far):
  `audio/windows.rs` captures the default output device via WASAPI loopback
  (following device changes); a dedicated thread resamples to 48 kHz stereo,
  gap-fills silence to keep the clock continuous, encodes Opus and publishes
  on a `broadcast` channel. Each authenticated WebRTC peer subscribes and
  pumps it into its own audio track; laggards skip packets. LAN sessions get
  no audio.
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
  (`packaging/macos`).
- **`macos_perms.rs`**: first-run check of the Screen Recording and
  Accessibility grants — requests the first, opens the pane for the second
  (which never prompts and otherwise fails silently) and logs when it lands.

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
  DTLS/SRTP-encrypted end to end; the signal server sees only SDP/ICE.

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
  `ProtocolSession` (`session.ts`, the transport-agnostic handshake/dispatch)
  plus the `Viewer` (`viewer.ts`, canvas render + pinch-zoom), the input
  adapter (`input.ts` + the clock-injected gesture engine `gestures.ts`, soft
  keyboard `keyboard.ts`), clipboard (`clipboard.ts`) and file transfer
  (`files.ts`: streamed uploads, downloads to disk via the File System Access
  API or an in-memory Blob). Frames decode via WebCodecs/`<img>`
  (`decode.ts`).
- `remap.ts` is the per-host-OS modifier policy: when an Apple client drives
  a Windows host (from `HostInfo.os`), Cmd becomes Ctrl at the single point
  key events leave the controller.
- `controller/public` holds the PWA manifest (`manifest.webmanifest`,
  standalone display) and icons, so the controller installs to a home screen.

## Testing

- Rust: `cargo test` — protocol vectors, auth, the signal relay, the file
  engine, an in-process WebRTC end-to-end (`tests/webrtc_e2e.rs`), and a
  fake-capture pipeline e2e. On macOS, capture/encode/clipboard/injection
  tests need a logged-in GUI session, so the macOS CI job runs only the
  GUI-free crates; the Windows job builds, lints and tests the whole
  workspace with hardware-touching tests `#[ignore]`d (see
  `.github/workflows/ci.yml`).
- Controller: `npm test` (vitest) — protocol parity vectors, input/gesture
  mapping, pairing crypto, signaling JSON, machine reconciliation, modifier
  remap, file transfer.
- Per-phase gate results and deferred decisions live in `docs/`.
