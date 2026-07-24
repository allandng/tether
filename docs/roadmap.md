# Roadmap — Chrome Remote Desktop parity

Phases 1–5b built a working remote-desktop engine: WebRTC peer-to-peer with
hardware H.264, device pairing bound to the DTLS channel, TURN credential
minting, adaptive bitrate, multi-monitor, touch UX, clipboard sync.

**The product goal is now explicit:** control a Mac *and* a Windows PC from a
phone, from anywhere — what Chrome Remote Desktop did before it was
discontinued. Measured against that goal, the gap is not the streaming engine.
It is three things: the host is unreachable from the public internet, there is
no Windows host, and setup is a source build plus a hand-written plist.

Phases 6–11 close that gap.

| Phase | Scope | Why |
|---|---|---|
| 6 — Reachable | `wss://` end to end, deployable signal+TURN stack, authenticated signal directory, token revocation CLI | **Blocking.** Nothing below matters if you can't reach the host from outside the LAN. |
| 7 — Windows host | WGC capture, Media Foundation H.264, `SendInput`, Win32 clipboard | The "PC" half of the goal. Largest single chunk. |
| 8 — Machine list | Saved machines with online state, one-tap connect, PWA install | Biggest usability gap vs CRD. Cheaper than it looks — see below. |
| 9 — Packaging | `.pkg` / `.msi`, config file, auto-start at login, guided permission grants | Turns a dev tool into something installable. |
| 10 — Audio | System-audio capture, Opus, a real WebRTC audio track | CRD had it; tether has no audio path at all. |
| 11 — File transfer | Chunked transfer over the existing `tether-bulk` channel | CRD had it; tether syncs text clipboard only. |

Order: 6 → 7 → 8 → 9, then 10 and 11 in either order. Phase 6 is genuinely
blocking. Phases 10 and 11 are features you can ship without.

## What already exists that these phases build on

Worth stating, because three of the six phases are smaller than they sound:

- **The platform seam is already cut.** `ScreenCapturer`, `InputInjector`, and
  `Clipboard` (`capture.rs`, `input.rs`, `clipboard.rs`) are traits with
  `#[cfg(target_os = "macos")]` modules behind them. A second OS is an additive
  impl, not surgery — exactly as `capture.rs` predicted in Phase 1.
- **The protocol is extensible without a version bump.** Unknown `msg_type` is
  length-skipped (`protocol.md`), and `Hello.capabilities` is a `u8` with only
  bits 0–1 used — six free bits for negotiating h264-decode, audio, and file
  transfer.
- **The signal server already maintains a device directory.** It broadcasts a
  full `ServerMessage::Peers` snapshot on every join and leave, and the
  controller already parses it — `signaling.ts` plumbs `onPeers`, and
  `webrtc.ts:153` throws it away. Phase 8's machine list is mostly wiring up
  data that is already arriving.
- **`tether-bulk` already carries oversized payloads** and `chunks.ts` already
  does 16 KiB chunking, which is most of Phase 11's plumbing.

## Cross-cutting: the locked-screen problem

This is the constraint most likely to bite in real use, and it deserves a
decision before Phase 7 rather than after.

`tetherd` needs a **logged-in GUI session**: the window server for capture, and
TCC grants that only exist in a user session. At the macOS login window, or
with the screen locked, it can neither capture nor inject. Chrome Remote
Desktop had the identical limitation on macOS. Windows isolates a locked
session the same way, and additionally puts services in session 0 where they
cannot see the user's desktop at all (see Phase 7).

Connecting while the *display* is asleep works — verified in Phase 1, the first
injected input wakes it. But a machine in real system sleep is simply
unreachable.

Three ways to live with it, in increasing order of cost:

1. **Document it.** Setup instructions tell you to disable auto-lock and set
   "Prevent automatic sleeping" on the machines you want to reach. This is what
   Phases 6–11 assume.
2. **Wake-on-LAN through the signal server.** A registered peer on the same LAN
   as a sleeping host relays a magic packet. Handles sleep, not lock.
3. **Login-screen access.** On Windows this means the session-0 daemon/host
   split that CRD used. On macOS it is not achievable without a launchd
   *daemon* plus a TCC posture that Apple does not grant to third parties.

If option 3 ever matters, it changes the Phase 7 Windows service design, so
decide before building it — not after.

## Verification debt carried forward

Items that are code-complete but unverifiable on the single-Mac development
machine, from the phase gate docs:

- A real two-device WAN run (Phase 2 onward).
- Live TURN relay traversal through a symmetric NAT (Phase 5).
- Multi-display switching on genuine multi-monitor hardware (Phase 5b).
- An iPad/phone pass to tune the gesture constants, which are educated defaults
  verified only against synthetic touch events (Phase 4).

Phase 6 deploys the infrastructure that finally makes the first two testable,
and should discharge them as part of its gate.
