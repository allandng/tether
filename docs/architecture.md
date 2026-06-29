# Tether architecture

A self-hosted remote-desktop system: a macOS host daemon streams its screen to
a browser controller and injects the controller's input, over the LAN directly
or across networks via WebRTC. No cloud, no media relay.

## Components

| Component | Crate / dir | Role |
|---|---|---|
| **Wire protocol** | `crates/tether-protocol` | Length-prefixed binary message types shared by host + controller. Pure, no I/O. Mirrored byte-for-byte in `controller/src/protocol.ts` (cross-pinned test vectors in both). |
| **Host daemon** | `crates/tetherd` | Screen capture, video encode, input injection, clipboard, device-pairing auth, and both transports. macOS-only (Apple frameworks). |
| **Signal server** | `crates/tether-signal` | A presence directory + SDP/ICE relay for WebRTC setup, and minting of ephemeral TURN credentials. Never carries media. Runs on macOS or Linux. |
| **Controller** | `controller/` | Browser viewer/controller (TypeScript + Vite, no framework). Renders frames to a canvas, captures input, drives pairing. |

## Wire protocol

`[u32 LE total_len][u8 msg_type][payload]`. Types: `0x01` Hello, `0x02`
Resolution, `0x03` FrameData, `0x04` InputEvent, `0x05` ClipboardData, `0x06`
TextInput, `0x07–0x0A` pairing/auth, `0x0B` Displays, `0x0C` SelectDisplay.
Unknown types are length-skipped (forward-compat). Full spec:
[protocol.md](protocol.md).

## Host pipeline (tetherd)

```
ScreenCaptureKit ─► capture thread ─► encoder ─► frames watch ─┐
   (BGRA frames)     (pipeline.rs)    (JPEG /     (broadcast)   │
                                       VT H.264)                 ▼
                                                        per-session sender
controller input ◄─ injector thread ◄── input mpsc ◄── (LAN ws / WebRTC dc)
   (CGEvent)         (input/macos.rs)
```

- **Capture + encode** run on one dedicated thread (`pipeline.rs`); platform
  handles aren't `Send`. Frames are published on a `watch` channel — latest
  wins, so a slow consumer drops frames rather than buffering. Display switching
  and adaptive-bitrate retuning happen on this thread between frames.
- **Encoders** implement `FrameEncoder` (`capture.rs`): JPEG (turbojpeg) or
  hardware H.264 (VideoToolbox, `encode/h264.rs`).
- **Injection** runs on its own thread (`CGEventSource` is thread-affine); all
  transports funnel `InjectCommand`s through one mpsc to it.
- **`ServerState`** (`server.rs`) is the shared spine every session clones:
  frame/resolution/clipboard/display `watch` receivers, the input sender, the
  pairing auth, and a `Semaphore` capping concurrent controllers across both
  transports.

## Transports

Both speak the same protocol over the same `ServerState`; only the byte pipe
differs.

- **LAN WebSocket** (`session.rs`): controller → `ws://host:7878`. Hello
  handshake, auth gate, then frames out / input in. IP-allowlisted; one accept
  loop, one task per session holding a permit.
- **WebRTC** (`webrtc.rs` + `tether-signal`): the host registers with the signal
  server and answers offers. Three data channels — `tether-ctl` (Hello/auth/
  Resolution/input, reliable), `tether-media` (frames, chunked at 16 KiB because
  webrtc-rs drops messages >64 KiB), `tether-bulk` (oversized clipboard). Media
  is DTLS-encrypted end to end; the signal server sees only SDP/ICE.

Signaling flow: controller and host both `Register` with the signal server (one
shared `--secret`); the controller sends an `Offer` addressed to the host's
`device_id`; the server relays Offer/Answer/ICE between them; once the data
channels open the signal path is no longer needed.

## Authentication & pairing

After Hello, every controller passes an auth gate (`auth.rs`, shared by both
transports via `handle_auth_message`):

1. Controller sends `Auth` with a stored token; if valid → proceed.
2. Else the host waits for a `PairRequest`. The user types the host's one-time
   pairing code; the controller proves knowledge via
   `HMAC(code, channel_binding)` where `channel_binding = SHA256(sorted DTLS
   fingerprints)` — a malicious relay derives a different binding and fails.
3. On success the host mints `token = HMAC(host_key, device_id || ":" ||
   paired_at)` and adds the device to the allowlist.

The media/bulk pumps stay gated until the control channel authenticates, so an
unpaired peer never receives frames or clipboard.

## Persisted state

- `~/.config/tether/host.key` — 32 random bytes, mode 0600. Signs tokens; never
  transmitted.
- `~/.config/tether/paired.json` — the device allowlist. Delete an entry (or
  the file) to revoke.
- Browser `localStorage` — the controller's per-host device token + identity.

## Controller (browser)

`main.ts` wires a `Transport` (`connection.ts` for WS, `webrtc.ts` for WebRTC)
to a `ProtocolSession` (`session.ts`, the transport-agnostic handshake/dispatch)
plus the `Viewer` (`viewer.ts`, canvas render + pinch-zoom), the input adapter
(`input.ts` + the clock-injected gesture engine `gestures.ts`), and clipboard
(`clipboard.ts`). Frames decode via WebCodecs/`<img>` (`decoder.ts`).

## Testing

- Rust: `cargo test` — protocol vectors, auth, the signal relay, an
  in-process WebRTC end-to-end (`tests/webrtc_e2e.rs`), and a fake-capture
  pipeline e2e. Capture/encode/clipboard/injection tests need a logged-in GUI
  session, so CI runs only the GUI-free crates (see `.github/workflows/ci.yml`).
- Controller: `npm test` (vitest) — protocol parity vectors, input/gesture
  mapping, pairing crypto.
- Per-phase gate results and deferred decisions live in `docs/`.
