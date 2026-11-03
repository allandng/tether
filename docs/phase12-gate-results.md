# Phase 12 gate results — macOS + iOS completion

Built per [phase12-plan.md](phase12-plan.md).

**Result: 12 of 13 gate criteria pass.** Criterion 12, a real iPhone and iPad
pass, still needs devices.

- **Open items:** 8 of the 12 passes have a pending check, mostly on real
  hardware. All open items are grouped in
  [Still needs a person](#still-needs-a-person).
- **Evidence:** unit tests, `h264_smoke`, a
  [live run on a real Mac](#live-end-to-end-run-on-a-real-mac), and an
  [iOS Simulator run](#ios-simulator-run) against `fake_host`.
- **Bugs:** the live run and a [review of the capture changes](#capture-review-fixes)
  found bugs that were fixed in this phase. What they left open is in
  [deferred.md](deferred.md#phase-12-additions).

## Gate criteria

| # | Criterion | Result | Evidence |
|---|---|---|---|
| 1 | macOS `--audio` streams system audio, Screen Recording only, 50 packets/s, alongside video | ✅ Pass. ⏳ Pending: listening on a phone. | 50.2 Opus packets/s, 0 lost, live over WebRTC. [Audio](#macos-system-audio) |
| 2 | Capture never goes silent; errors, sleep and mode changes recover without a restart | ✅ Pass. ⏳ Pending: a live mode change, hot-unplug and Stop Sharing. | Static screen at exactly 4.0 fps; resumes ~1.2 s after wake. [Capture](#capture-robustness) |
| 3 | Capture → encode cost measured at native resolution, no regression vs Phase 2 | ✅ Pass | CPU 15.3–15.6% → 1.1–3.9%. [Zero-copy](#zero-copy-capture--encode) |
| 4 | macOS input matches the Windows host | ✅ Pass | Real-hardware tests: F19, Caps Lock, volume, IOPM. [Input](#macos-input) |
| 5 | Prompt once per binary + launch context; grants in `tetherd status`; no crash loop | ✅ Pass. ⏳ Pending: launchd-context prompts (need a real `.pkg` install). | Marker logic unit-tested. [Permissions](#permissions-and-packaging) |
| 6 | `.pkg` builds with a stable identifier and refuses an unsupported Mac | ✅ Pass. ⏳ Pending: installing it. | Signed ad hoc as `com.tether.daemon`. [Packaging](#permissions-and-packaging) |
| 7 | Every soft-keyboard key and autocorrect edit arrives; missing keys reachable | ✅ Pass. ⏳ Pending: a real keyboard. | Simulator: Return/Backspace arrive (before: lost). [Keyboard](#soft-keyboard-toolbar-and-paste) |
| 8 | Scroll matches the wheel, slow drags keep motion, one-tap zoom reset, right-click where aimed | ✅ Pass. ⏳ Pending: feel on real hardware. | ±60 pt slow drag sends dy ∓50. [Touch](#touch) |
| 9 | Remote screen visible above the keyboard; bar out of the way on a phone | ✅ Pass | Session letterboxes above the toolbar. [Layout](#layout-and-phone-bar) |
| 10 | Resume reconnects promptly, healthy sessions kept, no auto-lock | ✅ Pass. ⏳ Pending: wake lock and standalone flows on a real device. | Reconnect ~0.2–0.3 s after resume. [Lifecycle](#lifecycle) |
| 11 | Live end-to-end run on a real Mac | ✅ Pass | All 10 checks, after fixing 5 bugs. Mouse moves only. [Live run](#live-end-to-end-run-on-a-real-mac) |
| 12 | Real iPhone and iPad pass | ⏳ Pending: needs devices | iPad Simulator pass stopped at a permission prompt. [Simulator run](#ios-simulator-run) |
| 13 | No regressions | ✅ Pass. ⏳ Pending: Windows CI for `RawFrame` and `DxgiCapturer`. | Controller 141 → 314 tests; Rust 112 → 194 passing. [CI](#ci-and-regressions) |

## Test environment

- **Mac:** one MacBook (M3, macOS 26.5.1, 3420×2214 native capture). It ran
  the host code for real: module tests, `h264_smoke`, and a
  [live end-to-end run](#live-end-to-end-run-on-a-real-mac) with a desktop
  browser as the controller.
- **iOS:** the iOS 26.5 Simulator (iPhone 17 Pro) drove the controller against
  `fake_host`, which logs input instead of injecting it. See
  [iOS Simulator run](#ios-simulator-run).

## macOS system audio

**Result: pass.** A browser received the Opus track live over WebRTC at
50.2 packets/s with 0 lost, alongside H.264 video. No microphone permission
is involved. ⏳ Not listened to on a phone: the live run's audio element was
muted.

| Stage | Measured |
|---|---|
| Real capture | 47,940 frames/s (48 kHz); a Tink alert peaked at 0.36 |
| Through the pipeline | **50.5 Opus packets/s**, 3–392 B (silence 3 B) |
| Live, over WebRTC with H.264 video | Opus `inbound-rtp` at **50.2 packets/s, 0 lost** |
| Live packet sizes | Silence ~39 B per 250 ms; Tink/Glass alerts 2.2–3.1 KB per 250 ms |

**What shipped** (`audio/macos.rs`):

- An audio-only ScreenCaptureKit stream on the main display: a 2×2, 1 fps
  video that nothing consumes, plus the Audio output. 48 kHz stereo, with
  tether's own process excluded.
- SCK delivers Float32 planar 20 ms buffers. The callback interleaves them into
  the existing Opus thread.
- It uses the same Screen Recording grant as video, and no microphone
  permission.
- A 5 s no-buffer watchdog rebuilds a stuck stream. A capturer that fails
  within 10 s of starting counts as a failure and backs off 2 s → 16 s.
- The crate's blocking calls (`SCShareableContent`, start, stop) run on helper
  threads with 5 / 5 / 2 s deadlines, so a wedged SCK can't hang the audio
  thread.

## Capture robustness

**Result: pass.** A static screen now produces frames and keyframes, and
stream errors and display sleep recover without a restart. ⏳ A real
display-mode change, hot-unplug and a live Stop Sharing click were not
exercised (fake-backend tests only).

| Condition | Measured |
|---|---|
| Static screen | **Exactly 4.0 fps**, longest gap 255–260 ms (before: no frames at all) |
| Idle keyframe gaps | ≤ 1.78 s; in the live run, 1942–1998 ms |
| Mostly static desktop, live | Longest frame gap 278 ms, ~1.1 KB frames |
| Heavy machine load (`h264_smoke`) | 24–25 fps, keyframe gaps 1.99–2.00 s |
| Display asleep | The last image repeats; capture resumes ~1.2 s after wake |

Stream errors rebuild in place (backoff 250 ms → 2 s). The capture review
closed the remaining gaps ([below](#capture-review-fixes)).

**What shipped: static screens** (`capture/macos.rs`, `pipeline.rs`):

- On a static screen SCK sends one frame, then only image-less idle samples.
  The capturer now re-emits the last image every 250 ms. Before, there were no
  frames at all, so a joining controller or an H.264 decoder waited forever.
- A latest-wins mailbox replaces the capacity-1 channel that could drop the
  *newest* frame.
- Keyframes are time-based: `MaxKeyFrameIntervalDuration` 2.0 plus a
  forced-IDR backstop.
- JPEG re-sends an unchanged image at most once a second.

**What shipped: recovery:**

- A stream error reported to the SCK delegate rebuilds the stream in place,
  with backoff 250 ms → 2 s. If the captured display is gone, it falls back to
  the main display.
- The display mode is re-read every 1 s and applied with
  `update_configuration`.
- A stream that is silent for 3 s after a (re)configuration is rebuilt.
- The pipeline retries capturer construction and mid-stream errors with
  backoff 1 → 3 s instead of exiting. A missing Screen Recording grant causes
  no crash loop under launchd. The resolution stays 0×0 until capture works.

## Zero-copy capture → encode

**Result: pass, no regression.** At native resolution, zero-copy cut tetherd's
CPU from 15.3–15.6% to 1.1–3.9% of one core. Phase 2's 31.8 ms encode figure
is superseded.

`h264_smoke` at 3420×2214 on an animated lock screen (~29 fps). Before: the
SCK buffer copied to BGRA bytes. After: the retained IOSurface, NV12 into
VideoToolbox.

| | Copy to bytes | Zero-copy |
|---|---|---|
| tetherd CPU (% of one core) | 15.3–15.6% | 1.1–3.9% |
| Peak RSS | 185–212 MiB | 28–29 MiB |
| Capture → encoded, avg (p95) | 21.7 ms (24.5) | 17.8–18.3 ms (22.2) |
| JPEG path, per frame | 15.1–18.8 ms | 11.4–11.7 ms |

The encode benchmark (`bench_zero_copy_vs_bytes_at_native_resolution`,
synthetic NV12 IOSurface):

| Path | Wall | CPU |
|---|---|---|
| Zero-copy | 16.95 ms/frame | 0.20 ms |
| Old bytes path | 17.5 ms | 4.4–5.3 ms |

Wall time is the hardware encoder. What zero-copy removes is the CPU copy and
conversion.

**Other checks:**

- Colours are identical (MAD ≤ 0.19). ffprobe reports `tv`/`bt709` ×3 (was
  unknown).
- Live over WebRTC: tetherd used 3–5% CPU and 33–38 MB RSS while streaming
  H.264 (9–10% before the live-run fixes).
- The SPS rewrite cut Chrome's decode → output from a ~204 ms median to ~3 ms.
- Hardware encode is capped at 4096×2304. Larger displays are scaled down
  rather than silently falling back to a ~49–83 ms/frame software encoder.

**What shipped** (`capture/pixel_buffer.rs`, `encode/h264.rs`):

- Frames carry SCK's retained IOSurface `CVPixelBuffer` instead of a copy
  (`RawFrame { pixels: Pixels }`).
- H.264 gets NV12 `420v`, handed straight to
  `VTCompressionSessionEncodeFrame`. JPEG gets BGRA, read in place.
- SCK's queue depth is 6, with at most 3 buffers retained at once.
- The stream now signals BT.709 in its SPS.
- Windows keeps BGRA bytes.

## Capture review fixes

**Result:** a review of the capture/encode changes (commit `7b31f1f`) fixed
the items below. `h264_smoke` rerun under heavy machine load: 24–25 fps,
keyframe gaps 1.99–2.00 s (baseline under the same load: 21.7 fps, 1.99 s).

**Displays:**

- **Display fallback misrouting input.** `ScreenCapturer::active_display()` is
  new. The pipeline republishes `displays` whenever the active display
  changes, not only when the frame size does. A silent fallback to a same-size
  display no longer sends input to the old one.
- **Display switches and the watchdog.** A pending mailbox frame is discarded
  on a switch. The 3 s watchdog is satisfied only by frames of the configured
  size, and is armed at start and after every rebuild. There is no busy-spin
  when no placeholder size is known (0×0 is refused in start and switch).
- **"Stop Sharing".** The user can stop capture from the macOS recording
  indicator (`SCStreamErrorCode::UserStopped`, −3817). Capture now pauses with
  black frames and no automatic rebuild that would fight the user. It resumes
  when a viewer joins (after auth) or a display is picked.

**Encoder:**

- **VideoToolbox teardown.** Sessions are invalidated before they're dropped,
  and frames are tagged through `sourceFrameRefcon`. An output that lands late
  is discarded instead of being taken for the next frame's.
- **Hardware H.264 size cap.** Measured on the M3:

  | Frame size | Encoder |
  |---|---|
  | 4096×2304, 4096×2560, 2304×4096, 2160×3840 | Hardware |
  | 4112 px wide or more | Silent fallback to software (5K ≈ 49 ms/frame, 6K ≈ 83 ms) |

  So `encode::h264::MAX_SIZE` is 4096×2304 on the long side, which is also the
  largest H.264 level 5.1/5.2 frame. For H.264, SCK scales larger displays down
  (`fit_within()` keeps the aspect ratio and even sides). A live check with a
  forced 1920 cap gave 1668×1080 frames. Pointer mapping is unaffected: input
  uses normalised coordinates onto the display bounds in points.

**Windows and code hygiene:**

- A clippy blocker (a test's `let Pixels::Bgra … else` is irrefutable on
  Windows) is allowed explicitly.
- `DxgiCapturer` now re-emits the same composed frame `Arc` while nothing
  changes, so the JPEG idle throttle applies on Windows too. This was
  type-checked through a Windows-cfg harness, not a real Windows build.
- `PixelBuffer::retain` is now an `unsafe fn`.
- `capture::pixel_buffer::tests::` joined the CI allowlist.

**Left open** ([deferred](deferred.md#phase-12-additions)):

- `DisplayInfo` still reports native pixels rather than the capped size.
- Stop Sharing was not clicked live (fake-backend tests only).
- A stale in-flight frame of the same size as the new display can still show
  briefly after a switch.

## macOS input

**Result: pass.** Real-hardware tests on the Mac:

- F19 held, then released on disconnect.
- Caps Lock synced through the HID lock state (restored afterwards).
- Volume up 0 → 6 (restored).
- 3 user-activity declares → 1 IOPM assertion, 0 after release.

**What shipped: keys** (`input/macos.rs`):

- Held keys and modifiers are released on disconnect. Modifier flags come from
  the held set.
- Caps Lock is synced as a *state* through `IOHIDSetModifierLockState`, from
  protocol modifier bits 4/5. The HID connection is reopened lazily every 5 s
  if missing.
  - Legacy senders without bit 5: a keydown toggles unless it's auto-repeat. A
    keyup toggles unless it closes an auto-repeated or <500 ms keydown. See
    [protocol.md](protocol.md).
- SecondaryFn/NumericPad flags on arrows, F-keys, Home/End/PgUp/PgDn,
  forward-delete and the keypad.
- Media keys (volume up/down/mute, play/pause, next, previous, fast-forward,
  rewind) are posted as NSEvent `systemDefined` subtype 8.

**What shipped: pointer and power:**

- Display bounds are refreshed at most every 250 ms, on mouse-down and on a
  display switch (`CGDisplayBounds` ≈ 36 µs).
- The `IOPMAssertionDeclareUserActivity` id is reused rather than stacked.

## Permissions and packaging

**Result: pass for criteria 5 and 6.** ⏳ The `.pkg` was not installed, so
the launchd-context prompts are still untested.

- **Prompt once:** the marker logic is unit-tested: asked once then never
  again, per launch context, an upgrade asks again, fallback locations,
  symlinks refused.
- **Status:** `tetherd status` reports both grants and upload-folder access.
- **No crash loop:** with Screen Recording missing, the pipeline retries at
  1 → 3 s, and the LaunchAgent restarts only on failure, at most every 30 s.
- **Package:** `build-pkg.sh` produced `dist/tether-host-macos-0.1.0.pkg`
  (arm64, macOS 26+), signed ad hoc as `com.tether.daemon`.
  `codesign --verify --strict` is clean.
- **Payload:** ⏳ built from this checkout, the payload has `._*` AppleDouble
  entries ([deferred](deferred.md#phase-12-additions)).

**What shipped: permissions and daemon** (`macos_perms.rs`, `control.rs`,
`files.rs`):

- The grants are requested once per (binary fingerprint, launch context). The
  launch contexts are launchd, ssh, terminal, app or job.
- The requests: `CGRequestScreenCaptureAccess`,
  `AXIsProcessTrustedWithOptions` with the prompt option and
  `CGRequestPostEventAccess`. Then the Accessibility pane opens.
- The marker `~/.config/tether/macos-permissions-prompted` is written *before*
  prompting. Fallbacks: `~/Library/Caches/tether`, then
  `$TMPDIR/tether-<uid>`; symlinks are refused. If no marker can be written,
  nothing is prompted and the log says what to grant.
- `tetherd status` now prints `Screen Recording: granted`,
  `Accessibility: granted` and `uploads <dir>: ok` (or `NO ACCESS — …` with
  what to do). The upload folder is probed at startup on a background thread;
  the probe is cached 30 s with one in flight.
- Clipboard calls run inside autorelease pools.

**What shipped: packaging** (`build.rs`, `packaging/macos`):

- `build.rs` embeds an Info.plist in the binary
  (`-sectcreate __TEXT __info_plist`): `CFBundleIdentifier`
  `com.tether.daemon`, `CFBundleName` Tether, versions from Cargo,
  `LSMinimumSystemVersion` 26.0, `NSScreenCaptureUsageDescription`,
  `NSDownloadsFolderUsageDescription`.
- `build-pkg.sh` always re-signs with `--identifier com.tether.daemon`:
  Developer ID when `TETHER_APP_IDENTITY` is set, ad hoc otherwise. It then
  runs `codesign --verify --strict`.
- It wraps the component in
  `productbuild --distribution packaging/macos/distribution.xml`, with
  `<os-version min="26.0"/>` and `hostArchitectures` from `lipo -archs`.
- The LaunchAgent restarts only after a failure (`KeepAlive` →
  `SuccessfulExit = false`), at most every 30 s (`ThrottleInterval`).

## CI and regressions

**Result: pass.** ⏳ Windows CI must confirm the cross-platform `RawFrame` and
`DxgiCapturer` changes. Neither was built for Windows here; the
`DxgiCapturer` change was type-checked through a Windows-cfg harness.

| Suite | Before | After |
|---|---|---|
| Controller tests | 141 | **314** |
| Rust workspace tests passing | 112 | **194** |

- 8 real-hardware tests are `#[ignore]`d.
- The Rust suite ran on this Mac as
  `cargo test --workspace -- --skip pasteboard_round_trip`. The skipped
  pasteboard test overwrites the real clipboard.
- `e2e_fake`'s ≥ 15 fps check failed once at 14.5 fps, on a debug build with
  the machine heavily loaded (load average 12–16). It passed on rerun.

**What shipped** (`.github/workflows/ci.yml`):

- macOS build and clippy now cover `--all-targets`.
- A new step runs tetherd's GUI-free lib tests by module allowlist, plus
  `--test connection --test webrtc_e2e`.
- `e2e_fake` stays off the macOS job. Its ≥ 15 fps gate is CPU-bound in a
  debug build, and the hosted macOS VM measured 7.5 fps. The Windows job still
  gates on it.
- VideoToolbox (`encode::h264::`) and pasteboard tests stay local-only.

## iOS controller

**Result: criteria 7–10 pass** in the iOS Simulator and unit tests. ⏳ Still
needed on a real device: a real keyboard, touch feel, the wake lock and the
standalone flows. Criterion 12 needs a real iPhone and iPad.

### Soft keyboard, toolbar and paste

**Result: pass (criterion 7).** ⏳ Not yet tried with a real keyboard.
Simulator evidence:

- Return/Backspace now arrive (before: lost).
- German autocorrect "teh" → "geh" arrived as 3× Backspace + `geh` + space. A
  QuickType completion did likewise.
- A 1,935-byte paste went out in two chunks, each within the 1,024-byte
  limit, and arrived byte-identical.
- Toolbar Ctrl+C logged as `ControlLeft↓ KeyC↓↑ ControlLeft↑`.
- Paste logged `clipboard "pasted from phone: Grüße €5"`, then
  `MetaLeft↓ KeyV MetaLeft↑`.

**What shipped: soft keyboard** (`keyboard.ts`):

- Return, Backspace, Tab, Esc, arrows, Delete, Home/End/PgUp/PgDn and iPad
  hardware Ctrl/Cmd shortcuts are taken from `keydown`. iOS fires no
  `beforeinput` for them, so they were lost before (confirmed in the
  Simulator).
- Autocorrect is applied by a grapheme-aware `diffEdit` against a rolling
  context of the field. Text over 1 KiB is chunked on code-point boundaries.
- Tapping the remote screen no longer dismisses the keyboard (canvas focus
  suppressed, `touchend` cancelled). The ⌨ button follows real focus,
  including iOS's Done.
- The hidden field has autocapitalize off and autocorrect on. Form fields
  have both off, with `inputmode`/`enterkeyhint`.
- Only the platform's own paste combo is left to the field (Ctrl+V is
  forwarded on iPad).

**What shipped: special-keys toolbar** (`extrakeys.ts`), above the keyboard:

- Sticky Ctrl / ⌥ / ⌘ (Win/Super against Windows/Linux hosts) / ⇧. A tap
  latches; a double-tap within 400 ms locks.
- Keys: Esc, Tab, Paste, arrows, Del, Home/End, PgUp/PgDn and F1–F12.
- Chords press the modifiers explicitly, because the Windows injector ignores
  the modifier mask.
- Chords bypass the Cmd→Ctrl remap by design, so Win is reachable.

**What shipped: phone Paste** (`clipboard.ts`):

- `navigator.clipboard.readText()` runs inside the tap; iOS shows its Paste
  confirmation.
- Then clipboard → host, and Cmd/Ctrl+V through `PasteFlow.pasteText`. This
  uses the same ordering and 150 ms delay for ≥32 KiB as the hardware combo.
- A textarea sheet is the fallback.

### Touch

**Result: pass (criterion 8).** Also covered by unit tests. ⏳ Feel on real
hardware is untested.

- A ±60 pt slow drag sends dy ∓50 (whole units of travel).
- A flick produced ~60 decaying scroll events over ~1.1 s.
- The "1×" chip resets zoom.
- A two-finger tap right-clicks at the first finger.

**What shipped: fixes** (`gestures.ts`, `viewer.ts`, `relayout.ts`):

- Two-finger scroll had the opposite sign to the wheel path; fixed.
- Slow drags lost or doubled motion to per-event rounding. They now send whole
  units of accumulated travel.
- A zoomed canvas no longer paints over the bar.

**What shipped: additions:**

- Momentum after a flick: τ 325 ms, minimum 0.05 px/ms, at most 2 s.
- Relayout on resize, orientation, `visualViewport` and `ResizeObserver`.
- The trackpad cursor is clamped to content ∩ visible stage, with edge
  auto-pan when zoomed.
- A "1×" chip resets zoom.
- A two-finger tap right-clicks at the first finger (direct) or the cursor
  (trackpad).

### Layout and phone bar

**Result: pass (criterion 9).**

- Before, the iOS keyboard hid ~43% of the screen: `visualViewport` went
  714 → 404 px while `innerHeight` stayed 714.
- The session now letterboxes above the toolbar.
- The compact bar, ⋯ sheet and auto-hide were exercised in the Simulator.

**What shipped: keyboard-aware viewport** (`viewport.ts`): `visualViewport` is
mirrored into `--vvh` / `--vv-top` / `--vv-bottom`. The session view
letterboxes above the toolbar instead of sitting under the keyboard.

**What shipped: compact phone bar** (`sessionbar.ts`, `autohide.ts`):

- Used on a coarse pointer or ≤600 px. The desktop bar is unchanged.
- One row plus a ⋯ overflow sheet: Files, display picker, fullscreen, audio,
  clipboard, Auto-hide, Settings.
- Auto-hide turns the bar into an overlay that slides away 3 s after the last
  interaction, with a pull-tab at the top (setting `tether-bar-autohide`).
- Taps on the remote screen are host input and don't reveal it.

**What shipped: CSS:**

- Tap highlight off.
- `touch-action` hygiene: a pinch on the bar no longer zooms the page.
- `@media (hover: hover)`.
- Safe-area insets, including landscape.
- Guards for older Safari: `HTMLDialogElement`, `MediaQueryList.addListener`,
  `:is()`.

### Lifecycle

**Result: pass (criterion 10).** ⏳ The wake lock and standalone flows still
need a real device.

- Reconnect observed ~0.2–0.3 s after resume.
- Liveness is unit-tested: a quiet link gets 2.5 s grace; traffic across the
  gap keeps it.
- The wake lock request on user activation is unit-tested.

**What shipped: connection** (`lifecycle.ts`, `wakelock.ts`, `save.ts`,
`pwa.ts`):

- Reconnect on `visibilitychange`, `online` and `pageshow`.
- Liveness comes from the transport's `lastRxAt` with a 2.5 s grace probe, so
  throttled desktop tabs are no longer torn down.
- The pairing prompt is no longer killed by the 10 s handshake timer.

**What shipped: device integration:**

- A Screen Wake Lock is held while connected. iOS grants it only on user
  activation, so it is (re)requested on `pointerup`/`keydown`.
- Downloads on iOS wait for a "Save…" tap that opens the share sheet.
- An install hint shows on the machine list when not standalone (dismissal
  persisted), and `navigator.storage.persist()` runs after pairing.
- Host audio `play()` is retried with a muted fallback.

## Live end-to-end run on a real Mac

**Result: pass.** All 10 checks pass, after fixing the 5 bugs the run found.
Input was verified with mouse moves only.

**Setup:**

- Release `tetherd` on the M3 (macOS 26.5), with a Chromium-based browser on
  the same machine as the controller.
- The session was locked at the start (lock screen with a moving aerial
  wallpaper). The user later unlocked it; after that, no more input was
  injected.

### Video and audio

| # | Check | Result |
|---|---|---|
| 1 | LAN WebSocket, JPEG | ✅ First frame 111 ms after connect. 27–29 fps, ~14 MB/s, ~505 KB/frame. Host CPU 35–44%, RSS 82 MB. |
| 2 | WebRTC + H.264 (zero-copy) | ✅ Pass after the [fixes below](#bugs-found-and-fixed). WebCodecs decode at 29.4 fps. First frame 115–1674 ms over 6 connects (always ≤ 2 s). |
| 3 | Audio | ✅ Opus `inbound-rtp` 50.2 packets/s, 0 lost (audio element muted). |
| 4 | Idle | ✅ Keyframe intervals 1942–1998 ms. Longest frame gap 278 ms, ~1.1 KB frames. |

- Check 1 ran on the animated lock screen. tetherd announced 3420×2214, which
  is the display (1710×1107 pt @2×).
- Check 2: the machine list showed the host Online. Colours match JPEG (sky
  74,130,184 vs 77,130,184).
- Check 3: silence ~39 B per 250 ms; Tink/Glass 2.2–3.1 KB per 250 ms.
- Check 4: the 278 ms gap is the 250 ms repeat, on the unlocked, mostly static
  desktop.

### Input, files and clipboard

| # | Check | Result |
|---|---|---|
| 5 | Mouse | ✅ Host cursor at (855, 553) and (427, 277), against expected (855, 553.5) and (427.5, 278.3). Moves only. |
| 6 | Files | ✅ Uploads: 300 KB over LAN, 3 MB over WebRTC (118 ms). Downloads: 2.5 MB over LAN and WebRTC (102 ms). SHA-256 matches throughout. |
| 7 | Clipboard | ✅ A `pbcopy` marker on the host reached the controller (📋 chip) over LAN and WebRTC. |

- Check 6: uploads went into a scratch `--upload-dir`. Files were supplied by
  script (a `DataTransfer`), not the native file chooser.
- Check 7: the clipboard was restored afterwards.

### Reconnect, control channel and resources

| # | Check | Result |
|---|---|---|
| 8 | Reconnect | ✅ tetherd killed and restarted: the controller reconnected without re-pairing. |
| 9 | Control channel | ✅ `tetherd status`, `tetherd pair` and `devices list/revoke` worked over the control socket. |
| 10 | Resources | ✅ H.264 over WebRTC: tetherd 3–5% CPU, RSS 33–38 MB (~9–10% CPU before the fixes). |

- Check 8: LAN reconnected after its ~5 s backoff, WebRTC immediately. A page
  reload reconnects with the stored token.
- Check 9: `tetherd status` showed Screen Recording granted, Accessibility
  granted, upload dir ok, audio on, and the display.
- Check 10: H.264 over LAN used 1–4% CPU and 33 MB. With no controller
  connected: ~4–5%.

### Bugs found and fixed

Commit `d649de8`, each with tests:

1. **~200 ms of decoder latency.** VideoToolbox's SPS (Main, level 5.1) has no
   VUI bitstream restriction, so Chrome's decoder held 6 frames before output.
   `encode/sps.rs` rewrites the SPS to declare `max_num_reorder_frames = 0`.
   Decode → output median: ~204 ms → ~3 ms. An SPS it can't parse passes
   through unchanged. Verified with ffmpeg `trace_headers`.
2. **Garbage after a reconnect.** The new session's mid-GOP frames went into
   the old decoder state until the next keyframe. Every reconnect now resets
   the decoder and waits for a keyframe. A gap in the host's frame numbering
   does the same.
3. **`--pair` on a fresh host** (no `paired.json`) never asked for the code.
   An armed code now enables the auth gate.
4. **LAN pairing timed out in 5 s**, before a person could type the code. It
   now gets the same 90 s window as WebRTC.
5. **A departed WebRTC controller kept its slot** until the next offer or a
   restart. The slot is shared with LAN, so status said 1/1 connected. The
   session now ends when the peer connection fails or closes (~30 s ICE
   timeout).

### Found, not fixed

See [deferred](deferred.md#phase-12-additions):

- Re-pairing over the other transport invalidates the first token.
- The latest-only frame sender can skip H.264 frames on slow links.
- A new controller waits up to 2 s for the periodic keyframe.
- The SPS rewrite doesn't cover the Windows OpenH264 path.

## iOS Simulator run

**Result:** the controller was driven in the iOS 26.5 Simulator (iPhone 17
Pro) against `fake_host`. Its results are under
[iOS controller](#ios-controller). The iPad Simulator pass stopped at an
unanswered permission prompt.

**`fake_host`** (`crates/tetherd/examples/fake_host.rs`, a dev tool):

- The real `Server` + pipeline behind the LAN WebSocket transport.
- A synthetic screen: a grid, a sweeping bar, and a marker where the host
  thinks the pointer is.
- An injector that only logs. Every input event is a JSON line, clipboard text
  is logged, uploads go to a temp dir, and nothing is ever posted to the OS.
- It made the Simulator pass possible on a machine whose real input must not
  be touched.

**Not covered:** Esc closing the sheets. The Simulator tool has no hardware
Esc.

## How to reproduce

**Live run:**

1. Build the release binaries:

   ```sh
   cargo build --release -p tetherd -p tether-signal
   ```

2. Serve the controller:

   ```sh
   (cd controller && npm ci && npx vite --port 5174 --strictPort --host 127.0.0.1)
   ```

3. Start the signal server:

   ```sh
   target/release/tether-signal --secret test --bind 127.0.0.1 --port 7889
   ```

4. Start the host. Isolate its state with `HOME=$(mktemp -d -t tether)`. Keep
   the path short: the control socket path must stay under 104 bytes.

   ```sh
   # LAN, JPEG:
   target/release/tetherd --bind 127.0.0.1 --port 7898 --allow 127.0.0.1 --codec jpeg --upload-dir <dir>
   # WebRTC, H.264 + audio:
   target/release/tetherd --signal 127.0.0.1:7889 --secret test --device-id e2e-mac \
       --codec h264 --audio --pair --upload-dir <dir>
   ```

5. Open the controller:
   - LAN: `http://127.0.0.1:5174/?host=127.0.0.1:7898`
   - WebRTC: `http://127.0.0.1:5174/?target=<device-id>`

**Inspect the SPS:**

1. Build the example: `cargo build --release -p tetherd --example h264_smoke`.
2. Dump a stream and print its headers:

   ```sh
   TETHER_H264_DUMP=x.h264 target/release/examples/h264_smoke 4000 3
   ffmpeg -i x.h264 -c copy -bsf:v trace_headers -f null -
   ```

**Test suites:**

1. Rust, on a Mac: `cargo test --workspace -- --skip pasteboard_round_trip`.
2. Controller, in `controller/`: `npm test`.

## Still needs a person

**iPhone and iPad:**

- Gesture tuning and haptics on a real iPhone and iPad.
- Autocorrect on a real keyboard.
- Add to Home Screen, the standalone flows, and the wake lock in standalone
  mode.
- The iPad Simulator pass (stopped at an unanswered permission prompt), and Esc
  closing the sheets (the Simulator tool has no hardware Esc).
- macOS audio listened to on a phone.

**Mac hardware and install:**

- Installing the `.pkg` and answering the launchd-context permission prompts.
- Clicks and keyboard input injected during a live session on the Mac. The
  live run injected mouse moves only; keys were checked by the real-hardware
  module tests.
- Multi-display switching and hot-unplug on real multi-monitor Mac hardware.
- A real display-mode change, and a display wider than 4096 px (scaling was
  checked with a forced 1920 cap).
- A live click on Stop Sharing.

**Signing, Windows and network:**

- Developer ID signing and notarization (needs a paid identity).
- Windows CI confirming the cross-platform `RawFrame` and `DxgiCapturer`
  changes.
- A live WAN + TURN run.
