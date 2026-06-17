import { describe, expect, it } from "vitest";
import { normalizedFromClient, type Rect } from "./input";

// Gate criterion #2: a click at a pinch-zoomed target maps to the exact host
// coordinate. The mapping is transform-invariant because both the pointer
// coordinate and the displayed rect (getBoundingClientRect) live in the same
// post-transform visual space. We model that here: applying a CSS-like
// scale+translate to the measured rect AND to the tapped point must leave the
// normalized coordinate unchanged.

/** Transform a rect the way getBoundingClientRect would report it under
 *  `transform-origin: 0 0; translate(tx,ty) scale(s)`. */
function transformedRect(r: Rect, s: number, tx: number, ty: number): Rect {
  return {
    left: r.left * s + tx,
    top: r.top * s + ty,
    width: r.width * s,
    height: r.height * s,
  };
}

/** Transform a client point the same way (a real pixel moves with the view). */
function transformedPoint(x: number, y: number, s: number, tx: number, ty: number) {
  return { x: x * s + tx, y: y * s + ty };
}

describe("pinch-zoom mapping invariance", () => {
  const rect: Rect = { left: 100, top: 50, width: 1000, height: 625 };

  it("normalized coordinate is identical zoomed vs unzoomed", () => {
    // a target at 30% / 70% of the content
    const baseX = rect.left + 0.3 * rect.width;
    const baseY = rect.top + 0.7 * rect.height;
    const base = normalizedFromClient(baseX, baseY, rect);

    const cases: Array<[number, number, number]> = [
      [2, -300, -120],
      [3, -800, -400],
      [1.5, -50, 0],
    ];
    for (const [s, tx, ty] of cases) {
      const zr = transformedRect(rect, s, tx, ty);
      const zp = transformedPoint(baseX, baseY, s, tx, ty);
      const zoomed = normalizedFromClient(zp.x, zp.y, zr);
      expect(Math.abs(zoomed.x - base.x)).toBeLessThanOrEqual(1);
      expect(Math.abs(zoomed.y - base.y)).toBeLessThanOrEqual(1);
    }
  });

  it("center of a 2x-zoomed view maps to the content center", () => {
    const s = 2;
    const tx = -500;
    const ty = -312;
    const zr = transformedRect(rect, s, tx, ty);
    // tap the visual center of the displayed (zoomed) content rect
    const cx = zr.left + zr.width / 2;
    const cy = zr.top + zr.height / 2;
    const n = normalizedFromClient(cx, cy, zr);
    expect(Math.abs(n.x - 32767)).toBeLessThanOrEqual(2);
    expect(Math.abs(n.y - 32767)).toBeLessThanOrEqual(2);
  });
});
