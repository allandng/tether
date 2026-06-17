# Phase 4 gate results — 2026-06-17

Touch/mobile controller UX. Verification was done with synthetic `PointerEvent`s
(`pointerType: "touch"`) and `beforeinput` events dispatched into the live
controller over a LAN connection to `tetherd` — the gesture logic and the full
controller→wire→host chain are exercised, but a **real iPad/iOS Safari pass
remains a human check** (synthetic events can't reproduce every Safari quirk).

| Gate criterion | Result | Evidence |
|---|---|---|
| Tap / double-tap / long-press right-click / long-press-drag / two-finger scroll produce the right host events | **PASS** | `gestures.test.ts` (16 cases) pins every gesture + edges. Live: a touch tap emits `mousemove+mousedown+mouseup` on the wire; a two-finger drag emits a stream of `scroll`. Host mouse injection itself was live-verified in Phase 1. |
| Pinch zooms locally; a click at a zoomed target lands on the exact host pixel | **PASS** | Live: synthetic pinch → canvas `scale(4)` (clamped) with pan-clamped translate; pinch back → identity. `zoom.test.ts` proves the mapping is transform-invariant (tap on a 2× view maps to content center ±2/65535). |
| Soft-keyboard text incl. emoji lands on the host | **PASS** | Live end-to-end: soft-keyboard `beforeinput` ("Tether 🎯 señor" + Enter) → `TextInput` over the wire → host `CGEvent::set_string` → TextEdit document read back as exactly **`Tether 🎯 señor`**. This exercised the M1 injection path on the host for the first time. |
| Desktop behavior unchanged | **PASS** | 49 Rust + 81 TS tests green. Live: a synthetic mouse click still emits `move+down+up`, bypassing the gesture engine entirely (`pointerType === "mouse"`). |
| UI usable at phone width | **PASS** | At 375×812 the bar wraps (mode + host field full-width, controls below), the remote renders letterboxed, nothing clipped. ⌨/🖱/⛶ buttons gate on a coarse pointer (correctly hidden on desktop). |

## Design decisions made during build (recorded for the architect)

- **Single taps click eagerly** (no 300 ms double-tap defer). A remote desktop's
  dominant interaction is the single click; deferring it would lag every one.
  A double-tap emits an explicit `doubleClick` at the second point — and a
  leading single before a double is exactly how a physical mouse behaves, so
  it's faithful, not spurious.
- **Long-press is deferred**: the timer arms the gesture but emits nothing;
  lifting → right click, moving → left-button hold+drag. So a selection-drag
  never pops a context menu first (the spec's simpler variant did).
- **Pinch transforms the `<canvas>` directly** (`transform-origin: 0 0`).
  Because `displayedRect()` measures that same element via
  `getBoundingClientRect` (post-transform), the existing coordinate mapping is
  a literal no-op under zoom — no special-casing.
- **Text injection has no modifier flags**: held modifiers would corrupt typed
  Unicode, so `inject_text` posts the string with clean flags.

## Remaining human checks (real device)

1. iPad/iPhone Safari: confirm pointer-event delivery, the gesture feel
   (thresholds may want tuning on real hardware), and that the soft keyboard
   summons and stays up.
2. Scroll direction and pinch sensitivity on a real trackpad/touchscreen
   (constants are config-injectable for tuning).
3. Fullscreen on iPhone (the Fullscreen API is iPad-only on iOS; the button is
   gated on `document.fullscreenEnabled`).
