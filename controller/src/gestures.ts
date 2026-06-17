// Touch gesture engine: a pure, clock-injected state machine that turns a
// stream of pointer events into remote-desktop intents. It never calls
// performance.now()/setTimeout — the caller drives the clock via tick(t) and
// schedules the single pending deadline the machine returns. This makes every
// gesture path deterministically unit-testable.
//
// Coordinates are CLIENT pixels throughout, so thresholds stay physical (a
// finger moves the same screen distance regardless of canvas zoom). The
// caller's sink normalizes emitted points through the displayed video rect
// (which reflects CSS transforms), keeping pinch-zoom click mapping a no-op.
//
// Two deliberate departures from a textbook design, both for remote-desktop
// feel:
//   * Single taps click EAGERLY (no double-tap defer) — zero latency on the
//     dominant interaction; a double-tap emits an explicit doubleClick at the
//     second point, mirroring how a physical mouse's first click also lands
//     before the OS recognizes the double.
//   * Long-press is DEFERRED: the timer arms the gesture but emits nothing;
//     lifting still → right click, moving instead → left-button hold+drag, so
//     a long-press-drag (text selection) never pops a context menu first.

export type Mode = "touch" | "trackpad";

export interface GestureSink {
  moveAbs(x: number, y: number): void; // client px; caller normalizes
  down(button: number): void; // 0 left, 2 right
  up(button: number): void;
  click(button: number): void;
  doubleClick(): void;
  scroll(dx: number, dy: number): void;
  /** LOCAL view zoom — never forwarded to the host. `scale` is cumulative
   * from the start of the current pinch; `focal` is the live centroid. */
  zoom(scale: number, focalX: number, focalY: number): void;
  /** The current pinch ended — commit the live zoom transform. */
  zoomEnd(): void;
}

export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface Thresholds {
  tapMaxMs: number;
  doubleTapMs: number;
  longPressMs: number;
  tapSlopPx: number;
  doubleTapSlopPx: number;
  twoTapMaxMs: number;
  twoMoveSlopPx: number;
  pinchDistPx: number;
  scrollGain: number;
  trackpadAccel: number;
}

export const DEFAULT_THRESHOLDS: Thresholds = {
  tapMaxMs: 250,
  doubleTapMs: 300,
  longPressMs: 500,
  tapSlopPx: 10,
  doubleTapSlopPx: 24,
  twoTapMaxMs: 250,
  twoMoveSlopPx: 10,
  pinchDistPx: 16,
  scrollGain: 1.0,
  trackpadAccel: 1.0,
};

export interface GestureConfig {
  mode: Mode;
  bounds: Rect;
  seedCursor?: { x: number; y: number };
  thresholds?: Partial<Thresholds>;
}

const enum State {
  Idle,
  PendingTap,
  LongPressArmed, // long-press timer fired; not yet emitted (defer)
  Drag, // single finger moved before long-press: cursor move, no button
  HoldDrag, // long-press then moved: left button held
  TwoPending,
  TwoScroll,
  TwoPinch,
  TwoDraining, // a two-finger gesture ended one finger; ignore survivor
}

interface Pointer {
  startX: number;
  startY: number;
  startT: number;
  x: number;
  y: number;
  maxDist: number;
}

const dist = (ax: number, ay: number, bx: number, by: number) =>
  Math.hypot(ax - bx, ay - by);

export class GestureMachine {
  private state = State.Idle;
  private readonly pointers = new Map<number, Pointer>();
  private order: number[] = []; // pointer ids in arrival order
  private mode: Mode;
  private readonly t: Thresholds;
  private bounds: Rect;

  private vx: number;
  private vy: number;

  private longPressDeadline: number | null = null;
  private lastTap: { x: number; y: number; t: number } | null = null;

  // two-finger episode start references
  private d0 = 0;
  private cx0 = 0;
  private cy0 = 0;

  constructor(
    private readonly sink: GestureSink,
    config: GestureConfig,
  ) {
    this.mode = config.mode;
    this.bounds = config.bounds;
    this.t = { ...DEFAULT_THRESHOLDS, ...config.thresholds };
    const seed = config.seedCursor ?? {
      x: config.bounds.left + config.bounds.width / 2,
      y: config.bounds.top + config.bounds.height / 2,
    };
    this.vx = seed.x;
    this.vy = seed.y;
  }

  setMode(mode: Mode): void {
    // Don't swap coordinate spaces mid-gesture: a HoldDrag anchored in finger
    // space would teleport to the stale virtual cursor. Defer until idle.
    if (this.state !== State.Idle) return;
    this.mode = mode;
  }

  setBounds(bounds: Rect): void {
    this.bounds = bounds;
  }

  reset(): void {
    // Never leave a button stuck or a pinch uncommitted on an abrupt reset
    // (transport teardown, reconfigure) — mirror onCancel.
    this.releaseHeld();
    if (this.state === State.TwoPinch) this.sink.zoomEnd();
    this.state = State.Idle;
    this.pointers.clear();
    this.order = [];
    this.longPressDeadline = null;
    this.lastTap = null;
  }

  onDown(id: number, x: number, y: number, t: number): number | null {
    this.pointers.set(id, { startX: x, startY: y, startT: t, x, y, maxDist: 0 });
    this.order.push(id);

    if (this.pointers.size >= 2) {
      if (this.canBecomeTwoFinger()) {
        this.beginTwoFinger();
        return this.deadline();
      }
      // A finger landed in a state that can't pair up (HoldDrag, or already
      // 3+ fingers). Release any held button so it can't get stuck, and route
      // to draining rather than restarting as a fresh single-finger gesture.
      this.releaseHeld();
      this.state = State.TwoDraining;
      this.longPressDeadline = null;
      return this.deadline();
    }

    // first finger down
    this.state = State.PendingTap;
    this.longPressDeadline = t + this.t.longPressMs;
    return this.deadline();
  }

  onMove(id: number, x: number, y: number, t: number): number | null {
    const p = this.pointers.get(id);
    const prevX = p ? p.x : x;
    const prevY = p ? p.y : y;
    if (p) {
      p.x = x;
      p.y = y;
      p.maxDist = Math.max(p.maxDist, dist(x, y, p.startX, p.startY));
    }

    switch (this.state) {
      case State.PendingTap: {
        if (p && p.maxDist > this.t.tapSlopPx) {
          this.longPressDeadline = null;
          this.lastTap = null; // a drag started — don't fuse a prior tap into a double
          this.state = State.Drag;
          this.advanceCursor(prevX, prevY, x, y);
          this.emitMove(p);
        }
        break;
      }
      case State.LongPressArmed: {
        if (p && p.maxDist > this.t.tapSlopPx) {
          // becomes a left-button hold+drag; no right-click was emitted.
          // Anchor the press at where the finger was held (touch) or the
          // virtual cursor (trackpad, unmoved during the hold), then drag.
          this.lastTap = null;
          this.state = State.HoldDrag;
          const anchor = this.mode === "touch"
            ? { x: p.startX, y: p.startY }
            : { x: this.vx, y: this.vy };
          this.sink.moveAbs(anchor.x, anchor.y);
          this.sink.down(0);
          this.advanceCursor(prevX, prevY, x, y);
          this.emitMove(p);
        }
        break;
      }
      case State.Drag:
      case State.HoldDrag: {
        if (p) {
          this.advanceCursor(prevX, prevY, x, y);
          this.emitMove(p);
        }
        break;
      }
      case State.TwoPending:
        this.classifyTwoFinger();
        break;
      case State.TwoScroll:
        this.emitScroll();
        break;
      case State.TwoPinch:
        this.emitZoom();
        break;
      default:
        break;
    }
    return this.deadline();
  }

  onUp(id: number, x: number, y: number, t: number): number | null {
    const p = this.pointers.get(id);
    if (!p) return this.deadline(); // unknown/already-removed id: never touch state
    p.x = x;
    p.y = y;
    p.maxDist = Math.max(p.maxDist, dist(x, y, p.startX, p.startY));

    switch (this.state) {
      case State.PendingTap: {
        // tap if quick and under slop
        if (t - p.startT <= this.t.tapMaxMs && p.maxDist <= this.t.tapSlopPx) {
          this.emitTap(p, t); // (re)sets lastTap
        } else {
          // slow press that never moved and never fired long-press: treat as a
          // click, but not a tap that could fuse into a later double.
          const pt = this.movePoint(p);
          this.sink.moveAbs(pt.x, pt.y);
          this.sink.click(0);
          this.lastTap = null;
        }
        this.endSingle(id);
        break;
      }
      case State.LongPressArmed: {
        // held still then lifted → right click
        if (p) {
          const pt = this.movePoint(p);
          this.sink.moveAbs(pt.x, pt.y);
          this.sink.click(2);
        }
        this.endSingle(id);
        break;
      }
      case State.Drag: {
        this.endSingle(id); // cursor move ended; no button
        break;
      }
      case State.HoldDrag: {
        this.sink.up(0); // release held left button
        this.endSingle(id);
        break;
      }
      case State.TwoPending: {
        // first of the two fingers lifting ends a potential two-finger tap;
        // both pointers are still in the map, so classify before removing.
        if (this.twoFingerWasTap(t)) {
          const c = this.centroidOfStarts();
          const pt = this.mode === "touch" ? c : { x: this.vx, y: this.vy };
          this.sink.moveAbs(pt.x, pt.y);
          this.sink.click(2);
        }
        this.state = State.TwoDraining;
        this.removePointer(id);
        if (this.pointers.size === 0) this.toIdle();
        break;
      }
      case State.TwoScroll:
      case State.TwoPinch:
      case State.TwoDraining: {
        if (this.state === State.TwoPinch) this.sink.zoomEnd();
        this.removePointer(id);
        if (this.pointers.size === 0) this.toIdle();
        else this.state = State.TwoDraining;
        break;
      }
      default: {
        this.removePointer(id);
        if (this.pointers.size === 0) this.toIdle();
      }
    }
    return this.deadline();
  }

  onCancel(id: number): number | null {
    this.releaseHeld(); // never leave a button stuck
    if (this.state === State.TwoPinch) this.sink.zoomEnd();
    this.removePointer(id);
    if (this.pointers.size === 0) this.toIdle();
    else if (this.state !== State.Idle) this.state = State.TwoDraining;
    return this.deadline();
  }

  /** Fire any elapsed deadline; returns the next one (or null). */
  tick(t: number): number | null {
    if (
      this.state === State.PendingTap &&
      this.longPressDeadline !== null &&
      t >= this.longPressDeadline
    ) {
      const p = this.firstPointer();
      if (p && p.maxDist <= this.t.tapSlopPx) {
        this.state = State.LongPressArmed; // defer the emit (see header)
        this.lastTap = null; // a long-press, not a tap that could double
      }
      this.longPressDeadline = null;
    }
    return this.deadline();
  }

  // ---- helpers ---------------------------------------------------------

  /** Release a held left button if we're mid-hold-drag (idempotent). */
  private releaseHeld(): void {
    if (this.state === State.HoldDrag) this.sink.up(0);
  }

  private deadline(): number | null {
    return this.state === State.PendingTap ? this.longPressDeadline : null;
  }

  private emitTap(p: Pointer, t: number): void {
    const pt = this.movePoint(p);
    const isDouble =
      this.lastTap !== null &&
      t - this.lastTap.t <= this.t.doubleTapMs &&
      dist(p.x, p.y, this.lastTap.x, this.lastTap.y) <= this.t.doubleTapSlopPx;
    this.sink.moveAbs(pt.x, pt.y);
    if (isDouble) {
      this.sink.doubleClick();
      this.lastTap = null;
    } else {
      this.sink.click(0);
      this.lastTap = { x: p.x, y: p.y, t };
    }
  }

  /** The point a discrete click/move targets: finger (touch) or cursor (trackpad). */
  private movePoint(p: Pointer): { x: number; y: number } {
    if (this.mode === "touch") return { x: p.x, y: p.y };
    return { x: this.vx, y: this.vy };
  }

  /** Emit a move to the finger (touch) or the virtual cursor (trackpad). */
  private emitMove(p: Pointer): void {
    if (this.mode === "touch") {
      this.sink.moveAbs(p.x, p.y);
    } else {
      this.sink.moveAbs(this.vx, this.vy);
    }
  }

  /** Trackpad: advance the virtual cursor by the per-move delta, clamped. */
  private advanceCursor(prevX: number, prevY: number, x: number, y: number): void {
    if (this.mode !== "trackpad") return;
    this.vx = clamp(
      this.vx + this.t.trackpadAccel * (x - prevX),
      this.bounds.left,
      this.bounds.left + this.bounds.width,
    );
    this.vy = clamp(
      this.vy + this.t.trackpadAccel * (y - prevY),
      this.bounds.top,
      this.bounds.top + this.bounds.height,
    );
  }

  private endSingle(id: number): void {
    this.removePointer(id);
    if (this.pointers.size === 0) this.toIdle();
    else this.state = State.TwoDraining;
  }

  private toIdle(): void {
    this.state = State.Idle;
    this.longPressDeadline = null;
  }

  private removePointer(id: number): void {
    this.pointers.delete(id);
    this.order = this.order.filter((p) => p !== id);
  }

  private firstPointer(): Pointer | undefined {
    const id = this.order[0];
    return id === undefined ? undefined : this.pointers.get(id);
  }

  private canBecomeTwoFinger(): boolean {
    return (
      this.state === State.PendingTap ||
      this.state === State.LongPressArmed ||
      this.state === State.Drag ||
      // a finger re-landing during the drain phase resumes a two-finger
      // episode rather than starting a fresh single-finger gesture
      this.state === State.TwoDraining
    );
  }

  private beginTwoFinger(): void {
    this.longPressDeadline = null;
    this.lastTap = null; // a two-finger gesture isn't a tap; don't fuse a later one
    this.state = State.TwoPending;
    const [a, b] = this.twoPointers();
    this.d0 = dist(a.x, a.y, b.x, b.y);
    this.cx0 = (a.x + b.x) / 2;
    this.cy0 = (a.y + b.y) / 2;
  }

  private twoPointers(): [Pointer, Pointer] {
    const ps = this.order.map((id) => this.pointers.get(id)!).filter(Boolean);
    return [ps[0]!, ps[1]!];
  }

  private centroidOfStarts(): { x: number; y: number } {
    const [a, b] = this.twoPointers();
    return { x: (a.startX + b.startX) / 2, y: (a.startY + b.startY) / 2 };
  }

  private twoFingerWasTap(t: number): boolean {
    const [a, b] = this.twoPointers();
    const quick =
      t - a.startT <= this.t.twoTapMaxMs && t - b.startT <= this.t.twoTapMaxMs;
    const still =
      a.maxDist <= this.t.twoMoveSlopPx && b.maxDist <= this.t.twoMoveSlopPx;
    return quick && still;
  }

  private classifyTwoFinger(): void {
    const [a, b] = this.twoPointers();
    const d = dist(a.x, a.y, b.x, b.y);
    const cx = (a.x + b.x) / 2;
    const cy = (a.y + b.y) / 2;
    const ddist = Math.abs(d - this.d0);
    const dcent = dist(cx, cy, this.cx0, this.cy0);

    if (ddist >= this.t.pinchDistPx && ddist >= dcent) {
      this.state = State.TwoPinch;
      this.emitZoom();
    } else if (dcent >= this.t.twoMoveSlopPx && dcent > ddist) {
      this.state = State.TwoScroll;
      this.cx0 = cx; // reset reference so the first scroll delta is from here
      this.cy0 = cy;
    }
  }

  private emitScroll(): void {
    const [a, b] = this.twoPointers();
    const cx = (a.x + b.x) / 2;
    const cy = (a.y + b.y) / 2;
    const dx = Math.round(this.t.scrollGain * (cx - this.cx0));
    const dy = Math.round(this.t.scrollGain * (cy - this.cy0));
    this.cx0 = cx;
    this.cy0 = cy;
    if (dx !== 0 || dy !== 0) this.sink.scroll(dx, dy);
  }

  private emitZoom(): void {
    const [a, b] = this.twoPointers();
    const d = dist(a.x, a.y, b.x, b.y);
    const scale = this.d0 === 0 ? 1 : d / this.d0;
    this.sink.zoom(scale, (a.x + b.x) / 2, (a.y + b.y) / 2);
  }
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}
