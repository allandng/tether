import { describe, expect, it } from "vitest";
import { modifierMask, normalizedFromClient, wheelToPixels } from "./input";
import { MOD_CTRL, MOD_META, MOD_SHIFT } from "./protocol";

describe("normalizedFromClient", () => {
  // Viewer letterboxed: 16:10 content displayed at 800x500 offset (100, 50).
  const rect = { left: 100, top: 50, width: 800, height: 500 };

  it("maps corners to coordinate extremes", () => {
    expect(normalizedFromClient(100, 50, rect)).toEqual({ x: 0, y: 0 });
    expect(normalizedFromClient(900, 550, rect)).toEqual({ x: 65535, y: 65535 });
  });

  it("maps the center to the middle of the range", () => {
    const { x, y } = normalizedFromClient(500, 300, rect);
    expect(Math.abs(x - 32768)).toBeLessThanOrEqual(1);
    expect(Math.abs(y - 32768)).toBeLessThanOrEqual(1);
  });

  it("clamps points outside the displayed rect (letterbox bars)", () => {
    expect(normalizedFromClient(0, 0, rect)).toEqual({ x: 0, y: 0 });
    expect(normalizedFromClient(2000, 9000, rect)).toEqual({ x: 65535, y: 65535 });
  });

  it("is resolution-independent: same fraction, same wire value", () => {
    const small = { left: 0, top: 0, width: 400, height: 250 };
    const large = { left: 0, top: 0, width: 3456, height: 2160 };
    expect(normalizedFromClient(100, 125, small).x).toBe(
      normalizedFromClient(864, 1080, large).x,
    );
  });
});

describe("modifierMask", () => {
  it("combines held modifiers", () => {
    expect(
      modifierMask({ shiftKey: true, ctrlKey: false, altKey: false, metaKey: true }),
    ).toBe(MOD_SHIFT | MOD_META);
    expect(
      modifierMask({ shiftKey: false, ctrlKey: true, altKey: false, metaKey: false }),
    ).toBe(MOD_CTRL);
    expect(
      modifierMask({ shiftKey: false, ctrlKey: false, altKey: false, metaKey: false }),
    ).toBe(0);
  });
});

describe("wheelToPixels", () => {
  it("passes pixel deltas through", () => {
    expect(wheelToPixels(120, 0)).toBe(120);
    expect(wheelToPixels(-3.7, 0)).toBe(-4);
  });
  it("converts line deltas", () => {
    expect(wheelToPixels(3, 1)).toBe(48);
  });
  it("clamps to i16", () => {
    expect(wheelToPixels(99999, 0)).toBe(32767);
    expect(wheelToPixels(-99999, 0)).toBe(-32768);
  });
});
