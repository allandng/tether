# Phase 1 Plan — LAN MVP

**Goal:** From a controller machine, view the host (macOS) screen live and control its mouse + keyboard, over a hardcoded IP:port on a trusted LAN.

This document covers: key decisions with justifications, module breakdown, crate/library choices, and build order. No code exists yet; nothing below is implemented.

---

## 1. Key decisions

### 1.1 Controller: browser-based, TypeScript

The brief asks for "a minimal desktop or browser-based viewer running on iOS." A browser-based controller is the only choice that satisfies both readings:

- Runs unchanged in a desktop browser (where Phase 1 development and the gate tests happen) and in iOS Safari (iPad with trackpad + hardware keyboard can exercise the mouse/keyboard gate criteria; full touch UX is Phase 4).
- No Rust GUI toolkit dependency — Rust desktop GUI is the weakest part of the ecosystem and would be throwaway work once Phase 4 mobile UX arrives.
- Browser `<canvas>` + Pointer Events + WebSocket is a complete, well-trodden viewer stack.
- Forward-compatible: Phase 2 WebRTC is *native* to browsers (no extra dependency), and WebCodecs `VideoDecoder` gives us hardware H.264 decode in Safari/Chrome when we upgrade the codec.

Plain TypeScript + Vite, no framework — it's one canvas and an event loop.

### 1.2 Transport: WebSocket (over TCP)

Forced by the browser controller — browsers can't open raw TCP sockets. WebSocket gives us message framing, ping/pong keepalive, and clean close semantics for free. The *protocol* stays transport-agnostic (see 1.3); WebSocket is just Phase 1's carrier, as WebRTC data channels will be Phase 2's.

### 1.3 Wire protocol: hand-rolled length-prefixed binary (not msgpack)

Every message is:

```
[ u32 length (LE) ][ u8 msg_type ][ payload bytes ]
```

with fixed, documented payload layouts per type (full spec to live in `docs/protocol.md`). A `Hello` exchange carries a `u16 protocol_version` for versioning.

Justification over msgpack:

- The hot path is `FrameData` — one large opaque byte blob per message. msgpack would wrap it in envelope encoding for zero benefit; with a fixed layout the frame payload is a zero-copy slice on both ends.
- The message set is four types with small, stable fields. Hand-rolled encode/decode is ~200 lines total across Rust and TS, fully round-trip tested, with no dependency on either side (no msgpack decoder shipped to the browser).
- Schema evolution is handled by the version field + the rule "unknown message types are skipped using the length prefix" — sufficient for a protocol we control on both ends.

Transport-agnostic by construction: the length prefix makes messages self-delimiting on a raw byte stream (TCP), while on message-oriented transports (WebSocket binary frames now, WebRTC data channels in Phase 2) we send exactly one protocol message per transport message and the length prefix is redundant but harmless. Nothing in the protocol crate knows about WebSocket.

### 1.4 Message types (Phase 1 complete set)

| Type | Direction | Contents |
|---|---|---|
| `Hello` | both, once at connect | protocol version, role (host/controller), capability flags (`can_host`, `can_control`) |
| `Resolution` | host → controller | capture width/height in pixels (re-sent if it changes) |
| `FrameData` | host → controller | codec id (`jpeg` / `h264`), sequence number, capture timestamp, encoded frame bytes |
| `InputEvent` | controller → host | tagged union: mouse move / button down/up / scroll / key down/up, with modifier bitmask |

`Hello` carries the capability flags from day one so the asymmetric-roles model is in the protocol's DNA, but Phase 1 hosts only ever act as host and controllers as controller. A mobile build simply never links the host code path — `can_host` is a protocol fact, not a feature flag in the host implementation.

### 1.5 Coordinates: normalized fixed-point

Mouse positions travel as `u16` x/y normalized to `0..=65535` over the capture area. The controller divides by its canvas display size; the host multiplies by its capture resolution. This makes resolution mapping (a gate criterion) a protocol property rather than client math, handles Retina scale factors for free, and survives host resolution changes mid-session.

### 1.6 Video codec: JPEG first, H.264 attempt second

Per the brief's explicit permission: the pipeline ships first with a JPEG frame stream (hardware-accelerated decode not needed; `drawImage(ImageBitmap)` is fast). Encoding sits behind an `Encoder` trait so VideoToolbox H.264 can drop in without touching capture or transport. After the full pipeline passes a smoke test end-to-end, I'll timebox a VideoToolbox attempt; if it rabbit-holes, JPEG ships Phase 1 and H.264 moves to `docs/deferred.md`. Gate math says JPEG is viable: turbojpeg encodes a ~6 MP Retina frame in ~15–25 ms — enough for 15+ fps, with bandwidth (~5–15 MB/s) fine on LAN.

### 1.7 Security floor

- `--bind <ip>` is **required** (no default). `tetherd` resolves local interface addresses at startup and refuses to bind anything that isn't a loopback or RFC 1918 private address — `0.0.0.0` and public addresses are rejected with an error.
- `--allow <ip>` is **required**; connections from any other peer address are dropped before the protocol handshake (repeatable flag for multiple controllers).
- macOS will additionally demand Screen Recording and Accessibility permissions for the `tetherd` binary — documented in the README, granted once per machine.

---

## 2. Repo layout

```
tether/
├── Cargo.toml              # workspace
├── crates/
│   ├── tether-protocol/    # wire types + encode/decode (transport-agnostic, dependency-light)
│   └── tetherd/            # host daemon (macOS)
├── controller/             # TypeScript browser viewer (Vite)
└── docs/
    ├── phase1-plan.md      # this file
    ├── protocol.md         # wire protocol spec (written with module 1)
    └── deferred.md         # scope decisions punted per workflow rule 4
```

### Trait boundaries inside `tetherd`

```rust
trait ScreenCapturer {            // macOS impl Phase 1; second OS later is additive
    fn resolution(&self) -> (u32, u32);
    fn next_frame(&mut self) -> Result<RawFrame>;   // BGRA pixels + timestamp
}

trait FrameEncoder {              // JPEG impl Phase 1; VideoToolbox H.264 drop-in
    fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame>;
}

trait InputInjector {             // macOS impl Phase 1
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}
```

Capture/encode runs on its own thread feeding a `tokio` channel; the connection task multiplexes frames out and input events in. One controller connection at a time in Phase 1 (a second connection is rejected; logged in deferred.md).

---

## 3. Crate / library choices

| Dependency | Where | Justification (one line each) |
|---|---|---|
| `tokio` | tetherd | De facto async runtime; needed for concurrent frame-out/input-in on one connection. |
| `tokio-tungstenite` | tetherd | The standard maintained WebSocket impl for tokio. |
| `bytes` | protocol, tetherd | Zero-copy buffers for frame payloads. |
| `screencapturekit` | tetherd | Maintained Rust bindings to ScreenCaptureKit, Apple's current (non-deprecated) capture API; unsafe lives inside the binding crate. Fallback if its API fights us: `core-graphics` `CGDisplayStream`. |
| `turbojpeg` | tetherd | SIMD libjpeg-turbo bindings; pure-Rust encoders are ~5–10× too slow for 15 fps at Retina native resolution. |
| `enigo` | tetherd | Well-maintained cross-platform input injection (CGEvent under the hood on macOS); keeps unsafe out of our code. Fallback for key-mapping gaps: direct `core-graphics` CGEvent. |
| `clap` | tetherd | Standard CLI parsing for `--bind` / `--allow` / `--port`. |
| `tracing` + `tracing-subscriber` | tetherd | Structured logging. |
| `thiserror` / `anyhow` | protocol / tetherd | Idiomatic error types at lib/bin boundaries respectively. |
| Vite + TypeScript | controller | Zero-config dev server + types; no runtime framework needed for one canvas. |

`tether-protocol` depends only on `bytes` + `thiserror` so it can sit on any transport (and compile fast).

---

## 4. Build order (each step ends with a compile + smoke test + your review gate)

**Module 0 — Scaffolding.** `git init`, cargo workspace, empty crates, controller skeleton, CI-less `cargo test` green. *Smoke test: workspace builds.*

**Module 1 — `tether-protocol`.** All four message types, encode/decode in Rust, `docs/protocol.md` written. *Tests: round-trip for every message type, truncated-input rejection, unknown-type skip, version mismatch detection.*

**Module 2 — `tetherd` connection layer.** WebSocket server, `--bind`/`--allow`/`--port` enforcement (bind-address validation, peer filtering), `Hello` handshake, clean disconnect → returns to listening (gate criterion: reconnect without restart). No capture yet. *Smoke test: Rust integration test connects, handshakes, disconnects, reconnects; disallowed IP and bad version are rejected.*

**Module 3 — Capture + encode.** `ScreenCapturer` (ScreenCaptureKit) + `FrameEncoder` (turbojpeg) behind their traits, capture thread, frames streamed as `FrameData` after handshake. *Smoke test: test client receives ≥15 fps for 5 s and writes a decodable JPEG to disk; unit test encodes a synthetic frame.*

**Module 4 — Controller viewer.** TS protocol encode/decode (mirroring module 1, with its own round-trip tests via vitest), WebSocket client, canvas rendering with aspect-fit scaling, connect/disconnect UI state. *Smoke test: live host screen visible in browser; fps counter overlay reads ≥15.*

**Module 5 — Input path.** Controller captures pointer/wheel/key events over the canvas (with `preventDefault` so browser shortcuts don't fire), normalizes coordinates, sends `InputEvent`; `InputInjector` (enigo) injects on host, including modifier combos. *Smoke test: move/click/type/shift-ctrl-cmd combos land correctly on host; unit test for coordinate mapping at mismatched resolutions.*

**Module 6 — Gate hardening.** H.264/VideoToolbox timeboxed attempt (drop-in via `FrameEncoder`), reconnect robustness, latency measurement via frame timestamps, run the full Phase 1 gate checklist. *Deliverable: gate results write-up.*

Known risks, in honesty order: ScreenCaptureKit binding ergonomics (mitigation: CGDisplayStream fallback), enigo key-code coverage for cmd-combos (mitigation: direct CGEvent fallback), Safari-specific event quirks (mitigation: gate tests run on desktop Chrome + Safari).

---

## 5. Pre-seeded deferred decisions (will be logged in `docs/deferred.md` as hit)

- Multi-monitor → capture primary display only.
- Cursor rendering → rely on cursor composited into capture by ScreenCaptureKit config; no client-side cursor.
- Multiple simultaneous controllers → reject second connection.
- Touch UX, clipboard, auth, TLS → later phases by definition.

---

**Status: awaiting architect approval before any code is written (gated workflow step 1).**
