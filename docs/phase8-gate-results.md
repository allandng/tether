# Phase 8 gate results — machine list and one-tap connect

Built per [phase8-plan.md](phase8-plan.md).

## What shipped

- **M1 directory.** `controller/src/directory.ts` holds a controller
  registration with the signal server open for as long as the app is, under its
  own id (`<device id>-dir`, so it never evicts the session's own
  registration). Reconnects with jittered backoff, immediately on
  `visibilitychange`→visible and on `online`, and stops retrying on refusals
  that won't fix themselves (bad secret, identity mismatch). The signal
  protocol gained an optional `os` on `Register` and `PeerInfo` (serde default,
  not wire-breaking); hosts report `macos` / `windows` / `linux`.
- **M2 store.** `machines.ts`: saved machines `{deviceId, name, os, lastSeen}`
  reconciled against the live snapshot — paired + present → online, paired +
  absent → offline with last-seen, present + unknown → "new, tap to pair".
  Pure and unit-tested. A machine is remembered on its first successful
  connection; Settings can forget it (and its token).
- **M3 views.** `main.ts` is two render paths over one state object: the
  machine list (landing view, with a "connect by address" fold-out for the LAN
  transport) and the session view (back, title with host OS, display picker,
  clipboard, files, speaker, keyboard, pointer mode, fullscreen). Settings
  (signal server, secret, this device's name, saved machines) is a dialog,
  entered once. The signal server defaults to the page's own origin — the
  single-origin deploy means a fresh phone types only the secret.
- **M4 PWA.** `controller/public/manifest.webmanifest` (standalone, theme
  colours), icons generated reproducibly by `controller/scripts/gen-icons.mjs`
  (192/512, maskable 512, 180 apple-touch, SVG). No service worker: Chrome no
  longer requires one to install, and caching a remote-control shell buys
  nothing.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | Paired machines listed with correct online/offline state without connecting | ✅ Live: host appeared with a Windows badge as soon as Settings were saved. |
| 2 | Tapping an online machine connects with no further input | ✅ After pairing once, tap → streaming (token auth), no code. |
| 3 | Starting/stopping a host flips its dot within a second or two | ✅ Killed `tetherd`: row showed "Offline · just now" within 2 s, no reload. |
| 4 | Backgrounding the phone browser for minutes restores a live list | ⏳ Reconnect-on-visible is implemented; needs a real iOS/Android device. |
| 5 | A never-seen host appears as new and pairs from its row | ✅ "New — tap to pair" → pairing code prompt → paired → row becomes Online. |
| 6 | Installed to a home screen, launches standalone on iOS and Android | ⏳ Manifest + icons in place; needs real devices over HTTPS. |
| 7 | No regressions: LAN by address, tokens, pairing, display picker | ✅ LAN connect (by address and `?host=`), pairing, display picker all exercised; controller suite 141/141. |
