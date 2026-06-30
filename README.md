# Tether

[![CI](https://github.com/allandng/tether/actions/workflows/ci.yml/badge.svg)](https://github.com/allandng/tether/actions/workflows/ci.yml)

Self-hosted remote desktop control: view and control a macOS host from a
browser-based controller — over the LAN directly, or across networks via
WebRTC with a tiny self-hosted signaling server. No accounts, no cloud relay
for media (peer-to-peer, DTLS-encrypted).

## Status

| Phase | Scope | State |
|---|---|---|
| 1 — LAN MVP | WS transport, ScreenCaptureKit capture, JPEG, full mouse/keyboard injection | ✅ Done — [gate results](docs/phase1-gate-results.md) (29 fps native, ~40 ms) |
| 2 — WebRTC | Signaling server, P2P data channels, hardware H.264 (VideoToolbox ↔ WebCodecs) | ✅ Done — [gate results](docs/phase2-gate-results.md) (2.5 Mbps at native res) |
| 3 — Clipboard | Bidirectional text clipboard sync, paste-keystroke ordering | ✅ Done — [gate results](docs/phase3-gate-results.md) |
| 4 — Touch UX | Gesture engine (tap/long-press/2-finger scroll/pinch), soft-keyboard TextInput, phone UI | ✅ Done — [gate results](docs/phase4-gate-results.md) (synthetic-touch verified; iPad pass pending) |
| 5 — Secure-internet | Device pairing auth (DTLS-bound, revocable), TURN relay config, adaptive bitrate | ✅ Done — [gate results](docs/phase5-gate-results.md) (live relay traversal pending) |
| 5b — Further hardening | Multi-monitor (switchable), multiple controllers (`--max-controllers`), client-drawn cursor | ✅ Done — [gate results](docs/phase5b-gate-results.md) (multi-display switch pending real hardware) |

Verified end-to-end on a single machine (including connect-while-display-asleep
→ input wakes it). Remaining human checks: a real two-device WAN run and an
iPad pass — see the gate-results docs. Consciously-cut corners live in
[deferred.md](docs/deferred.md).

## Layout

- `crates/tether-protocol` — transport-agnostic wire protocol (shared types)
- `crates/tether-signal` — signaling server: presence + SDP/ICE relay, no media
- `crates/tetherd` — host daemon: capture, encoding, input injection, clipboard
- `controller/` — browser-based viewer/controller (TypeScript + Vite)
- `docs/` — per-phase plans and gate results, protocol spec, deferred decisions

See [docs/architecture.md](docs/architecture.md) for the component/data-flow
overview and where state is persisted.

## Prerequisites

**Host (`tetherd`) — macOS only** (binds ScreenCaptureKit, VideoToolbox, AppKit):

- macOS 26 (Tahoe) with Xcode 26 — a transitive Metal dependency
  (`apple-metal` via `screencapturekit`) compiles against macOS 26 Metal 4
  APIs, so an older SDK won't build. Full Xcode (not just the CLT) provides the
  matching SDK.
- Rust (stable, edition 2024 — 1.85+): install via [rustup](https://rustup.rs).
- `cmake` and `nasm` (build the JPEG encoder): `brew install cmake nasm`.

**Signal server (`tether-signal`)** builds and runs on macOS *or* Linux — it
carries no media, so you can host it on any small box. Same Rust toolchain; no
Apple frameworks needed.

**Controller** (browser app): Node 18+ (`brew install node`).

Build the binaries once with `cargo build --release`; they land at
`target/release/{tetherd,tether-signal}`. The examples below use `cargo run`
for clarity — substitute the release binaries for real use.

## Running

Two transports; run either or both.

### Signaled WebRTC (Phase 2 — works across networks)

Start the signal server somewhere both devices can reach:

```sh
cargo run --release -p tether-signal -- --bind 192.168.1.5 --secret <shared-secret>
```

On the host (macOS):

```sh
cargo run --release -p tetherd -- --signal 192.168.1.5:7879 --secret <shared-secret> \
    --codec h264 --bitrate-kbps 4000   # codec flags optional; default jpeg
```

In the controller UI pick **Signaled**, enter the signal server address, the
secret, and the host's device id (its hostname unless `--device-id` was set).
Media flows peer-to-peer (DTLS-encrypted); the signal server only introduces
the peers.

### Device pairing (Phase 5)

The shared `--secret` only gates the signal server. To authorize a *device*,
pair it once: start the host with `--pair` (or `--require-pairing` to refuse
all unpaired controllers), read the printed code, and enter it in the
controller when prompted. The host issues a per-device token (stored in the
browser) so future connects need no code. The pairing proof is bound to the
DTLS fingerprints, so a malicious signal relay can't MITM it. Revoke by
deleting the device from `~/.config/tether/paired.json`.

### TURN relay (for symmetric NAT)

STUN-only by default — symmetric-NAT pairs won't connect. Run a coturn relay
(`--use-auth-secret --static-auth-secret <S>`) and point the signal server at
it; it mints short-lived credentials per registration:

```sh
TETHER_TURN_SECRET=<S> cargo run --release -p tether-signal -- \
    --bind 0.0.0.0 --secret <shared-secret> \
    --turn-url 'turn:relay.example.com:3478?transport=udp' \
    --turn-url 'turns:relay.example.com:5349?transport=tcp'
```

The `static-auth-secret` in coturn must equal `TETHER_TURN_SECRET` (the signal
server mints `username=<expiry>:<id>`, `password=base64(HMAC-SHA1(secret,
username))`, which is exactly coturn's `use-auth-secret` scheme). A minimal
`/etc/turnserver.conf`:

```ini
realm=relay.example.com
use-auth-secret
static-auth-secret=<S>          # == TETHER_TURN_SECRET
listening-port=3478
tls-listening-port=5349         # for turns:
min-port=49152
max-port=65535
external-ip=<public-ip>         # if the relay is behind NAT
```

Open UDP/TCP 3478 (and 5349 for TLS) plus the `min-port`–`max-port` UDP range
in the firewall. `brew install coturn` (macOS) or `apt install coturn` (Linux),
then `turnserver -c /etc/turnserver.conf`.

### Multiple controllers & multi-monitor (Phase 5b)

`--max-controllers N` (default 1) lets several paired devices view and control
at once, shared across both transports; input from all of them interleaves
into the host. If the host has multiple displays, a picker appears in the
controller bar — switching is shared (everyone sees the chosen display). In
trackpad pointer mode the controller draws a local cursor dot for zero-lag
aiming.

### Adaptive bitrate

With `--codec h264`, the host adapts the encoder bitrate to WebRTC send-buffer
pressure (AIMD between ~600 kbps and the `--bitrate-kbps` ceiling).

Use `--codec h264` over WAN (~2.5 Mbps at native resolution vs ~125 Mbps for
JPEG). Connecting to a Mac whose display is asleep works: the screen stays
black until your first input wakes it.

### LAN WebSocket (Phase 1 path)

```sh
cargo run --release -p tetherd -- --bind 192.168.1.10 --allow 192.168.1.20
# --bind: this machine's LAN address (loopback/private only; 0.0.0.0 is refused)
# --allow: the controller's IP (repeatable); all other peers are dropped
# --port: optional, default 7878
```

On the controller, serve the viewer and open it in a browser:

```sh
cd controller && npm install && npm run dev          # same machine, or:
npm run dev -- --host                                # reachable from LAN/iPad
```

For a deployed controller, build the static bundle and serve `dist/` from any
static server or reverse proxy:

```sh
cd controller && npm install && npm run build        # → controller/dist/
npx vite preview --host                               # or: serve dist/ behind nginx/caddy
```

Serve it over **HTTPS** (or `http://localhost`) for anything real: pairing uses
WebCrypto and clipboard auto-write needs a secure context, both of which the
browser blocks on plain remote HTTP.

Then enter `192.168.1.10:7878` in the connect bar (or open `?host=192.168.1.10:7878`).
Click the canvas to give it focus; keyboard and mouse are forwarded while focused.

### macOS permissions (once per machine)

`tetherd` needs two TCC grants for the app that launches it (your terminal):

- **Screen Recording** — System Settings → Privacy & Security → Screen Recording.
  Without it, `tetherd` exits at startup with an explanatory error.
- **Accessibility** — System Settings → Privacy & Security → Accessibility.
  Without it, macOS silently discards injected input.

> **TCC follows the launching binary.** The grant is tied to whichever app
> started `tetherd` — so moving from your terminal to a launchd agent (below)
> means re-granting both for that context, and TCC **cannot** be granted over a
> headless SSH session (you need a logged-in GUI session to approve it once).

## Running as a service

`tetherd` and `tether-signal` are foreground processes; a service manager keeps
them up. `tetherd` must run in your **logged-in GUI session** (it needs the
window server + the TCC grants above), so a per-user **LaunchAgent** is the fit
— `~/Library/LaunchAgents/com.tether.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.tether.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/tetherd</string>
    <string>--signal</string><string>relay.example.com:443</string>
    <string>--secret</string><string>CHANGE-ME</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/tmp/tetherd.log</string>
</dict></plist>
```

`launchctl load ~/Library/LaunchAgents/com.tether.daemon.plist` (and
`launchctl stop com.tether.daemon` sends SIGTERM, which `tetherd` handles by
closing active sessions cleanly). Re-grant Screen Recording + Accessibility to
the launchd context the first time. The **signal server** has no GUI/TCC needs,
so on Linux a systemd unit (or any process supervisor) works; on macOS a
LaunchDaemon is fine.

## Development

```sh
cargo test                      # Rust workspace (protocol, daemon, e2e-with-fake-capture)
cd controller && npm test       # controller (protocol vectors, input mapping)
cargo run -p tetherd --example capture_smoke   # live capture fps check (needs Screen Recording)
```
