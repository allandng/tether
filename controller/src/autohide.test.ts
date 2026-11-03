import { describe, expect, it } from "vitest";
import { AUTO_HIDE_MS, BarAutoHide, type BarConditions, decideBar, isPinned } from "./autohide";

const free: BarConditions = { connected: true, autoHide: true, held: false };

describe("decideBar", () => {
  it("pins the bar while not connected, with auto-hide off, or while held", () => {
    for (const c of [
      { ...free, connected: false },
      { ...free, autoHide: false },
      { ...free, held: true },
    ]) {
      expect(isPinned(c)).toBe(true);
      // however long ago the last interaction was
      expect(decideBar(c, 0, 1_000_000)).toEqual({ visible: true, deadline: null });
    }
    expect(isPinned(free)).toBe(false);
  });

  it("shows the bar until the delay after the last interaction, then hides it", () => {
    expect(decideBar(free, 1000, 1000)).toEqual({ visible: true, deadline: 1000 + AUTO_HIDE_MS });
    expect(decideBar(free, 1000, 1000 + AUTO_HIDE_MS - 1)).toEqual({
      visible: true,
      deadline: 1000 + AUTO_HIDE_MS,
    });
    expect(decideBar(free, 1000, 1000 + AUTO_HIDE_MS)).toEqual({ visible: false, deadline: null });
  });

  it("takes a custom delay", () => {
    expect(decideBar(free, 0, 400, 500)).toEqual({ visible: true, deadline: 500 });
    expect(decideBar(free, 0, 500, 500).visible).toBe(false);
  });
});

describe("BarAutoHide", () => {
  /** A session that connected at `at` with auto-hide on. */
  function connectedAt(at: number) {
    const m = new BarAutoHide(0);
    m.update({ ...free, connected: false }, 0); // the bar starts out pinned
    return { m, first: m.update(free, at) };
  }

  it("counts down from connecting, not from page load", () => {
    const { m, first } = connectedAt(60_000);
    expect(first).toEqual({ visible: true, deadline: 60_000 + AUTO_HIDE_MS });
    expect(m.update(free, 60_000 + AUTO_HIDE_MS - 1).visible).toBe(true);
    expect(m.update(free, 60_000 + AUTO_HIDE_MS)).toEqual({ visible: false, deadline: null });
  });

  it("re-arms the same deadline when the timer fires early", () => {
    const { m } = connectedAt(0);
    expect(m.update(free, AUTO_HIDE_MS - 3)).toEqual({ visible: true, deadline: AUTO_HIDE_MS });
  });

  it("restarts the countdown on bar interaction", () => {
    const { m } = connectedAt(0);
    m.interact(2000);
    expect(m.update(free, 2000)).toEqual({ visible: true, deadline: 2000 + AUTO_HIDE_MS });
    expect(m.update(free, AUTO_HIDE_MS).visible).toBe(true); // the original deadline passed
    expect(m.update(free, 2000 + AUTO_HIDE_MS).visible).toBe(false);
  });

  it("brings a hidden bar back on the pull-tab, then hides it again", () => {
    const { m } = connectedAt(0);
    expect(m.update(free, 10_000).visible).toBe(false);
    m.interact(20_000);
    expect(m.update(free, 20_000)).toEqual({ visible: true, deadline: 20_000 + AUTO_HIDE_MS });
    expect(m.update(free, 20_000 + AUTO_HIDE_MS).visible).toBe(false);
  });

  it("stays hidden through updates that change nothing", () => {
    const { m } = connectedAt(0);
    expect(m.update(free, 10_000).visible).toBe(false);
    expect(m.update(free, 10_001)).toEqual({ visible: false, deadline: null });
    expect(m.update(free, 99_000)).toEqual({ visible: false, deadline: null });
  });

  it("never hides while held, and restarts the countdown when the hold ends", () => {
    const { m } = connectedAt(0);
    // a menu opened at 1 s and stayed open long past the deadline
    expect(m.update({ ...free, held: true }, 1000)).toEqual({ visible: true, deadline: null });
    expect(m.update({ ...free, held: true }, 30_000)).toEqual({ visible: true, deadline: null });
    // closed at 30 s: the bar stays up for another full delay
    expect(m.update(free, 30_000)).toEqual({ visible: true, deadline: 30_000 + AUTO_HIDE_MS });
    expect(m.update(free, 30_000 + AUTO_HIDE_MS).visible).toBe(false);
  });

  it("shows a hidden bar the moment the connection drops", () => {
    const { m } = connectedAt(0);
    expect(m.update(free, 10_000).visible).toBe(false);
    expect(m.update({ ...free, connected: false }, 11_000)).toEqual({ visible: true, deadline: null });
    // reconnected: up for a full delay, then away again
    expect(m.update(free, 15_000)).toEqual({ visible: true, deadline: 15_000 + AUTO_HIDE_MS });
    expect(m.update(free, 15_000 + AUTO_HIDE_MS).visible).toBe(false);
  });

  it("stays pinned with auto-hide off and counts down once it's switched on", () => {
    const m = new BarAutoHide(0);
    const off = { ...free, autoHide: false };
    expect(m.update(off, 0)).toEqual({ visible: true, deadline: null });
    expect(m.update(off, 50_000)).toEqual({ visible: true, deadline: null });
    expect(m.update(free, 50_000)).toEqual({ visible: true, deadline: 50_000 + AUTO_HIDE_MS });
    // and switching it off again brings a hidden bar back
    expect(m.update(free, 60_000).visible).toBe(false);
    expect(m.update(off, 60_001)).toEqual({ visible: true, deadline: null });
  });

  it("treats an interaction during a hold as harmless", () => {
    const { m } = connectedAt(0);
    m.interact(500);
    expect(m.update({ ...free, held: true }, 500).visible).toBe(true);
    expect(m.update(free, 4000)).toEqual({ visible: true, deadline: 4000 + AUTO_HIDE_MS });
  });

  it("honors a custom delay", () => {
    const m = new BarAutoHide(0, 1000);
    m.update({ ...free, connected: false }, 0);
    expect(m.update(free, 100)).toEqual({ visible: true, deadline: 1100 });
    expect(m.update(free, 1100).visible).toBe(false);
  });
});
