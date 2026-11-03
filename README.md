# Tether

[![CI](https://github.com/allandng/tether/actions/workflows/ci.yml/badge.svg)](https://github.com/allandng/tether/actions/workflows/ci.yml)

Self-hosted remote desktop control — a Chrome Remote Desktop replacement:
view and control a **macOS or Windows** computer from a browser or an
installed phone app (PWA), over the LAN directly or across networks via
WebRTC with a tiny self-hosted signaling server. Your machines show up in a
list with live online status; tap one to connect. Clipboard sync, file
transfer both ways, system audio, multi-monitor, touch gestures. No accounts,
no cloud relay for media (peer-to-peer, DTLS-encrypted).

## Status

| Phase | Scope | State |
|---|---|---|
| 1 — LAN MVP | WS transport, ScreenCaptureKit capture, JPEG, full mouse/keyboard injection | ✅ Done — [gate results](docs/phase1-gate-results.md) (29 fps native, ~40 ms) |
| 2 — WebRTC | Signaling server, P2P data channels, hardware H.264 (VideoToolbox ↔ WebCodecs) | ✅ Done — [gate results](docs/phase2-gate-results.md) (2.5 Mbps at native res) |
| 3 — Clipboard | Bidirectional text clipboard sync, paste-keystroke ordering | ✅ Done — [gate results](docs/phase3-gate-results.md) |
| 4 — Touch UX | Gesture engine (tap/long-press/2-finger scroll/pinch), soft-keyboard TextInput, phone UI | ✅ Done — [gate results](docs/phase4-gate-results.md) (synthetic-touch verified; iPad pass pending) |
| 5 — Secure-internet | Device pairing auth (DTLS-bound, revocable), TURN relay config, adaptive bitrate | ✅ Done — [gate results](docs/phase5-gate-results.md) (live relay traversal pending) |
| 5b — Further hardening | Multi-monitor (switchable), multiple controllers (`--max-controllers`), client-drawn cursor | ✅ Done — [gate results](docs/phase5b-gate-results.md) (multi-display switch pending real hardware) |
| 6 — Reachable | `wss://` end to end, deployable signal+TURN stack, authenticated signal directory, revocation CLI | ✅ Done — [gate results](docs/phase6-gate-results.md) (live WAN/TURN run still pending) |
| 7 — Windows host | DXGI capture, OpenH264, `SendInput`, Win32 clipboard, `HostInfo` + Cmd→Ctrl | ✅ Done — [gate results](docs/phase7-gate-results.md) (30 fps at 1080p, 12.5 ms encode) |
| 8 — Machine list | Saved machines with live online state, one-tap connect, PWA install | ✅ Done — [gate results](docs/phase8-gate-results.md) (phone/PWA pass pending) |
| 9 — Packaging | `config.toml`, control channel (`tetherd pair`/`status`), Windows logon task + installer, macOS `.pkg` + permission flow | ✅ Done — [gate results](docs/phase9-gate-results.md) (code signing needs certificates) |
| 10 — Audio | WASAPI loopback → Opus → a real WebRTC audio track | ✅ Windows — [gate results](docs/phase10-gate-results.md) (macOS capture not built yet) |
| 11 — File transfer | Upload/download over `tether-bulk`, safe naming, backpressure | ✅ Done — [gate results](docs/phase11-gate-results.md) |

Verified end-to-end on a single machine (including connect-while-display-asleep
→ input wakes it). Remaining human checks: a real two-device WAN run and an
iPad pass — see the gate-results docs. Consciously-cut corners live in
[deferred.md](docs/deferred.md).

Phases 1–5b built the engine; phases 6–11 closed the gap to what Chrome
Remote Desktop did — reachable from anywhere, a Windows host, a machine list,
installers, audio and file transfer — per the [roadmap](docs/roadmap.md),
which also covers the locked-screen constraint (keep hosts awake and signed
in). CI builds and tests on both macOS and Windows. Still open: real-device
phone/iPad passes, a live WAN/TURN run, a hands-on run on a real Mac, the macOS
audio capturer, and signing certificates for the installers.

## Layout

- `crates/tether-protocol` — transport-agnostic wire protocol (shared types)
- `crates/tether-signal` — signaling server: presence + SDP/ICE relay, no media
- `crates/tetherd` — host daemon: capture, encoding, input injection, clipboard,
  audio, file transfer, control channel (macOS + Windows)
- `controller/` — browser-based viewer/controller and PWA (TypeScript + Vite)
- `packaging/` — Windows installer scripts and macOS `.pkg` build
- `deploy/` — docker-compose stack: Caddy (TLS) + signal server + coturn
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

**Host (`tetherd`) — Windows 10 (1703+) / 11:**

- Visual Studio 2022+ Build Tools (MSVC, "Desktop development with C++");
  its bundled CMake works (or install CMake separately).
- Rust (stable, MSVC toolchain): [rustup](https://rustup.rs).
- NASM for the JPEG encoder: `winget install NASM.NASM`.
- No permission grants needed. Limitations: UAC prompts / the lock screen
  can't be captured or controlled, and elevated windows ignore input unless
  tetherd runs elevated.

**Signal server (`tether-signal`)** builds and runs on macOS, Windows *or* Linux — it
carries no media, so you can host it on any small box. Same Rust toolchain; no
Apple frameworks needed.

**Controller** (browser app): Node 18+ (`brew install node`).

Build the binaries once with `cargo build --release`; they land at
`target/release/{tetherd,tether-signal}`. The examples below use `cargo run`
for clarity — substitute the release binaries for real use.

## Quick start (Windows host, local test)

```powershell
cargo build --release
.\target\release\tether-signal.exe --secret test-secret          # terminal 1
.\target\release\tetherd.exe --signal 127.0.0.1:7879 --secret test-secret --codec h264 --pair   # terminal 2
cd controller; npm install; npm run dev                            # terminal 3
```

Open http://localhost:5173, ⚙ Settings → signal server `127.0.0.1:7879`,
secret `test-secret`, Save. The machine appears in the list; tap it and enter
the pairing code tetherd printed. For phones and other networks, deploy the
stack in [deploy/](deploy/README.md) (HTTPS is required off-localhost).

## Installing a host as a background service

**Windows** — per-user, starts at sign-in, no console window:

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1 -Signal wss://tether.example.com/ws
& "$env:LOCALAPPDATA\Programs\Tether\tetherd.exe" pair     # prints a pairing code
```

It writes `%APPDATA%\tether\config.toml` (the secret goes in on stdin, never
on a command line), registers the "Tether Host" logon task running
`tetherd-agent.exe` (keeps tetherd alive, logs to
`%LOCALAPPDATA%\Tether\tetherd.log`). `packaging\windows\uninstall.ps1`
removes it and asks before deleting pairing state. `packaging\windows\build.ps1`
makes a distributable zip (and signs it if given a certificate).

**macOS** — `packaging/macos/build-pkg.sh` builds a `.pkg` (signed and
notarized when the Developer ID env vars are set) that installs tetherd and a
per-user LaunchAgent; then `tetherd setup --signal wss://…` and `tetherd pair`.
On first run tetherd checks Screen Recording and Accessibility and opens the
right System Settings pane.

**Config and control.** Every flag can live in `config.toml` in the state
directory (`~/.config/tether`, `%APPDATA%\tether`) under the same name;
command-line flags override it. `tetherd setup --signal … [--codec h264]
[--audio]` writes it owner-only. A running daemon is driven over a local,
owner-only control channel: `tetherd pair` (arm and print a pairing code),
`tetherd status`. `tetherd devices list|revoke <id>` manage paired devices.

## Features at a glance

- **Machine list** — the controller lists every host on your signal server
  with a platform badge and live online/offline state; paired ones connect in
  one tap. Add it to your home screen to use it as an app.
- **Files** — the 📁 drawer uploads files (or drop them on the screen) into
  the host's Downloads folder (`--upload-dir`, `--max-upload-mb`) and
  downloads any host path; transfers are logged on the host.
- **Audio** — start the host with `--audio` (opt-in) to stream its system
  audio over WebRTC; tap 🔊 in the controller to unmute. Windows hosts only so
  far; not over the LAN transport.
- **Keyboard** — controlling a Windows PC from a Mac/iPhone/iPad, Cmd acts
  as Ctrl (Cmd+C copies); Ctrl still works too.

## Running

Two transports; run either or both.

### Signaled WebRTC (Phase 2 — works across networks)

Start the signal server somewhere both devices can reach:

```sh
cargo run --release -p tether-signal -- --bind 192.168.1.5 --secret <shared-secret>
```

On the host (macOS or Windows):

```sh
cargo run --release -p tetherd -- --signal 192.168.1.5:7879 --secret <shared-secret> \
    --codec h264 --bitrate-kbps 4000   # codec flags optional; default jpeg
```

In the controller open ⚙ Settings, enter the signal server address (it
defaults to the page's own origin) and the secret; the host appears in the
machine list under its device id (hostname unless `--device-id` was set).
Media flows peer-to-peer (DTLS-encrypted); the signal server only introduces
the peers.

### Deploying it on the internet (Phase 6)

`deploy/` has a docker-compose stack — Caddy for TLS, the signal server, and
coturn — that puts the controller and the signal endpoint on one hostname, so a
phone just opens `https://tether.example.com`. See
[deploy/README.md](deploy/README.md) for the runbook (VPS sizing, DNS, firewall
rules). Point the host at it with `--signal wss://tether.example.com/ws`.

Give the signal server `--identity-store <path>`. It pins each `device_id` to
the public key that first registered it, so nobody else holding the shared
secret can claim your host's id and evict it. Without the flag those pins are
in-memory and a restart reopens the window.

### Device pairing (Phase 5)

The shared `--secret` only gates the signal server. To authorize a *device*,
pair it once: start the host with `--pair` (or `--require-pairing` to refuse
all unpaired controllers), read the printed code, and enter it in the
controller when prompted. The host issues a per-device token (stored in the
browser, valid 90 days) so future connects need no code. The pairing proof is
bound to the DTLS fingerprints, so a malicious signal relay can't MITM it.

List and revoke paired devices with `tetherd devices list` and `tetherd devices
revoke <id>`. A running daemon notices the change and refuses that device at its
next connect — no restart needed.

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

Then open "Connect by address (LAN)" below the machine list and enter
`192.168.1.10:7878` (or open `?host=192.168.1.10:7878`).
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

The installers above set this up for you; this section is the manual
equivalent. `tetherd` and `tether-signal` are foreground processes; a service
manager keeps them up. `tetherd` must run in your **logged-in GUI session** (it needs the
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
    <!-- no arguments: settings (incl. the secret) come from
         ~/.config/tether/config.toml, written by `tetherd setup` -->
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
cargo run -p tetherd --example capture_smoke   # live capture fps check (macOS: needs Screen Recording)
cargo run --release -p tetherd --example h264_smoke  # capture + H.264 fps/cost
cargo run -p tetherd --example inject_smoke    # moves the cursor and reads it back (briefly takes your mouse)
cargo test -p tetherd -- --ignored             # real clipboard / audio device checks
```
