import { describe, expect, it } from "vitest";
import { Viewer, clampPan, containRect } from "./viewer";

/** Just enough canvas for the zoom/pan state: a CSS box (clientWidth/Height)
 * showing content of the given backing size. No DOM in this environment. */
function stubCanvas(boxW: number, boxH: number, w: number, h: number) {
  const c = {
    width: w,
    height: h,
    clientWidth: boxW,
    clientHeight: boxH,
    style: { transform: "", transformOrigin: "" },
    parentElement: null,
    getContext: () => ({}),
    getBoundingClientRect: () => ({ left: 0, top: 0, width: c.clientWidth, height: c.clientHeight }),
  };
  return c;
}

function viewerOn(c: ReturnType<typeof stubCanvas>) {
  const v = new Viewer(c as unknown as HTMLCanvasElement);
  const zoomed: boolean[] = [];
  v.onZoomedChange = (z) => zoomed.push(z);
  return { v, zoomed };
}

describe("containRect", () => {
  it("pillarboxes content narrower than the box", () => {
    const r = containRect(1000, 500, 800, 600); // 4:3 in 2:1
    expect(r.height).toBe(500);
    expect(r.width).toBeCloseTo(666.667, 2);
    expect(r.left).toBeCloseTo(166.667, 2);
    expect(r.top).toBe(0);
  });

  it("letterboxes content wider than the box", () => {
    expect(containRect(400, 800, 1600, 900)).toEqual({ left: 0, top: 287.5, width: 400, height: 225 });
  });
});

describe("clampPan", () => {
  const full = { left: 0, top: 0, width: 1000, height: 500 };

  it("1× has no pan", () => {
    expect(clampPan(1, 40, -30, 1000, 500, full)).toEqual({ tx: 0, ty: 0 });
  });

  it("keeps a zoomed view's edges from crossing into the box", () => {
    expect(clampPan(2, 100, -900, 1000, 500, full)).toEqual({ tx: 0, ty: -500 });
    expect(clampPan(2, -300, -200, 1000, 500, full)).toEqual({ tx: -300, ty: -200 });
  });

  it("centers an axis whose scaled content is still smaller than the box", () => {
    // landscape desktop letterboxed on a portrait phone, 2×: wider than the
    // box horizontally (pannable), still shorter than it vertically (centered)
    const content = containRect(400, 800, 1600, 900);
    expect(clampPan(2, 50, 0, 400, 800, content)).toEqual({ tx: 0, ty: -400 });
  });
});

describe("Viewer zoom state", () => {
  it("reports entering and leaving zoom once each; resetZoom returns to 1×", () => {
    const c = stubCanvas(1000, 500, 1000, 500);
    const { v, zoomed } = viewerOn(c);
    v.applyZoom(2, 500, 250);
    v.applyZoom(2.5, 500, 250);
    v.endZoom();
    expect(v.zoom).toBe(2.5);
    expect(c.style.transform).toContain("scale(2.5)");
    expect(zoomed).toEqual([true]);

    v.resetZoom();
    expect(v.zoom).toBe(1);
    expect(c.style.transform).toBe("");
    expect(zoomed).toEqual([true, false]);
    v.resetZoom(); // already at 1×: no change, no event
    expect(zoomed).toEqual([true, false]);
  });

  it("pinching back down to 1× also reports un-zoomed", () => {
    const { v, zoomed } = viewerOn(stubCanvas(1000, 500, 1000, 500));
    v.applyZoom(2, 500, 250);
    v.applyZoom(0.4, 500, 250); // clamps at 1×
    v.endZoom();
    expect(v.zoom).toBe(1);
    expect(zoomed).toEqual([true, false]);
  });

  it("panBy applies only what the clamp allows and says so", () => {
    const c = stubCanvas(1000, 500, 1000, 500);
    const { v } = viewerOn(c);
    expect(v.panBy(-50, 0)).toEqual({ dx: 0, dy: 0 }); // 1×: nothing to pan
    v.applyZoom(2, 500, 250); // translate(-500, -250)
    v.endZoom();
    expect(v.panBy(-300, 0)).toEqual({ dx: -300, dy: 0 });
    expect(v.panBy(-300, 0)).toEqual({ dx: -200, dy: 0 }); // right edge reached at -1000
    expect(v.panBy(0, 400)).toEqual({ dx: 0, dy: 250 }); // top edge reached at 0
    expect(c.style.transform).toBe("translate(-1000px, 0px) scale(2)");
  });

  it("relayout re-clamps the pan to a new box (rotation)", () => {
    const c = stubCanvas(1000, 500, 1000, 500);
    const { v } = viewerOn(c);
    v.applyZoom(2, 1000, 500); // zoomed into the bottom-right corner
    v.endZoom();
    expect(c.style.transform).toBe("translate(-1000px, -500px) scale(2)");
    // rotate to portrait: the landscape desktop letterboxes into 500×1000
    c.clientWidth = 500;
    c.clientHeight = 1000;
    v.relayout();
    expect(c.style.transform).toBe("translate(-500px, -500px) scale(2)");
    expect(v.zoom).toBe(2);
  });

  it("relayout leaves the pan alone while the view is hidden (0×0 box)", () => {
    const c = stubCanvas(1000, 500, 1000, 500);
    const { v } = viewerOn(c);
    v.applyZoom(2, 1000, 500);
    v.endZoom();
    c.clientWidth = 0;
    c.clientHeight = 0;
    v.relayout();
    expect(c.style.transform).toBe("translate(-1000px, -500px) scale(2)");
  });
});
