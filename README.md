# Tether

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
| 5b — Further hardening | Multi-monitor, client-drawn cursor, multiple controllers | Future |

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

Then enter `192.168.1.10:7878` in the connect bar (or open `?host=192.168.1.10:7878`).
Click the canvas to give it focus; keyboard and mouse are forwarded while focused.

### macOS permissions (once per machine)

`tetherd` needs two TCC grants for the app that launches it (your terminal):

- **Screen Recording** — System Settings → Privacy & Security → Screen Recording.
  Without it, `tetherd` exits at startup with an explanatory error.
- **Accessibility** — System Settings → Privacy & Security → Accessibility.
  Without it, macOS silently discards injected input.

## Development

```sh
cargo test                      # Rust workspace (protocol, daemon, e2e-with-fake-capture)
cd controller && npm test       # controller (protocol vectors, input mapping)
cargo run -p tetherd --example capture_smoke   # live capture fps check (needs Screen Recording)
```
