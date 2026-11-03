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
//
// Two-finger scroll is content-follows-fingers (iOS-native) and coasts after
// a flick: the release velocity decays exponentially, driven through the same
// deadline/tick() channel as long-press. Any new finger stops the coast.

export type Mode = "touch" | "trackpad";

export interface GestureSink {
  moveAbs(x: number, y: number): void; // client px; caller normalizes
  down(button: number): void; // 0 left, 2 right
  up(button: number): void;
  click(button: number): void;
  doubleClick(): void;
  /** Wire sign = DOM wheel delta: +dy scrolls the document down (content
   * moves up), exactly what the desktop wheel path forwards. */
  scroll(dx: number, dy: number): void;
  /** LOCAL view zoom — never forwarded to the host. `scale` is cumulative
   * from the start of the current pinch; `focal` is the live centroid. */
  zoom(scale: number, focalX: number, focalY: number): void;
  /** The current pinch ended — commit the live zoom transform. */
  zoomEnd(): void;
  /** LOCAL view pan in client px (trackpad edge auto-pan while zoomed);
   * returns the shift actually applied after the view's own clamping.
   * Optional: without it the cursor just stops at the visible edge. */
  pan?(dx: number, dy: number): { dx: number; dy: number };
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
  momentumTauMs: number; // decay time constant of a released scroll
  momentumMinVelocity: number; // px/ms; coast stops below (never starts under) this
  momentumMaxMs: number; // hard cap on one coast
  momentumFrameMs: number; // emit cadence while coasting
  momentumSampleMs: number; // release velocity = travel over this trailing window
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
  momentumTauMs: 325,
  momentumMinVelocity: 0.05,
  momentumMaxMs: 2000,
  momentumFrameMs: 16,
  momentumSampleMs: 100,
};

export interface GestureConfig {
  mode: Mode;
  /** The displayed content rect (client px, zoom transform included). */
  bounds: Rect;
  /** The on-screen part of the stage; the trackpad cursor stays inside
   * bounds ∩ visible, and edge auto-pan reveals the rest when zoomed. */
  visible?: Rect;
  seedCursor?: { x: number; y: number };
  thresholds?: Partial<Thresholds>;
}

/**
 * Trackpad edge auto-pan: how far to shift a zoomed view so the cursor's
 * desired point (x, y) stays visible — limited, per axis, to the content
 * still hidden past the edge it's pushing against. Returns the content
 * translation in client px (negative = content moves left/up); zero when
 * the point is inside `visible` or nothing is hidden that way (unzoomed).
 */
export function edgePan(x: number, y: number, content: Rect, visible: Rect): { dx: number; dy: number } {
  return {
    dx: panAxis(x, content.left, content.width, visible.left, visible.width),
    dy: panAxis(y, content.top, content.height, visible.top, visible.height),
  };
}

function panAxis(p: number, cStart: number, cLen: number, vStart: number, vLen: number): number {
  const vEnd = vStart + vLen;
  if (p > vEnd) return -Math.min(p - vEnd, Math.max(0, cStart + cLen - vEnd)) || 0;
  if (p < vStart) return Math.min(vStart - p, Math.max(0, vStart - cStart));
  return 0;
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

/** A released two-finger scroll coasting to a stop. */
interface Momentum {
  vx: number; // wire scroll units per ms at t0
  vy: number;
  t0: number;
  end: number; // speed falls under the cutoff (or the cap) here
  next: number; // next emit deadline
  sentX: number; // whole units emitted so far
  sentY: number;
}

const dist = (ax: number, ay: number, bx: number, by: number) =>
  Math.hypot(ax - bx, ay - by);

/** Round to an integer, never -0 (keeps emitted deltas tidy). */
const round = (v: number) => Math.round(v) || 0;

/** Big enough to aim into (a hidden view or pre-first-frame rect isn't). */
const usable = (r: Rect) => r.width > 1 && r.height > 1;

export class GestureMachine {
  private state = State.Idle;
  private readonly pointers = new Map<number, Pointer>();
  private order: number[] = []; // pointer ids in arrival order
  private mode: Mode;
  private readonly t: Thresholds;
  private bounds: Rect;
  private visible: Rect | null;

  private vx: number;
  private vy: number;
  private cursorSeeded: boolean;

  private longPressDeadline: number | null = null;
  private lastTap: { x: number; y: number; t: number } | null = null;

  // two-finger episode start references
  private d0 = 0;
  private cx0 = 0;
  private cy0 = 0;

  // two-finger scroll: cumulative unrounded travel (wire units), the whole
  // units already sent, and recent samples of the travel for the release
  // velocity
  private scrollX = 0;
  private scrollY = 0;
  private scrollSentX = 0;
  private scrollSentY = 0;
  private scrollSamples: { t: number; x: number; y: number }[] = [];
  private momentum: Momentum | null = null;

  constructor(
    private readonly sink: GestureSink,
    config: GestureConfig,
  ) {
    this.mode = config.mode;
    this.bounds = config.bounds;
    this.visible = config.visible && usable(config.visible) ? config.visible : null;
    this.t = { ...DEFAULT_THRESHOLDS, ...config.thresholds };
    const seed = config.seedCursor ?? {
      x: config.bounds.left + config.bounds.width / 2,
      y: config.bounds.top + config.bounds.height / 2,
    };
    this.vx = seed.x;
    this.vy = seed.y;
    // If seeded from an explicit point or a real (non-degenerate) rect, the
    // virtual cursor is positioned; otherwise re-seed on the first valid bounds
    // (the displayed rect is degenerate before the first frame arrives).
    this.cursorSeeded = config.seedCursor != null || config.bounds.width > 1;
  }

  /** Returns true if the swap was applied. Deferred (false) mid-gesture: a
   * HoldDrag anchored in finger space would teleport to the stale cursor. */
  setMode(mode: Mode): boolean {
    if (this.state !== State.Idle) return false;
    this.mode = mode;
    return true;
  }

  /**
   * New displayed content rect (and visible stage): rotation, resize, zoom,
   * pan. The trackpad cursor keeps its CONTENT point across the change (it
   * sits on the remote desktop, not on the glass), then is clamped back into
   * bounds ∩ visible. Unusable rects (no frame yet, view hidden) are ignored.
   */
  setBounds(bounds: Rect, visible?: Rect): void {
    if (visible && usable(visible)) this.visible = visible;
    if (!usable(bounds)) return;
    const prev = this.bounds;
    this.bounds = bounds;
    if (!this.cursorSeeded) {
      // first valid displayed rect: center the trackpad cursor here
      this.vx = bounds.left + bounds.width / 2;
      this.vy = bounds.top + bounds.height / 2;
      this.cursorSeeded = true;
    } else if (usable(prev)) {
      this.vx = bounds.left + ((this.vx - prev.left) / prev.width) * bounds.width;
      this.vy = bounds.top + ((this.vy - prev.top) / prev.height) * bounds.height;
    }
    this.clampCursor();
  }

  /** The trackpad virtual cursor, client px (where the next tap clicks). */
  get cursor(): { x: number; y: number } {
    return { x: this.vx, y: this.vy };
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
    this.momentum = null;
  }

  onDown(id: number, x: number, y: number, t: number): number | null {
    this.momentum = null; // a new touch catches a coasting scroll
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
        this.classifyTwoFinger(t);
        break;
      case State.TwoScroll:
        this.emitScroll(t);
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
          // touch: the first finger's point — the centroid of two spread
          // fingers can be empty space; trackpad: the virtual cursor
          const [first] = this.twoPointers();
          const pt = this.mode === "touch"
            ? { x: first.startX, y: first.startY }
            : { x: this.vx, y: this.vy };
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
        // the first finger up ends the scroll; a flick coasts on from here
        if (this.state === State.TwoScroll) this.startMomentum(t);
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
    this.momentum = null; // the system took the touch over: don't scroll on
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
    if (this.momentum && t >= this.momentum.next) this.stepMomentum(t);
    return this.deadline();
  }

  // ---- helpers ---------------------------------------------------------

  /** Release a held left button if we're mid-hold-drag (idempotent). */
  private releaseHeld(): void {
    if (this.state === State.HoldDrag) this.sink.up(0);
  }

  private deadline(): number | null {
    // exclusive: any onDown (the only way into PendingTap) stops momentum
    if (this.momentum) return this.momentum.next;
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

  /**
   * Trackpad: advance the virtual cursor by the per-move delta. Pushing past
   * the visible edge of a zoomed view pans it (the cursor stays at the edge
   * and travels on over newly revealed content); then clamp.
   */
  private advanceCursor(prevX: number, prevY: number, x: number, y: number): void {
    if (this.mode !== "trackpad") return;
    this.vx += this.t.trackpadAccel * (x - prevX);
    this.vy += this.t.trackpadAccel * (y - prevY);
    if (this.visible && this.sink.pan) {
      const want = edgePan(this.vx, this.vy, this.bounds, this.visible);
      if (want.dx !== 0 || want.dy !== 0) {
        const got = this.sink.pan(want.dx, want.dy);
        // track the moved content rect ourselves (the next setBounds confirms)
        this.bounds = { ...this.bounds, left: this.bounds.left + got.dx, top: this.bounds.top + got.dy };
        this.vx += got.dx;
        this.vy += got.dy;
      }
    }
    this.clampCursor();
  }

  /** Keep the virtual cursor on content the user can actually see. */
  private clampCursor(): void {
    const r = (this.visible && intersect(this.bounds, this.visible)) || this.bounds;
    this.vx = clamp(this.vx, r.left, r.left + r.width);
    this.vy = clamp(this.vy, r.top, r.top + r.height);
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

  private twoFingerWasTap(t: number): boolean {
    const [a, b] = this.twoPointers();
    const quick =
      t - a.startT <= this.t.twoTapMaxMs && t - b.startT <= this.t.twoTapMaxMs;
    const still =
      a.maxDist <= this.t.twoMoveSlopPx && b.maxDist <= this.t.twoMoveSlopPx;
    return quick && still;
  }

  private classifyTwoFinger(t: number): void {
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
      this.scrollX = 0;
      this.scrollY = 0;
      this.scrollSentX = 0;
      this.scrollSentY = 0;
      this.scrollSamples = [{ t, x: 0, y: 0 }];
    }
  }

  private emitScroll(t: number): void {
    const [a, b] = this.twoPointers();
    const cx = (a.x + b.x) / 2;
    const cy = (a.y + b.y) / 2;
    // Content follows the fingers, as in native iOS scrolling: fingers
    // moving UP scroll the document DOWN, which on the wire (DOM wheel sign)
    // is +dy. So the wire delta is the NEGATED centroid travel — the same
    // sign a natural-scrolling Mac trackpad's wheel event carries.
    const rawX = this.t.scrollGain * (this.cx0 - cx);
    const rawY = this.t.scrollGain * (this.cy0 - cy);
    this.cx0 = cx;
    this.cy0 = cy;
    this.scrollX += rawX;
    this.scrollY += rawY;
    this.scrollSamples.push({ t, x: this.scrollX, y: this.scrollY });
    const horizon = t - this.t.momentumSampleMs;
    while (this.scrollSamples.length > 0 && this.scrollSamples[0]!.t < horizon) {
      this.scrollSamples.shift();
    }
    // Emit the whole units of the cumulative travel not yet sent. Rounding
    // each event's delta instead loses the remainder: pointer events come one
    // finger at a time, so a slow drag moves the centroid by ±0.5 px per
    // event, which rounded to +1 going up (double speed) and to 0 going down
    // (no scroll at all) — seen on iOS.
    const dx = round(this.scrollX) - this.scrollSentX;
    const dy = round(this.scrollY) - this.scrollSentY;
    this.scrollSentX += dx;
    this.scrollSentY += dy;
    if (dx !== 0 || dy !== 0) this.sink.scroll(dx, dy);
  }

  /**
   * Release velocity = scroll travel over the trailing sample window, up to
   * the lift time `t` (a pause before lifting therefore damps it to zero).
   * Fast enough → coast for as long as the decay stays above the cutoff.
   */
  private startMomentum(t: number): void {
    const samples = this.scrollSamples;
    this.scrollSamples = [];
    const first = samples.find((s) => s.t >= t - this.t.momentumSampleMs);
    if (!first || t <= first.t) return;
    const vx = (this.scrollX - first.x) / (t - first.t);
    const vy = (this.scrollY - first.y) / (t - first.t);
    const speed = Math.hypot(vx, vy);
    const { momentumTauMs: tau, momentumMinVelocity: min } = this.t;
    if (tau <= 0 || !(speed > min)) return;
    const run = Math.min(this.t.momentumMaxMs, tau * Math.log(speed / min));
    this.momentum = {
      vx,
      vy,
      t0: t,
      end: t + run,
      next: Math.min(t + run, t + this.t.momentumFrameMs),
      sentX: 0,
      sentY: 0,
    };
  }

  /** Emit the coast's progress up to `t`, in whole units. */
  private stepMomentum(t: number): void {
    const m = this.momentum!;
    const tau = this.t.momentumTauMs;
    // Closed-form distance under v = v0·e^(-(t-t0)/τ): exact whatever the
    // timer jitter, so a late tick catches up rather than slowing the coast.
    const k = tau * (1 - Math.exp(-(Math.min(t, m.end) - m.t0) / tau));
    const totX = round(m.vx * k);
    const totY = round(m.vy * k);
    const dx = totX - m.sentX;
    const dy = totY - m.sentY;
    m.sentX = totX;
    m.sentY = totY;
    if (dx !== 0 || dy !== 0) this.sink.scroll(dx, dy);
    if (t >= m.end) this.momentum = null;
    else m.next = Math.min(m.end, t + this.t.momentumFrameMs);
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

/** Overlap of two rects, or null if they don't overlap. */
function intersect(a: Rect, b: Rect): Rect | null {
  const left = Math.max(a.left, b.left);
  const top = Math.max(a.top, b.top);
  const right = Math.min(a.left + a.width, b.left + b.width);
  const bottom = Math.min(a.top + a.height, b.top + b.height);
  return right > left && bottom > top ? { left, top, width: right - left, height: bottom - top } : null;
}
