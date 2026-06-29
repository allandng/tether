# Contributing to Tether

## Setup

See [Prerequisites](README.md#prerequisites). In short: macOS + Xcode CLT for
the host, Rust stable (edition 2024), `cmake`/`nasm`, and Node 18+ for the
controller.

## Before you push

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) gates on all of these
— run them locally first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test                         # full Rust suite (needs a GUI session — see below)
cd controller && npm test && npm run build
```

## What CI can and can't run

`tetherd`'s capture, encode, clipboard, and injection tests touch macOS APIs
(Screen Recording, VideoToolbox, NSPasteboard) that need a logged-in GUI
session, so **headless CI runs only the GUI-free crates** (`tether-protocol`,
`tether-signal`) plus the controller. Run the full `cargo test` locally on a Mac
before submitting changes to those areas.

## Conventions

- **Protocol parity is load-bearing.** Any change to `crates/tether-protocol`
  must be mirrored in `controller/src/protocol.ts`, with the cross-pinned byte
  vectors updated in *both* test suites.
- **Platform code is macOS-only** under `#[cfg(target_os = "macos")]`; keep the
  traits (`ScreenCapturer`, `FrameEncoder`, `InputInjector`) portable so a
  second OS is an additive impl, not surgery.
- Match the surrounding style: comment density, naming, and idiom.
- Security-sensitive changes (auth, transport, signaling) should come with a
  test and, ideally, a note in [docs/deferred.md](docs/deferred.md) for any
  residual tradeoff.

## Orientation

[docs/architecture.md](docs/architecture.md) is the map; [docs/protocol.md](docs/protocol.md)
is the wire format; the `docs/phase*-gate-results.md` files are the change
history and rationale.
