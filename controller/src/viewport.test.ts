import { describe, expect, it } from "vitest";
import { cssVars, viewportVars } from "./viewport";

describe("viewportVars", () => {
  it("null where visualViewport is missing (CSS fallbacks apply)", () => {
    expect(viewportVars(null, 714)).toBeNull();
    expect(viewportVars(undefined, 714)).toBeNull();
  });

  it("null while the whole layout viewport is visible", () => {
    expect(viewportVars({ height: 714, offsetTop: 0, scale: 1 }, 714)).toBeNull();
    // sub-pixel rounding noise isn't a keyboard
    expect(viewportVars({ height: 713.4, offsetTop: 0 }, 714)).toBeNull();
  });

  it("iOS keyboard up: the visible part above it (iOS 26.5 Simulator numbers)", () => {
    expect(viewportVars({ height: 404, offsetTop: 0, scale: 1 }, 714)).toEqual({
      vvh: 404,
      vvTop: 0,
      vvBottom: 310,
    });
  });

  it("follows a panned visual viewport", () => {
    expect(viewportVars({ height: 404, offsetTop: 120, scale: 1 }, 714)).toEqual({
      vvh: 404,
      vvTop: 120,
      vvBottom: 190,
    });
  });

  it("rounds to whole pixels and clamps overscroll and overflow", () => {
    expect(viewportVars({ height: 404.6, offsetTop: 10.4 }, 714)).toEqual({ vvh: 405, vvTop: 10, vvBottom: 299 });
    // rubber-band bounce: negative offset
    expect(viewportVars({ height: 404, offsetTop: -30 }, 714)).toEqual({ vvh: 404, vvTop: 0, vvBottom: 310 });
    // never reaches past the layout bottom
    expect(viewportVars({ height: 700, offsetTop: 100 }, 714)).toEqual({ vvh: 614, vvTop: 100, vvBottom: 0 });
  });

  it("ignores pinch zoom (the layout shouldn't chase it)", () => {
    expect(viewportVars({ height: 357, offsetTop: 200, scale: 2 }, 714)).toBeNull();
  });

  it("null for degenerate sizes", () => {
    expect(viewportVars({ height: 0, offsetTop: 0 }, 714)).toBeNull();
    expect(viewportVars({ height: Number.NaN, offsetTop: 0 }, 714)).toBeNull();
    expect(viewportVars({ height: 404, offsetTop: 0 }, 0)).toBeNull();
  });
});

describe("cssVars", () => {
  it("formats px values for :root", () => {
    expect(cssVars({ vvh: 404, vvTop: 0, vvBottom: 310 })).toEqual({
      "--vvh": "404px",
      "--vv-top": "0px",
      "--vv-bottom": "310px",
    });
  });
});
