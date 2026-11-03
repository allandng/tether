import { describe, expect, it } from "vitest";
import { attachInput, modifierMask, normalizedFromClient, wheelToPixels } from "./input";
import { MOD_CAPS_LOCK, MOD_CAPS_LOCK_KNOWN, MOD_CTRL, MOD_META, MOD_SHIFT } from "./protocol";
import type { Viewer } from "./viewer";

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

  it("reports the Caps Lock state when the event can say it", () => {
    const none = { shiftKey: false, ctrlKey: false, altKey: false, metaKey: false };
    const capsOn = { ...none, getModifierState: (k: string) => k === "CapsLock" };
    const capsOff = { ...none, getModifierState: () => false };
    expect(modifierMask(capsOn)).toBe(MOD_CAPS_LOCK_KNOWN | MOD_CAPS_LOCK);
    expect(modifierMask(capsOff)).toBe(MOD_CAPS_LOCK_KNOWN);
    expect(modifierMask({ ...capsOn, shiftKey: true })).toBe(
      MOD_SHIFT | MOD_CAPS_LOCK_KNOWN | MOD_CAPS_LOCK,
    );
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

describe("attachInput (touch)", () => {
  // iOS WebKit turns an uncancelled tap into mouse events whose mousedown
  // focuses the canvas — blurring the soft keyboard's field, which dismisses
  // the keyboard on every tap on the remote screen (seen in the Simulator).
  it("cancels touchend so no focus-stealing mouse tap is synthesized", () => {
    const canvas = new EventTarget() as unknown as HTMLCanvasElement;
    const rect = { left: 0, top: 0, width: 100, height: 100 };
    const viewer = {
      displayedRect: () => rect,
      visibleRect: () => rect,
      setCursor: () => {},
      hideCursor: () => {},
      cursorVisible: false,
    } as unknown as Viewer;
    attachInput(canvas, viewer, { sendInput: () => {} });
    const ev = new Event("touchend", { cancelable: true });
    canvas.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
  });
});
