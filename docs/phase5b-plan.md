# Phase 5b Plan — multi-monitor, multiple controllers, client cursor

The remaining hardening items deferred across earlier phases. Three mostly
independent features.

---

## 1. Multi-monitor (shared active display, switchable)

The capture pipeline is one capturer → one frame `watch` channel broadcast to
all sessions. Per-controller *independent* displays would need N capturers +
N encoders — a big change, deferred. Phase 5b does **shared active display
with switching**: the host captures one display; any controller can list the
displays and switch which one is captured; the change is announced (new
`Resolution`) to everyone.

- **Protocol:** `Displays` (host→controller): list of `{id, name, width,
  height, active}`. `SelectDisplay { id }` (controller→host).
- **Host:** enumerate via ScreenCaptureKit; the capture thread can rebuild its
  `SCStream` against a new display on request (the design workflow confirms
  whether the filter can be swapped live or the stream must be recreated —
  recreate is fine, it's rare). `Displays` is sent after the handshake and on
  change; `SelectDisplay` routes to the capture thread.
- **Controller:** a display dropdown in the bar, populated from `Displays`.

**Verification gap:** this machine has one display, so enumeration returns 1
and switching is a no-op. The enumerate/switch *code path* is exercised by
unit tests + the single-display live run; true multi-display switching is a
documented human check (like TURN traversal).

## 2. Multiple controllers (opt-in, capped)

Phase 5 hardened a single-session lock (one controller, cross-transport).
Phase 5b makes it a **counted permit**: `--max-controllers N` (default **1**,
so current behavior is unchanged unless opted in). With N>1:

- The shared `AtomicBool` becomes an `AtomicUsize` count (or a semaphore);
  acquire fails past N.
- Frame/`Resolution`/clipboard broadcast already fans out to N sessions (it's
  a `watch` channel) — no change.
- **Input model:** all connected controllers may view *and* inject; input
  serializes through the single injector channel (last-event-wins). This is
  the "anyone in the room can grab the mouse" collaboration model. Concurrent
  input from two people interleaves — documented, not arbitrated. (A held
  "control token" with handoff is a later refinement.) Safe because every
  controller is an authenticated/paired device.
- An adversarial design check (workflow) reviews the relaxation for races
  before building, since we're loosening a freshly-hardened invariant.

## 3. Client-drawn cursor (trackpad mode)

Trackpad mode moves a client-side virtual cursor and sends absolute moves; the
only on-screen feedback is the host cursor composited into the capture, which
lags one frame and is imprecise for aiming. Draw the virtual-cursor position
as a crisp overlay dot on the canvas. Controller-only, **no protocol change**.
The gesture machine already tracks `(vx, vy)`; expose it so the viewer can
draw it (correct under pinch zoom, since it's in the same client space the
mapping uses). Touch (absolute) and mouse modes don't need it.

## 4. Module order

1. **M1 — multi-monitor:** protocol messages + host enumerate/switch +
   controller picker. Tests: protocol vectors, display-switch routing.
2. **M2 — multiple controllers:** counted permit + `--max-controllers`;
   concurrent-session integration test.
3. **M3 — client cursor:** virtual-cursor overlay; zoom-correct positioning
   test.
4. **M4 — gate + adversarial review.**

## 5. Gate criteria (proposed)

1. Controller lists the host's displays and switches the active one; everyone
   sees the new display + `Resolution` (single-display: enumerate shows 1,
   switch is a no-op — full switch is a human check on multi-display hardware).
2. With `--max-controllers 2`, two controllers connect concurrently, both see
   frames, either can inject; a third is refused. Default (1) still refuses a
   second.
3. Trackpad mode shows a client cursor that tracks the virtual position with
   no frame lag and stays correct under pinch zoom.
4. No regressions: full Rust + TS suites green; existing single-controller and
   pairing paths unchanged.

## 6. Risks

- Relaxing the single-session lock (just hardened) — design-reviewed before
  build; default stays 1.
- SCStream live display switching may need stream recreation (rebuild on the
  capture thread) — confirmed in the design step; mid-switch a frame gap is
  acceptable.
- Multi-display untestable on a one-display machine — documented.

---

**Status: proceeding (scope approved via "run 5b").**
