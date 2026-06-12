# Phase 4 Plan — mobile / touch controller UX

**Goal:** Controlling the Mac from an iPad or iPhone in Safari feels
deliberate, not accidental: touch gestures map to pointer actions
predictably, text enters through the on-screen keyboard (emoji included),
pinch zooms the view locally for precision, and the UI fits a phone.

Touch has no hover, no buttons, no wheel, and a fat imprecise contact patch —
this phase is mostly a careful translation layer, almost entirely
controller-side. One small protocol addition (below) fixes text input
properly instead of faking it.

---

## 1. Key decisions

### 1.1 Two pointer modes, user-toggleable

| Mode | Mapping | Default on |
|---|---|---|
| **Touch** (absolute) | Tap where you want to click; finger position = cursor position | Tablets (≥768px viewport) |
| **Trackpad** (relative) | The screen is a big trackpad: drags move a client-side virtual cursor (drawn as an overlay dot), tap clicks at the cursor | Phones |

The wire protocol stays absolute either way — trackpad mode just accumulates
deltas into a virtual cursor position client-side and sends absolute moves.
No protocol change.

### 1.2 Gesture vocabulary (both modes unless noted)

| Gesture | Action |
|---|---|
| 1-finger tap | Left click |
| Double tap | Double click |
| Long-press (≈500 ms, still) | Right click |
| Long-press then drag | Hold left button + drag (text selection, window moves) |
| 1-finger drag | Move cursor (trackpad) / move cursor absolutely (touch) |
| 2-finger drag | Scroll (content follows fingers) |
| 2-finger tap | Right click (faster alternative to long-press) |
| Pinch | **Local** view zoom + pan (see 1.3) — never forwarded |

Implemented as a pure, exhaustively unit-tested state machine
(`gestures.ts`) that consumes pointer-event streams and emits the same
`InputEvent`s the mouse path produces. Mouse/trackpad input on desktops
(`pointerType === "mouse"`) bypasses it entirely — zero regression surface.

### 1.3 Pinch zoom is local

Hitting a 20-point close button on a 6" phone showing a 16" desktop needs
magnification. Pinch scales/translates the canvas with CSS transforms;
`getBoundingClientRect` reflects transforms, so the existing
`normalizedFromClient` mapping keeps working — with a unit test pinning
click-accuracy-while-zoomed. Double-tap with two fingers resets zoom.

### 1.4 Soft-keyboard text needs one protocol message: `0x06 TextInput`

iOS soft keyboards don't produce usable `KeyboardEvent.code` values (often
`"Unidentified"`), and mapping typed characters back to physical key codes
breaks on non-US host layouts and can't express emoji at all. The right fix
is text injection, which macOS supports natively:

```
0x06 — TextInput (controller → host)
0  …  UTF-8 text, ≤ 1 KiB per message (committed text, usually 1–3 chars)
```

Host injects via `CGEvent` + `CGEventKeyboardSetUnicodeString` —
layout-independent, unicode-complete. Unknown-type skip keeps old peers safe;
no version bump.

Controller side: a keyboard button (⌨) focuses a hidden input that summons
the OS keyboard; `beforeinput` events are harvested — `insertText` /
composition commits become `TextInput`, `deleteContentBackward` becomes a
Backspace key tap, `insertLineBreak` becomes Return. Hardware keyboards keep
the existing code-based path untouched (it already works on iPad Magic
Keyboard).

### 1.5 Phone-sized UI

The bar collapses into a drawer on narrow viewports; fullscreen button;
`viewport-fit=cover` + safe-area insets; `user-scalable=no` so Safari's page
zoom never fights the canvas pinch; home-screen web-app metas for a
chromeless fullscreen experience.

## 2. Module order

1. **M1 — TextInput protocol + host injection.** Message 0x06 both codecs
   (cross-pinned vectors), `CGEventKeyboardSetUnicodeString` injection in
   MacInjector, session/webrtc relay. Tests: vectors, injector wiring;
   live text round-trip in M4.
2. **M2 — gesture engine.** `gestures.ts` state machine + virtual-cursor
   model, integrated behind `pointerType` dispatch in input.ts. Tests: tap /
   double-tap / long-press / drag / two-finger scroll / pinch classification,
   timing edges, mode differences.
3. **M3 — mobile UI.** Drawer bar, keyboard button + hidden-input harvesting,
   pinch zoom/pan transforms, fullscreen, metas. Tests: zoomed-click mapping;
   the rest is live verification.
4. **M4 — gate.** Synthetic touch streams through the real input path in the
   preview (pointer events with `pointerType: "touch"` are constructible);
   text round-trip incl. emoji via TextEdit; desktop regression suite; UI at
   390×844. Gate write-up + deferred.md. Real-iPad pass remains the human
   check.

## 3. Gate criteria (proposed)

1. Tap, double-tap, long-press right-click, long-press drag, and two-finger
   scroll all produce the correct host-side events (engine unit tests +
   live synthetic-touch verification).
2. Pinch zooms locally; a click at a zoomed-in target lands on the exact
   host coordinate (mapping test).
3. Soft-keyboard text — including emoji — lands on the host via `TextInput`
   (live TextEdit round-trip).
4. Desktop behavior unchanged: full existing test suites green, live mouse
   path regression.
5. Controller UI usable at 390×844: nothing clipped, all controls reachable.

## 4. Risks

- **iOS Safari pointer-event quirks** (gesture vs pointer event ordering,
  pointer capture on touch). Mitigation: pointer events only,
  `touch-action: none`, and the engine is input-agnostic enough to feed from
  touch events if a Safari version demands it.
- **`beforeinput` coverage** varies by keyboard (Android IMEs especially).
  Phase 4 targets iOS Safari; Android is logged as deferred.
- **Two-finger tap vs scroll discrimination** needs tuned thresholds; the
  state machine's tests encode the timings so regressions are visible.

Out of scope, logged on completion: Android IME support, momentum scrolling,
haptics, Pencil hover, ctrl→cmd remap decision (carried from Phase 3),
client-drawn cursor (still Phase 5).

---

**Status: awaiting architect approval. No Phase 4 code written.**
