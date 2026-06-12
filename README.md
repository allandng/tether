# Tether

Self-hosted remote desktop control. Phase 1: LAN MVP — view and control a macOS
host from a browser-based controller over a trusted LAN.

## Layout

- `crates/tether-protocol` — transport-agnostic wire protocol (shared types)
- `crates/tetherd` — host daemon: screen capture, encoding, input injection
- `controller/` — browser-based viewer/controller (TypeScript + Vite)
- `docs/` — plan, protocol spec, deferred decisions

## Development

```sh
cargo test                      # Rust workspace
cd controller && npm install && npm test   # controller
```

macOS host requirements (granted once per machine, prompted on first run):
**Screen Recording** and **Accessibility** permissions for `tetherd`.
