import { describe, expect, it } from "vitest";
import {
  DEFAULT_THRESHOLDS,
  GestureMachine,
  edgePan,
  type GestureConfig,
  type GestureSink,
  type Mode,
} from "./gestures";

type Event = string;

function harness(
  mode: Mode,
  overrides: Partial<GestureConfig> = {},
  /** the view's pan: applies the request in full by default; null = none */
  pan: ((dx: number, dy: number) => { dx: number; dy: number }) | null = (dx, dy) => ({ dx, dy }),
) {
  const events: Event[] = [];
  const sink: GestureSink = {
    moveAbs: (x, y) => events.push(`move(${Math.round(x)},${Math.round(y)})`),
    down: (b) => events.push(`down(${b})`),
    up: (b) => events.push(`up(${b})`),
    click: (b) => events.push(`click(${b})`),
    doubleClick: () => events.push("dbl"),
    scroll: (dx, dy) => events.push(`scroll(${dx},${dy})`),
    zoom: (s, fx, fy) => events.push(`zoom(${s.toFixed(2)},${Math.round(fx)},${Math.round(fy)})`),
    zoomEnd: () => events.push("zoomEnd"),
  };
  if (pan) {
    sink.pan = (dx, dy) => {
      events.push(`pan(${Math.round(dx)},${Math.round(dy)})`);
      return pan(dx, dy);
    };
  }
  const m = new GestureMachine(sink, {
    mode,
    bounds: { left: 0, top: 0, width: 1000, height: 1000 },
    seedCursor: { x: 400, y: 300 },
    ...overrides,
  });
  return { m, events };
}

const LP = DEFAULT_THRESHOLDS.longPressMs;

describe("touch mode", () => {
  it("single tap → left click at the finger point (eager, no wait)", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onUp(1, 102, 101, 80);
    expect(events).toEqual(["move(102,101)", "click(0)"]);
  });

  it("double tap → click then doubleClick at the second point", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onUp(1, 100, 100, 70);
    m.onDown(1, 108, 104, 200);
    m.onUp(1, 109, 103, 260);
    expect(events).toEqual(["move(100,100)", "click(0)", "move(109,103)", "dbl"]);
  });

  it("two taps too far apart → two singles, never a double", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onUp(1, 100, 100, 60);
    m.onDown(1, 400, 400, 200);
    m.onUp(1, 400, 400, 260);
    expect(events).toEqual(["move(100,100)", "click(0)", "move(400,400)", "click(0)"]);
  });

  it("long press (held still) → right click on lift, no left click", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.tick(LP); // long-press timer fires (armed, deferred)
    expect(events).toEqual([]); // nothing emitted yet
    m.onUp(1, 200, 200, LP + 40);
    expect(events).toEqual(["move(200,200)", "click(2)"]);
  });

  it("drag cancels the tap and disarms long-press (cursor move, no button)", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 140, 100, 40); // Δ=40 > slop
    m.onMove(1, 180, 100, 80);
    m.onUp(1, 180, 100, 120);
    expect(events).toEqual(["move(140,100)", "move(180,100)"]);
    expect(m.tick(LP + 100)).toBeNull(); // timer was disarmed
  });

  it("long-press-then-drag → left hold + drag + release, NO right click", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.tick(LP);
    m.onMove(1, 240, 200, LP + 40); // moves after long-press armed
    m.onMove(1, 300, 200, LP + 80);
    m.onUp(1, 300, 200, LP + 120);
    expect(events).toEqual([
      "move(200,200)", "down(0)", "move(240,200)", "move(300,200)", "up(0)",
    ]);
    expect(events).not.toContain("click(2)");
  });

  it("slop boundary is strict >", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 0, 0, 0);
    m.onMove(1, 10, 0, 30); // Δ=10, not > 10 → still pending
    expect(events).toEqual([]);
    m.onMove(1, 11, 0, 40); // Δ=11 > 10 → drag
    expect(events).toEqual(["move(11,0)"]);
  });
});

describe("trackpad mode", () => {
  it("tap clicks at the virtual cursor, not the finger", () => {
    const { m, events } = harness("trackpad"); // seed (400,300)
    m.onDown(1, 100, 100, 0);
    m.onUp(1, 103, 100, 90);
    expect(events).toEqual(["move(400,300)", "click(0)"]);
  });

  it("drag moves the virtual cursor by the relative delta", () => {
    const { m, events } = harness("trackpad");
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 120, 100, 30); // Δx=+20 → cursor 420,300
    m.onMove(1, 120, 140, 60); // Δy=+40 → cursor 420,340
    m.onUp(1, 120, 140, 90);
    expect(events).toEqual(["move(420,300)", "move(420,340)"]);
  });

  it("clamps the virtual cursor to bounds", () => {
    const { m, events } = harness("trackpad", { seedCursor: { x: 5, y: 5 } });
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 50, 100, 30); // Δx=-50 → clamp to 0
    expect(events).toEqual(["move(0,5)"]);
  });
});

const scrollDeltas = (events: Event[]) =>
  events
    .filter((e) => e.startsWith("scroll"))
    .map((s) => s.match(/scroll\((-?\d+),(-?\d+)\)/)!.slice(1).map(Number) as [number, number]);

describe("two-finger gestures", () => {
  // Wire sign = DOM wheel deltaY: +dy scrolls the document down (content
  // moves up). Content must follow the fingers, like native iOS scrolling.
  it("two-finger drag down → content follows fingers: document scrolls up (dy<0)", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 140, 100, 5); // d0=40, c0=(120,100)
    // both fingers slide down; pointer events arrive one finger at a time, so
    // the centroid advances in increments → several negative-dy scrolls.
    m.onMove(1, 100, 140, 30);
    m.onMove(2, 140, 140, 30);
    m.onMove(1, 100, 180, 60);
    m.onMove(2, 140, 180, 60);
    m.onUp(1, 100, 180, 90);
    m.onUp(2, 140, 180, 95);
    const scrolls = scrollDeltas(events);
    expect(scrolls.length).toBeGreaterThanOrEqual(2);
    for (const [dx, dy] of scrolls) {
      expect(dx).toBe(0);
      expect(dy).toBeLessThan(0);
    }
    expect(events).not.toContain("click(0)");
    expect(events.some((e) => e.startsWith("zoom"))).toBe(false);
  });

  it("two-finger drag up → dy>0, the sign a natural-scrolling trackpad's wheel sends", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 300, 0);
    m.onDown(2, 140, 300, 5);
    m.onMove(1, 100, 260, 30);
    m.onMove(2, 140, 260, 30);
    m.onMove(1, 100, 220, 60);
    m.onMove(2, 140, 220, 60);
    const scrolls = scrollDeltas(events);
    expect(scrolls.length).toBeGreaterThanOrEqual(2);
    for (const [dx, dy] of scrolls) {
      expect(dx).toBe(0);
      expect(dy).toBeGreaterThan(0);
    }
    // total = finger travel past the slop point (classified at centroid 280)
    expect(scrolls.reduce((sum, [, dy]) => sum + dy, 0)).toBe(60);
  });

  it("slow drags scroll by their true travel, both ways (no per-event rounding loss)", () => {
    // 1 px per finger per frame; pointer events come one finger at a time, so
    // each moves the centroid 0.5 px. Rounding per event sent +1 each going
    // up (double) and 0 going down (nothing) — the iOS Simulator showed it.
    const drag = (dir: 1 | -1) => {
      const { m, events } = harness("touch");
      m.onDown(1, 100, 500, 0);
      m.onDown(2, 140, 500, 0);
      for (let i = 1; i <= 60; i++) {
        m.onMove(1, 100, 500 + dir * i, 16 * i);
        m.onMove(2, 140, 500 + dir * i, 16 * i);
      }
      return scrollDeltas(events).reduce((sum, [, dy]) => sum + dy, 0);
    };
    // classified once the centroid passes the 10 px slop; the rest scrolls
    expect(drag(1)).toBe(-50); // fingers down → content follows → dy<0
    expect(drag(-1)).toBe(50);
  });

  it("horizontal two-finger drag: fingers right → dx<0", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 100, 140, 5);
    m.onMove(1, 140, 100, 30);
    m.onMove(2, 140, 140, 30);
    m.onMove(1, 180, 100, 60);
    const scrolls = scrollDeltas(events);
    expect(scrolls.length).toBeGreaterThanOrEqual(1);
    for (const [dx, dy] of scrolls) {
      expect(dx).toBeLessThan(0);
      expect(dy).toBe(0);
    }
  });

  it("two-finger tap (touch) → right click at the FIRST finger, not the centroid", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 160, 100, 8);
    m.onUp(1, 101, 100, 120);
    m.onUp(2, 159, 100, 130);
    expect(events).toEqual(["move(100,100)", "click(2)"]);
  });

  it("two-finger tap (trackpad) → right click at the virtual cursor", () => {
    const { m, events } = harness("trackpad"); // seed (400,300)
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 160, 100, 8);
    m.onUp(2, 159, 100, 120); // either finger may lift first
    m.onUp(1, 101, 100, 130);
    expect(events).toEqual(["move(400,300)", "click(2)"]);
  });

  it("pinch (spread) → cumulative zoom, never scroll or click", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.onDown(2, 260, 200, 6); // d0=60
    m.onMove(2, 320, 200, 40); // d=120 → scale 2.0, ddist=60 dominates
    m.onMove(2, 380, 200, 70); // d=180 → scale 3.0
    m.onUp(2, 380, 200, 100);
    m.onUp(1, 200, 200, 110);
    expect(events).toEqual(["zoom(2.00,260,200)", "zoom(3.00,290,200)", "zoomEnd"]);
  });

  it("second finger mid-drag → scroll, no spurious click or button", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 150, 100, 40); // DRAG: move(150,100)
    m.onDown(2, 210, 100, 60); // → two-finger
    m.onMove(1, 150, 140, 90);
    m.onMove(2, 210, 140, 90);
    m.onUp(1, 150, 140, 120);
    m.onUp(2, 210, 140, 125);
    expect(events.some((e) => e.startsWith("scroll"))).toBe(true);
    expect(events).not.toContain("down(0)");
    expect(events).not.toContain("click(0)");
    expect(events).not.toContain("click(2)");
  });
});

describe("safety", () => {
  it("cancel mid-hold-drag releases the button exactly once", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.tick(LP);
    m.onMove(1, 240, 200, LP + 40); // HoldDrag: down(0)
    m.onCancel(1);
    expect(events.filter((e) => e === "up(0)")).toEqual(["up(0)"]);
    expect(events).not.toContain("dbl");
  });

  it("reset clears pending state", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 10, 10, 0);
    m.reset();
    expect(m.tick(LP + 100)).toBeNull();
    m.onUp(1, 10, 10, LP + 200); // unknown pointer after reset
    expect(events).toEqual([]);
  });

  // ---- regressions from the Phase 4 adversarial review --------------------

  it("a 2nd finger during a hold-drag releases the button, no leak, no stray click", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.tick(LP); // long-press armed
    m.onMove(1, 240, 200, LP + 40); // → HoldDrag: move, down(0), move
    m.onDown(2, 400, 200, LP + 60); // 2nd finger lands mid-hold-drag
    m.onUp(2, 400, 200, LP + 80);
    m.onUp(1, 240, 200, LP + 100);
    const downs = events.filter((e) => e === "down(0)").length;
    const ups = events.filter((e) => e === "up(0)").length;
    expect(downs).toBe(1);
    expect(ups).toBe(1); // released, not leaked
    expect(events).not.toContain("click(0)"); // no spurious tap
  });

  it("reset mid-hold-drag releases the held button", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.tick(LP);
    m.onMove(1, 240, 200, LP + 40); // HoldDrag, down(0)
    m.reset();
    expect(events.filter((e) => e === "up(0)")).toEqual(["up(0)"]);
  });

  it("double-tap does NOT fuse across an intervening drag", () => {
    const { m, events } = harness("touch");
    // tap 1
    m.onDown(1, 100, 100, 0);
    m.onUp(1, 100, 100, 40);
    // a quick drag (all still within doubleTapMs of tap 1)
    m.onDown(1, 100, 100, 80);
    m.onMove(1, 160, 100, 120); // → Drag, clears lastTap
    m.onUp(1, 160, 100, 140);
    // tap 2 near tap 1, still within the double-tap window
    m.onDown(1, 102, 100, 180);
    m.onUp(1, 102, 100, 210);
    expect(events).not.toContain("dbl"); // the drag broke the pairing
    expect(events.filter((e) => e === "click(0)").length).toBe(2);
  });

  it("trackpad cursor re-seeds to center on the first valid bounds (degenerate at attach)", () => {
    // constructed with a degenerate rect (no frame yet), no explicit seed
    const { m, events } = harness("trackpad", {
      bounds: { left: 0, top: 0, width: 0, height: 0 },
      seedCursor: undefined,
    });
    // a real displayed rect arrives (frames started)
    m.setBounds({ left: 100, top: 50, width: 800, height: 600 }); // center (500,350)
    m.onDown(1, 200, 200, 0);
    m.onMove(1, 220, 200, 30); // +20x → cursor 520,350
    m.onUp(1, 220, 200, 60);
    expect(events).toContain("move(520,350)");
  });

  it("onUp for an unknown pointer id is ignored (doesn't corrupt a live gesture)", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0); // a real finger is pending
    m.onUp(99, 0, 0, 50); // stray up for a never-seen pointer
    // the real finger's tap must still work
    m.onUp(1, 100, 100, 60);
    expect(events).toEqual(["move(100,100)", "click(0)"]);
  });
});

describe("relayout: trackpad cursor vs. rect changes", () => {
  it("keeps the cursor on the same content point when the rect changes (rotation)", () => {
    const { m, events } = harness("trackpad"); // 1000×1000 at 0,0; cursor (400,300)
    m.setBounds({ left: 0, top: 100, width: 500, height: 500 });
    expect(m.cursor).toEqual({ x: 200, y: 250 });
    m.onDown(1, 50, 50, 0);
    m.onUp(1, 50, 50, 60);
    expect(events).toEqual(["move(200,250)", "click(0)"]);
  });

  it("clamps the cursor into content ∩ visible stage", () => {
    const { m } = harness("trackpad");
    // zoomed 2×: the content rect now overhangs a 500×500 visible stage
    m.setBounds({ left: 0, top: 0, width: 2000, height: 2000 }, { left: 0, top: 0, width: 500, height: 500 });
    expect(m.cursor).toEqual({ x: 500, y: 500 }); // remapped to (800,600), then clamped
  });

  it("ignores an unusable rect (hidden view / no frame yet)", () => {
    const { m } = harness("trackpad");
    m.setBounds({ left: 0, top: 0, width: 0, height: 0 });
    expect(m.cursor).toEqual({ x: 400, y: 300 });
    m.setBounds({ left: 0, top: 0, width: 500, height: 500 }); // remaps from the last usable rect
    expect(m.cursor).toEqual({ x: 200, y: 150 });
  });

  it("an unusable visible rect (stage hidden at attach) neither clamps nor pans", () => {
    const { m, events } = harness("trackpad", { visible: { left: 0, top: 0, width: 0, height: 0 } });
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 300, 100, 30); // +200 → 600, inside the content rect
    expect(events).toEqual(["move(600,300)"]);
  });

  it("without a view pan, a drag stops at the visible edge, not the content edge", () => {
    const { m, events } = harness(
      "trackpad",
      {
        bounds: { left: 0, top: 0, width: 2000, height: 1000 },
        visible: { left: 0, top: 0, width: 1000, height: 500 },
        seedCursor: { x: 900, y: 250 },
      },
      null,
    );
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 400, 100, 30); // +300 → would be 1200
    expect(events).toEqual(["move(1000,250)"]);
  });
});

describe("edge auto-pan", () => {
  const visible = { left: 0, top: 0, width: 1000, height: 500 };

  it("edgePan: zero inside the visible area or when nothing is hidden", () => {
    const zoomed = { left: -500, top: -250, width: 2000, height: 1000 };
    expect(edgePan(500, 250, zoomed, visible)).toEqual({ dx: 0, dy: 0 });
    const fitted = { left: 100, top: 0, width: 800, height: 500 }; // unzoomed, pillarboxed
    expect(edgePan(1030, 520, fitted, visible)).toEqual({ dx: 0, dy: 0 });
  });

  it("edgePan: shifts by the overflow, limited to the content hidden past that edge", () => {
    const zoomed = { left: -500, top: -250, width: 2000, height: 1000 }; // 500 hidden right, 250 below
    expect(edgePan(1030, 250, zoomed, visible)).toEqual({ dx: -30, dy: 0 });
    expect(edgePan(1900, 600, zoomed, visible)).toEqual({ dx: -500, dy: -100 });
    expect(edgePan(-20, -5, zoomed, visible)).toEqual({ dx: 20, dy: 5 });
    expect(edgePan(-900, 250, zoomed, visible)).toEqual({ dx: 500, dy: 0 }); // only 500 hidden left
  });

  it("pushing the cursor past the edge pans the view; the cursor rides the edge", () => {
    const { m, events } = harness("trackpad", {
      bounds: { left: -500, top: -250, width: 2000, height: 1000 }, // zoomed, 500 hidden right
      visible,
      seedCursor: { x: 900, y: 250 },
    });
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 250, 100, 30); // +150 → 1050: pan 50
    m.onMove(1, 850, 100, 60); // +600 → 1600: only 450 left to reveal
    m.onMove(1, 950, 100, 90); // +100 → content edge reached: no pan
    expect(events).toEqual([
      "pan(-50,0)", "move(1000,250)",
      "pan(-450,0)", "move(1000,250)",
      "move(1000,250)",
    ]);
  });

  it("trusts the pan the view actually applied", () => {
    const { m, events } = harness(
      "trackpad",
      { bounds: { left: -500, top: -250, width: 2000, height: 1000 }, visible, seedCursor: { x: 900, y: 250 } },
      () => ({ dx: -20, dy: 0 }), // the view clamps harder than asked
    );
    m.onDown(1, 100, 100, 0);
    m.onMove(1, 250, 100, 30);
    expect(events).toEqual(["pan(-50,0)", "move(1000,250)"]);
    // the machine's content rect moved by 20, not 50: a rect report at the
    // same place is a no-op for the cursor
    m.setBounds({ left: -520, top: -250, width: 2000, height: 1000 }, visible);
    expect(m.cursor).toEqual({ x: 1000, y: 250 });
  });
});

describe("momentum scrolling", () => {
  const T = DEFAULT_THRESHOLDS;

  /** Two fingers flick up 20px per 16ms frame; first lift at t=100. The
   * scroll classifies at t=16 and then travels 110 units in 84 ms. */
  function flickUp(m: GestureMachine, liftAt = 100): number | null {
    m.onDown(1, 100, 300, 0);
    m.onDown(2, 140, 300, 0);
    for (let i = 1; i <= 6; i++) {
      m.onMove(1, 100, 300 - 20 * i, 16 * i);
      m.onMove(2, 140, 300 - 20 * i, 16 * i);
    }
    m.onUp(1, 100, 180, liftAt);
    return m.onUp(2, 140, 180, liftAt + 4);
  }
  const V0 = 110 / 84; // release speed, units/ms

  /** Drive the deadline loop; `late(i)` delays the i-th tick. */
  function coast(m: GestureMachine, deadline: number | null, late = (_i: number) => 0): number[] {
    const times: number[] = [];
    for (let i = 0; deadline !== null && i < 1000; i++) {
      const t = deadline + late(i);
      times.push(t);
      deadline = m.tick(t);
    }
    return times;
  }

  it("a flick coasts on with decaying, same-direction scrolls, then stops", () => {
    const { m, events } = harness("touch");
    const deadline = flickUp(m);
    expect(deadline).toBe(100 + T.momentumFrameMs);
    const before = scrollDeltas(events).length;
    const times = coast(m, deadline);
    const coastDeltas = scrollDeltas(events).slice(before);
    expect(coastDeltas.length).toBeGreaterThan(10);
    for (const [dx, dy] of coastDeltas) {
      expect(dx).toBe(0);
      expect(dy).toBeGreaterThan(0); // same sign as the flick
    }
    expect(coastDeltas[0]![1]).toBeGreaterThan(coastDeltas.at(-1)![1]);
    // runs until the speed decays to the cutoff: τ·ln(v0/min) after the lift
    expect(times.at(-1)).toBeCloseTo(100 + T.momentumTauMs * Math.log(V0 / T.momentumMinVelocity), 6);
    // distance = ∫ v = τ·(v0 − min), emitted in whole units
    const total = coastDeltas.reduce((s, [, dy]) => s + dy, 0);
    expect(total).toBe(Math.round(T.momentumTauMs * (V0 - T.momentumMinVelocity)));
  });

  it("decay is time-based: late or irregular ticks cover the same distance", () => {
    const a = harness("touch");
    const b = harness("touch");
    coast(a.m, flickUp(a.m));
    coast(b.m, flickUp(b.m), (i) => [0, 37, 5, 90, 12][i % 5]!);
    const sum = (ev: Event[]) => scrollDeltas(ev).reduce((s, [, dy]) => s + dy, 0);
    expect(sum(b.events)).toBe(sum(a.events));
  });

  it("pausing before lifting → no coast", () => {
    const { m, events } = harness("touch");
    const deadline = flickUp(m, 96 + T.momentumSampleMs + 20);
    expect(deadline).toBeNull();
    expect(scrollDeltas(events).length).toBe(11); // only the live scroll
  });

  it("a new finger down stops the coast", () => {
    const { m, events } = harness("touch");
    let d = flickUp(m);
    d = m.tick(d!);
    d = m.tick(d!);
    const sofar = scrollDeltas(events).length;
    const next = m.onDown(3, 500, 500, d! - 5); // catch it
    expect(next).toBe(d! - 5 + T.longPressMs); // only the long-press deadline remains
    m.tick(d! + 100);
    m.onUp(3, 500, 500, d! + 120); // the catching touch is just a tap
    expect(scrollDeltas(events).length).toBe(sofar);
  });

  it("reset (transport teardown) stops the coast", () => {
    const { m, events } = harness("touch");
    const d = flickUp(m);
    const sofar = scrollDeltas(events).length;
    m.reset();
    expect(m.tick(d! + 500)).toBeNull();
    expect(scrollDeltas(events).length).toBe(sofar);
  });

  it("too slow to coast → nothing; the cap bounds a long coast", () => {
    const slow = harness("touch", { thresholds: { momentumMinVelocity: 2 } });
    expect(flickUp(slow.m)).toBeNull();

    const capped = harness("touch", { thresholds: { momentumMinVelocity: 1e-6, momentumMaxMs: 300 } });
    const times = coast(capped.m, flickUp(capped.m));
    expect(times.at(-1)).toBe(100 + 300);
  });

  it("a pinch never coasts", () => {
    const { m } = harness("touch");
    m.onDown(1, 200, 200, 0);
    m.onDown(2, 260, 200, 0);
    m.onMove(2, 380, 200, 16);
    expect(m.onUp(2, 380, 200, 20)).toBeNull();
  });
});
