# Tether

Self-hosted remote desktop control. Phase 1: LAN MVP — view and control a macOS
host from a browser-based controller over a trusted LAN.

## Layout

- `crates/tether-protocol` — transport-agnostic wire protocol (shared types)
- `crates/tetherd` — host daemon: screen capture, encoding, input injection
- `controller/` — browser-based viewer/controller (TypeScript + Vite)
- `docs/` — plan, protocol spec, deferred decisions

## Running (Phase 1, trusted LAN only)

On the host (macOS):

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
