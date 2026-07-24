# Phase 7 Plan — Windows host

**Scope:** a `tetherd` that runs on Windows with the same protocol, transports,
pairing, and controller as the macOS host. Capture, encode, input injection,
clipboard, and multi-monitor — four new platform modules behind traits that
already exist.

The seam was cut in Phase 1 and has held: `ScreenCapturer`, `InputInjector`,
and `Clipboard` are traits whose only implementations sit behind
`#[cfg(target_os = "macos")]`. Everything above them — the pipeline, both
transports, auth, adaptive bitrate, the session layer — is platform-neutral and
should not need edits.

---

## 0. The build does not currently compile on Windows

Step zero, before any Windows code. `crates/tetherd/Cargo.toml` lists every
Apple dependency unconditionally — `core-graphics`, `screencapturekit`, the
seven `objc2-*` crates. Cargo will try to build all of them on Windows and
fail.

- Move them into `[target.'cfg(target_os = "macos")'.dependencies]`.
- Add `[target.'cfg(target_os = "windows")'.dependencies]` with the `windows`
  crate and the feature set for Graphics.Capture, Direct3D11, Media
  Foundation, and Win32 input/clipboard.
- `turbojpeg` builds libjpeg-turbo from source and needs **NASM** and **CMake**
  on the build machine. Keep the JPEG path for parity and debugging, but the
  Windows prerequisites section has to say so.
- Add a `windows-latest` job to CI. It cannot run the capture, injection, or
  clipboard tests (they need a real desktop session, same as macOS), but it
  must compile `tetherd` and run the platform-neutral suites — otherwise the
  Windows build rots silently between phases.

## 1. Capture — Windows.Graphics.Capture

**Primary: WGC** (Windows 10 1903+), with DXGI Desktop Duplication as a
fallback if a WGC problem forces it.

WGC hands back a `Direct3D11CaptureFramePool` producing `IDirect3DSurface`
frames in BGRA. To satisfy the existing `RawFrame`, copy each to a staging
texture and `Map` it.

One pleasant fit: `RawFrame` already carries `bytes_per_row` separately from
`width * 4`, precisely because captured rows can be padded. That field maps
exactly onto `D3D11_MAPPED_SUBRESOURCE::RowPitch`. No struct change.

Two caveats to plan around:

- **The capture border.** WGC draws a yellow border around captured content by
  default. It can be turned off via `IsBorderRequired = false`, but only on
  Windows 11 build 22000+. On Windows 10 the border is unavoidable with WGC —
  which is the strongest argument for keeping the Desktop Duplication fallback.
- **Mode changes.** Resolution changes, monitor hot-plug, and remote-session
  transitions invalidate the capture item. Desktop Duplication surfaces this as
  `DXGI_ERROR_ACCESS_LOST`; WGC closes the item. Either way the capturer must
  rebuild itself and let the pipeline re-announce `Resolution` — the pipeline
  already detects dimension changes per frame, so recovery is a capturer-local
  concern.

**Multi-monitor** falls out cleanly. `EnumDisplayMonitors` enumerates, and
`IGraphicsCaptureItemInterop::CreateForMonitor` builds an item per `HMONITOR`.
That maps directly onto the `displays()` and `switch_display()` trait methods
Phase 5b already defined, so the protocol and the controller picker need
nothing new.

## 2. Encode — Media Foundation H.264

Use the Media Foundation H.264 encoder MFT rather than NVENC or AMF, to stay
vendor-neutral across whatever GPU the machine has.

**The one real architectural question in this phase** is colour conversion and
where frames live. On macOS, VideoToolbox accepts BGRA directly
(`encode/h264.rs:214` wraps the capture bytes in a `CVPixelBuffer` with
`kCVPixelFormatType_32BGRA` and no copy), so the CPU-side `RawFrame` costs
nothing. Media Foundation's encoder wants **NV12**.

Worse, the current trait shape forces a round trip: WGC produces a GPU texture,
`RawFrame` requires CPU bytes, and the encoder wants it back on the GPU. That
is a readback and an upload per frame — roughly 8 MB each way at 1080p, more at
4K.

Recommendation: **take the readback in v1.** Convert BGRA→NV12 on the CPU,
feed the MFT, and measure. It reuses the entire existing pipeline unchanged and
is very likely fast enough at 1080p/30. If measurement says otherwise, the
optimisation is well-understood and additive: an optional GPU-resident frame
path where `ID3D11VideoProcessor` does BGRA→NV12 on the GPU and the MFT takes a
DXGI surface sample, with the CPU path kept for the JPEG encoder. Do not build
that speculatively — the trait change is only worth making against a number.

Adaptive bitrate needs a Windows equivalent of `VtH264Encoder::set_bitrate`:
`ICodecAPI::SetValue` with `CODECAPI_AVEncCommonMeanBitRate` on the live MFT.
The AIMD control loop itself is platform-neutral and already unit-tested.

## 3. Input — SendInput

`SendInput` with `INPUT_MOUSE` and `INPUT_KEYBOARD` structures.

- **Absolute mouse** uses coordinates normalised to 0–65535, and with
  `MOUSEEVENTF_VIRTUALDESK` they span the whole virtual desktop rather than the
  primary monitor. That flag is what makes `set_active_display()` work on a
  multi-monitor Windows host — without it, injected clicks land on the primary
  display no matter which one is being captured.
- **Keyboard** should use `KEYEVENTF_SCANCODE` rather than virtual key codes.
  The controller sends DOM `code` values, which are physical positions; scan
  codes are also physical, so the mapping is layout-independent and mirrors
  what `input/macos/keymap.rs` does. A `keymap.rs` alongside it, with the same
  cross-pinned test-vector treatment.
- **Unicode text** — the soft-keyboard and emoji path — uses
  `KEYEVENTF_UNICODE`, which injects a character directly with no layout
  involvement. Clean parity with the macOS `inject_text` implementation.

### The modifier problem

This needs a decision, not just an implementation. The controller sends Meta
for Cmd. On a Windows host Meta is the Win key, but a user pressing Cmd+C on an
Apple keyboard means copy, which is Ctrl+C on Windows. This is the mirror image
of the ctrl→cmd item deferred since Phase 3.

Proposal: **the host tells the controller what it is, and the controller
remaps.** A new additive message `0x0D HostInfo { os, version, name }` sent
after the handshake — old peers skip it via the existing unknown-type path. The
controller then applies a per-OS modifier policy: against a Windows host, Cmd
becomes Ctrl and a real Ctrl passes through; against a macOS host, behaviour is
unchanged. Keeping the remap client-side means it also fixes the Phase 3 item
for non-Mac keyboards, and `HostInfo` is useful to Phase 8's machine list
anyway, which can show a platform icon per machine.

## 4. Clipboard

Windows is event-driven where macOS is not: a message-only window
(`HWND_MESSAGE`) plus `AddClipboardFormatListener` delivers `WM_CLIPBOARDUPDATE`
instead of the 600 ms `changeCount` polling macOS forces.

No trait change needed, though. Run the message pump on the clipboard thread
and have it increment an atomic counter; `change_count()` returns that counter.
The existing `ClipboardSync` loop keeps working, just with a change signal that
is genuinely instant rather than up to 600 ms stale. Text is `CF_UNICODETEXT`,
which is UTF-16 — convert at the boundary, since the protocol carries UTF-8.

## 5. Running as a service

**This is the Windows-specific trap.** A Windows Service runs in session 0,
which has no access to the interactive user's desktop — it cannot capture the
screen or inject input into the user's session. Chrome Remote Desktop solved
this with a daemon/host split: a service that spawns a per-session host process
with the logged-in user's token via `CreateProcessAsUser`.

Recommendation for v1: **a Scheduled Task registered to run at logon as the
user.** It sidesteps session 0 entirely, works whenever someone is logged in,
and is what the roadmap's option 1 assumes. The daemon/host split is only worth
building if login-screen access becomes a requirement — and per the roadmap,
that decision should be made *before* this phase ships, because it changes this
design.

Two documented limitations either way:

- **UAC prompts are uncapturable.** They render on the secure desktop, where a
  normal-integrity process can neither capture nor inject. The remote screen
  freezes for the duration of a UAC prompt and input does not reach it. CRD had
  the same limitation unless its host ran elevated.
- **SmartScreen and Defender** will flag an unsigned remote-control binary,
  loudly. That is a Phase 9 signing problem, but it shows up first here, the
  moment the binary leaves the build machine.

The LAN transport also needs an inbound firewall rule; the WebRTC path is
outbound-only and needs nothing.

## 6. Module order

1. **M0 — build:** dependency gating, Windows CI job, prerequisites docs. The
   workspace compiles on Windows with `todo!()` platform stubs.
2. **M1 — capture:** WGC capturer, staging readback, `displays()` and
   `switch_display()` via `HMONITOR`, rebuild-on-mode-change.
3. **M2 — input:** `SendInput` mouse and keyboard, scan-code keymap with
   cross-pinned vectors, `KEYEVENTF_UNICODE` text, `set_active_display` via
   `MOUSEEVENTF_VIRTUALDESK`.
4. **M3 — `HostInfo` + modifier policy:** protocol message, host side, and the
   controller's per-OS remap. Closes the Phase 3 ctrl→cmd item too.
5. **M4 — clipboard:** message-only window, `WM_CLIPBOARDUPDATE`, UTF-16
   boundary.
6. **M5 — H.264:** Media Foundation MFT, BGRA→NV12, live bitrate via
   `ICodecAPI`. Measure the readback cost and record the number.
7. **M6 — service:** logon Scheduled Task, firewall rule, limitations
   documented.
8. **M7 — gate.**

## 7. Gate criteria (proposed)

1. A phone controls a Windows host end to end over WebRTC — screen, mouse,
   keyboard, and clipboard both directions — with the **unmodified** controller
   build used for the Mac host.
2. `--codec h264` sustains ≥ 25 fps at 1080p with the measured end-to-end
   readback-plus-encode cost recorded in the gate doc.
3. On a multi-monitor Windows host, the display picker enumerates all monitors,
   switching works, and injected clicks land on the correct monitor. This also
   discharges the Phase 5b multi-display debt, which macOS hardware never
   provided.
4. Cmd+C on an Apple keyboard against a Windows host copies; Ctrl+C still
   copies. The reverse against a macOS host is unchanged.
5. Adaptive bitrate rises and falls on Windows under the same synthetic
   backpressure the macOS path was gated with.
6. Pairing, revocation, `--max-controllers`, and both transports behave
   identically to macOS — the platform-neutral layers took no Windows-specific
   edits.
7. No macOS regressions: the full suite still passes on the Mac host.

## 8. Risks

- **GPU→CPU readback may not hold up at 4K.** Mitigated by measuring in M5
  rather than guessing, with the GPU-resident path scoped and understood but
  not built.
- **The WGC yellow border on Windows 10** is not removable. If the target
  machine is Windows 10, Desktop Duplication becomes the primary rather than
  the fallback — worth confirming the actual Windows version before M1.
- **Scope.** This is the largest phase by far, and the temptation will be to
  fix the session-0 story at the same time. Do not. Ship the logon task,
  observe whether locked-machine access is genuinely missed, and revisit with
  evidence.
- **Two hosts, one protocol.** Every platform-neutral change from here on has
  to be exercised on both. The Windows CI job is the guard, but it cannot cover
  capture or injection — those stay human checks on real hardware.

---

**Status: planned, not started. Blocked on Phase 6 for anything requiring
remote reachability, though the platform modules can be built and tested over
LAN first.**
