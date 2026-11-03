# Phase 7 gate results — Windows host

Built per [phase7-plan.md](phase7-plan.md). `tetherd` builds and runs on
Windows (tested on Windows 11 Pro, build 26300, two 1920×1080 monitors) with
the same protocol, transports, pairing and controller as the macOS host. The
platform-neutral layers took no Windows-specific logic: the pipeline, session,
WebRTC, auth and adaptive-bitrate code is shared unchanged apart from the
additive Phase 7–11 features.

## What shipped

| Module | Implementation |
|---|---|
| M0 build | Apple crates moved under `[target.'cfg(target_os = "macos")']`; Windows deps behind `cfg(windows)`. `auth.rs` persistence made portable (0600/0700 on Unix; `%APPDATA%`'s owner-only inherited ACL on Windows). State dir `%APPDATA%\tether`. A `rust-windows` CI job builds, lints (`-D warnings`) and runs the full test suite — green on its first run (`1a363ee`). Prereqs: VS Build Tools (MSVC), CMake (VS ships one), NASM (`winget install NASM.NASM`). |
| M1 capture | `capture/windows.rs`: **DXGI Desktop Duplication** (not WGC — see below). D3D11 device on the output's own adapter, staging-texture readback, `RowPitch` → `bytes_per_row`. Cursor composited in software from the duplication's pointer shape (`capture/windows/cursor.rs`, all three shape types, unit-tested). Rebuilds itself on `DXGI_ERROR_ACCESS_LOST` (mode change, hot-plug, secure desktop) and repeats the last image every 250 ms while the screen is static, so a newly joined controller always gets a frame and a keyframe. Display ids are the GDI device numbers (`\\.\DISPLAY2` → 2), shared with the injector. |
| M2 input | `input/windows.rs`: `SendInput`, absolute mouse with `MOUSEEVENTF_VIRTUALDESK`, scan-code keyboard (`input/windows/keymap.rs`, extended-key table pinned by tests), `KEYEVENTF_UNICODE` text (surrogate pairs; `\n`/`\t` as real Enter/Tab). Process is per-monitor-DPI-aware so every coordinate is physical pixels. |
| M3 HostInfo | `0x0D HostInfo {os, version, name}` sent after the handshake on both transports; cross-pinned vectors in Rust and TS. Controller `remap.ts`: an Apple client driving a Windows host sends Meta as Control (Cmd+C copies; Ctrl+C still copies); against a macOS host nothing changes. Applied at the single point all key events leave the controller (keys, paste flow, soft keyboard). |
| M4 clipboard | `clipboard/windows.rs`: change detection via `GetClipboardSequenceNumber` (maps straight onto `change_count`); reads/writes on a dedicated owner thread with a message-only window and a real message loop (so other apps' `EmptyClipboard` never blocks on us); UTF-16 ↔ UTF-8 and CRLF ↔ LF at the boundary. |
| M5 H.264 | `encode/h264_soft.rs`: **OpenH264** (not the Media Foundation MFT — see below), screen-content preset, Annex B with SPS/PPS on every IDR, wall-clock keyframes every 2 s, live bitrate via `ENCODER_OPTION_BITRATE` for adaptive bitrate (rate control needs frame-skip on; skipped frames come back empty and the pipeline drops them). BGRA→I420 BT.601 conversion in `encode/yuv.rs` (2:1 downscale above 3840×2160). |
| M6 service | Logon **Scheduled Task** running `tetherd-agent.exe` (a windowless launcher that keeps tetherd alive and logs to `%LOCALAPPDATA%\Tether\tetherd.log`), installed by `packaging/windows/install.ps1` — see Phase 9. |

### Two deliberate deviations from the plan

- **Desktop Duplication instead of Windows.Graphics.Capture.** The plan left
  this open pending the Windows version. DD is pull-based like the trait, needs
  no WinRT dispatcher, has no yellow capture border on any Windows version, and
  behaves the same on 10 and 11. Its cost — the cursor isn't in the image — is
  covered by the compositor.
- **OpenH264 instead of the Media Foundation MFT.** Both are vendor-neutral, but
  the MF hardware encoders are asynchronous MFTs whose event model differs per
  GPU vendor, and the software MFT is no faster than OpenH264. OpenH264 is one
  synchronous call that behaves identically on every machine, including VMs.
  The measured cost below clears the gate; a hardware path stays an additive
  `FrameEncoder` if a 4K host ever needs it.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | Controller drives a Windows host end to end over WebRTC — screen, mouse, keyboard, clipboard — with the unmodified controller | ✅ Verified on this machine over WebRTC (local signal server) and LAN: H.264 stream at 27–30 fps, pairing, token reconnect, display switch, file transfer. Mouse injection verified to the pixel on both monitors (`inject_smoke`); keyboard verified by `SendInput` acceptance; clipboard by the real-clipboard round-trip test. **Not yet done: a phone over a real WAN.** |
| 2 | `--codec h264` ≥ 25 fps at 1080p, readback + encode cost recorded | ✅ `h264_smoke`, 1920×1080, 5 s of live desktop: **30.0 fps**, capture wait + DD readback avg 20.6 ms (mostly the pacing wait), **encode avg 12.5 ms** (of which BGRA→I420 ≈ 3.0 ms), ~0.6 Mbps on mostly-static content at a 4 Mbps target. JPEG path: 27.6 fps, 3.5 ms/frame. |
| 3 | Multi-monitor: picker lists all, switching works, clicks land on the right monitor | ✅ Two monitors (primary at x=0, secondary at x=−1920). Picker showed both; switching to Display 2 streamed it (29 fps, a playing video); `inject_smoke` landed on expected pixels on both, including negative virtual-desktop coordinates. Discharges the Phase 5b multi-display debt. |
| 4 | Cmd+C on an Apple keyboard copies on Windows; Ctrl+C still copies; macOS host unchanged | ✅ by unit tests (`remap.test.ts`); not exercised with a physical Apple keyboard. |
| 5 | Adaptive bitrate rises/falls on Windows | ✅ `set_bitrate` applies live without disturbing the stream (encoder test); the AIMD loop is shared and already gated. Not re-measured under synthetic backpressure on Windows. |
| 6 | Pairing, revocation, `--max-controllers`, both transports identical | ✅ Same code paths; pairing with a printed code and token reconnect exercised live over WebRTC; full integration suites pass on Windows. |
| 7 | No macOS regressions | ✅ compiles: CI on `1a363ee` passed on `macos-26` — the whole workspace builds and is clippy-clean with `-D warnings` (including `macos_perms.rs`), and the protocol + signal suites pass. ⚠️ Not yet *run* on a Mac: the macOS job doesn't execute `tetherd`'s tests (they need a GUI session), and macOS code paths changed only in dependency gating, `auth.rs` persistence (same behaviour under `cfg(unix)`) and the new `macos_perms.rs`. |

## Documented limitations

- **UAC and the secure desktop** (UAC prompts, Ctrl+Alt+Del, the lock screen)
  can't be captured or injected into by a normal-integrity process: the stream
  freezes on the last image and input is dropped until it closes. Likewise
  **UIPI** blocks input to elevated windows unless tetherd itself runs elevated
  (logged once when `SendInput` is refused).
- **Sleep / lock**: as on macOS (roadmap, "locked-screen problem"), set the PC
  never to sleep; a logged-in, unlocked session is required.
- **Rotated displays** stream unrotated (logged).
- **Unsigned binaries** trip SmartScreen/Defender until code-signed (Phase 9).
