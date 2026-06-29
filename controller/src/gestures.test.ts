import { describe, expect, it } from "vitest";
import {
  DEFAULT_THRESHOLDS,
  GestureMachine,
  type GestureConfig,
  type GestureSink,
  type Mode,
} from "./gestures";

type Event = string;

function harness(mode: Mode, overrides: Partial<GestureConfig> = {}) {
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

describe("two-finger gestures", () => {
  it("two-finger drag → scroll, content follows fingers (dy>0 down)", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 140, 100, 5); // d0=40, c0=(120,100)
    // both fingers slide down; pointer events arrive one finger at a time, so
    // the centroid advances in increments → several positive-dy scrolls.
    m.onMove(1, 100, 140, 30);
    m.onMove(2, 140, 140, 30);
    m.onMove(1, 100, 180, 60);
    m.onMove(2, 140, 180, 60);
    m.onUp(1, 100, 180, 90);
    m.onUp(2, 140, 180, 95);
    const scrolls = events.filter((e) => e.startsWith("scroll"));
    expect(scrolls.length).toBeGreaterThanOrEqual(2);
    for (const s of scrolls) {
      const [, dx, dy] = s.match(/scroll\((-?\d+),(-?\d+)\)/)!.map(Number);
      expect(dx).toBe(0);
      expect(dy).toBeGreaterThan(0); // fingers down → content scrolls down
    }
    expect(events).not.toContain("click(0)");
    expect(events.some((e) => e.startsWith("zoom"))).toBe(false);
  });

  it("two-finger tap → right click", () => {
    const { m, events } = harness("touch");
    m.onDown(1, 100, 100, 0);
    m.onDown(2, 160, 100, 8);
    m.onUp(1, 101, 100, 120);
    m.onUp(2, 159, 100, 130);
    expect(events).toEqual(["move(130,100)", "click(2)"]);
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
