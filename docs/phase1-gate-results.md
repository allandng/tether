# Phase 1 gate results — 2026-06-12

Verification environment: single MacBook (3420×2214 native capture), tetherd
release build on loopback, controller in a Chromium-based browser. The
remaining "real LAN, two devices" pass is a human check (see bottom).

| Gate criterion | Result | Evidence |
|---|---|---|
| ≥15 fps at native resolution | **PASS — 29 fps** | `capture_smoke`: 29.3 fps capture at 3420×2214, 12.1 ms avg encode, ~540 KiB/frame. Browser viewer stats: 29 fps sustained. Transport-level floor also pinned by `tests/e2e_fake.rs` (fails CI below 15 fps). |
| Mouse mapping incl. different resolutions | **PASS** | `inject_smoke`: cursor moved to normalized (32768,32768) and landed at the exact display-point center, delta (0.0, 0.0). Mapping is structural (normalized u16 over the displayed rect ↔ display points), unit-tested in both implementations. |
| Keyboard incl. modifiers | **PASS** | `inject_typing` against TextEdit: injected `tether`, Shift+A, Cmd+S through the real injector; saved file read back as `tetherA` — shift produced the capital, cmd triggered Save. |
| Clean disconnect/reconnect without restarting tetherd | **PASS** | Integration test `clean_disconnect_then_reconnect_without_restart`, plus live browser toggle: disconnect → reconnect → streaming again at 29 fps. |
| <150 ms end-to-end latency | **PASS (same-machine)** | Capture-timestamp → render age readout: 38–57 ms observed. LAN adds one WS hop of a ~540 KiB frame (~5 ms on gigabit, ~15–40 ms on decent Wi-Fi) — comfortably inside budget, but confirm subjectively on the real setup. |

## Operational notes

- **Run tetherd with `--release`.** Debug builds compile libjpeg-turbo
  unoptimized: 115 ms/frame (8.6 fps) vs 12 ms/frame (29 fps).
- **Native pixels**: `CGDisplayPixelsWide` lies under scaled modes (returns
  logical size); capture uses `CGDisplayMode::pixel_width()` instead.
- **TCC attribution** is per launching app, not per binary. Whatever terminal
  runs `tetherd` needs Screen Recording + Accessibility. During verification
  these were granted to the Claude desktop app ("Claude" for Screen Recording,
  nested helper "Claude Code" for Accessibility) — grant them again for your
  real terminal before first use.
- Verification helpers live in `crates/tetherd/examples/`: `capture_smoke`
  (fps), `inject_smoke` (cursor mapping self-test), `inject_diag` (permission
  preflight), `inject_typing` (keyboard/modifier check vs TextEdit).

## Remaining human checks (real-world setup)

1. Two-device LAN run (Mac host + second machine / iPad in Safari):
   subjective latency and input feel.
2. Scroll direction feel (deferred.md: sign is a one-line flip if wrong).
3. iPad Safari with trackpad: pointer/keyboard behavior (Phase 4 owns the
   full touch UX).
