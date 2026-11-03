# Phase 12 Plan — macOS + iOS completion

**Goal:** make both ends of the Mac-and-phone pairing hold up in real use.
The product goal names a Mac and a phone first.

**Gaps to close**, as the [roadmap](roadmap.md) listed them after Phases 7–11:

- the macOS system-audio capturer;
- a run of the host on a real Mac;
- an iPhone/iPad controller that is usable rather than merely functional.

---

## 1. What was left

- **macOS audio was never built.** Phase 10 shipped the Windows half. `--audio`
  on macOS logged that it was unavailable and carried on without it.
- **The macOS host had only been compiled since Phase 7.** CI proved it builds
  and is clippy-clean. But its capture, encode and input code had not run on a
  Mac since the Windows work rearranged everything around it. The Phase 9
  permission flow and `.pkg` had never run at all.
- **The phone controller was verified against synthetic touch events only.**
  Phase 4's soft keyboard, gestures and phone UI had never met iOS's real
  keyboard, visual viewport or app lifecycle.

## 2. Scope

**macOS host (`tetherd`): media**

- System audio through ScreenCaptureKit into the existing Opus thread and
  WebRTC audio track. No virtual device, no driver, and no permission beyond
  Screen Recording.
- Capture robustness: a steady frame trickle on a static screen. Recovery from
  stream errors, display sleep and display-mode changes without restarting the
  daemon.
- A zero-copy capture → encode path (ScreenCaptureKit's IOSurface buffers
  straight into VideoToolbox), with the cost measured.

**macOS host (`tetherd`): input, permissions, packaging**

- Input parity with the Windows host: held keys released on disconnect,
  Caps Lock, media/volume keys, navigation-key flags.
- The permission flow and daemon behaviour under launchd: prompt once, report
  grants in `tetherd status`, no crash loop when a grant is missing.
- Packaging: a stable signing identifier, and a product archive that refuses a
  Mac it can't run on.
- CI building every target.

**iOS controller (`controller/`)**

- Soft keyboard: the keys iOS never reports through `beforeinput`, real
  autocorrect, long text.
- A special-keys toolbar (Esc, Tab, arrows, F-keys, sticky modifiers) and a
  Paste button.
- A layout that survives the on-screen keyboard and gives the screen back on
  a phone.
- Touch fixes, momentum and a zoom reset.
- Lifecycle: resume after backgrounding, wake lock, saving downloads, keeping
  the browser identity.

**Tooling**

- A safe fake host to drive the controller from the iOS Simulator, on a
  machine whose real input must not be touched.

## 3. Out of scope

- **Developer ID signing and notarization.** This still needs a paid Apple
  identity. The scripts already use one when it's provided.
- **Windows changes** beyond what keeps it compiling against shared types.
- **A native iOS app.** The controller stays a PWA.

## 4. Gate criteria (proposed)

**macOS media**

1. `--audio` on a macOS host streams system audio alongside video, at the
   Windows path's 50 Opus packets/s. Screen Recording only; no microphone
   permission.
2. macOS capture never goes silent. A static screen keeps producing frames and
   keyframes. Stream errors, display sleep and mode changes recover without a
   restart.
3. Capture → encode cost at native resolution is measured on the Mac and does
   not regress against Phase 2.

**macOS input, permissions, packaging**

4. macOS input matches the Windows host: held keys released on disconnect,
   Caps Lock, media keys, navigation keys.
5. Permissions are asked once per installed binary and launch context.
   `tetherd status` shows the grants. A missing grant is a slow retry, not a
   crash loop.
6. The `.pkg` builds with a stable signing identifier and refuses a Mac it
   can't run on.

**iOS controller**

7. On an iPhone soft keyboard, every key and autocorrect edit arrives as typed,
   and the keys a phone keyboard lacks are reachable.
8. Touch: scrolling matches the wheel, slow drags keep their motion, zoom
   resets in one tap, and a right-click lands where it was aimed.
9. The remote screen stays visible above the keyboard. On a phone, the bar
   gets out of the way.
10. Returning to a backgrounded app reconnects promptly without tearing down
    healthy sessions. A phone that's watching doesn't auto-lock.

**End to end**

11. A live end-to-end run on a real Mac: a controller drives tetherd (video,
    input, clipboard, audio, files).
12. A real iPhone and iPad pass.
13. No regressions: Rust and controller suites, both CI jobs.

## 5. Risks

- **ScreenCaptureKit through a young crate.** `screencapturekit` 7.0 is the
  only safe binding. Anything it gets wrong surfaces as a crash or a silent
  stream, not a compile error.
- **TCC on an unsigned binary.** Ad-hoc builds get a fresh identity per build,
  so grants that worked yesterday may not work today. The prompt logic must
  not turn that into a prompt loop under launchd.
- **The Simulator is not a phone.** It runs WebKit's real keyboard and
  viewport code, but not real fingers, haptics or the installed-app lifecycle.
  Criterion 12 stays with a human.

---

**Status: done (real iPhone/iPad pass pending). See [phase12-gate-results.md](phase12-gate-results.md).**
