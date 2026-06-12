import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HostClipboard, PASTE_FALLBACK_MS, PasteFlow } from "./clipboard";

function makeFlow() {
  const calls: string[] = [];
  const flow = new PasteFlow({
    sendClipboard: (text) => calls.push(`clip:${text}`),
    sendKeyTap: (code, mods) => calls.push(`tap:${code}:${mods}`),
  });
  return { flow, calls };
}

describe("PasteFlow", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("paste event before the fallback: clipboard first, then the V tap", () => {
    const { flow, calls } = makeFlow();
    flow.onPasteCombo(0b1000); // cmd held
    expect(flow.onPasteEvent("copied text")).toBe(true);
    expect(calls).toEqual(["clip:copied text", "tap:KeyV:8"]);
    // fallback timer must not fire a second tap
    vi.advanceTimersByTime(PASTE_FALLBACK_MS * 2);
    expect(calls).toHaveLength(2);
  });

  it("no paste event: the fallback still delivers the keystroke", () => {
    const { flow, calls } = makeFlow();
    flow.onPasteCombo(0b1000);
    vi.advanceTimersByTime(PASTE_FALLBACK_MS + 1);
    expect(calls).toEqual(["tap:KeyV:8"]); // no clipboard send
  });

  it("empty clipboard text sends only the keystroke", () => {
    const { flow, calls } = makeFlow();
    flow.onPasteCombo(0);
    expect(flow.onPasteEvent(null)).toBe(true);
    expect(calls).toEqual(["tap:KeyV:0"]);
  });

  it("a paste event with no armed combo is not consumed", () => {
    const { flow, calls } = makeFlow();
    expect(flow.onPasteEvent("irrelevant")).toBe(false);
    expect(calls).toHaveLength(0);
  });

  it("auto-repeat keydowns while armed do not double-arm", () => {
    const { flow, calls } = makeFlow();
    flow.onPasteCombo(8);
    flow.onPasteCombo(8);
    flow.onPasteCombo(8);
    flow.onPasteEvent("once");
    vi.runAllTimers();
    expect(calls).toEqual(["clip:once", "tap:KeyV:8"]);
  });

  it("swallows exactly one KeyV keyup per consumed combo", () => {
    const { flow } = makeFlow();
    flow.onPasteCombo(8);
    flow.onPasteEvent("x");
    expect(flow.onPasteKeyUp()).toBe(true);
    expect(flow.onPasteKeyUp()).toBe(false); // an unrelated later keyup passes
  });
});

describe("HostClipboard", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("shows the chip when no Clipboard API exists (insecure context)", async () => {
    vi.stubGlobal("navigator", {});
    const chips: boolean[] = [];
    const hc = new HostClipboard((v) => chips.push(v));
    await hc.receive("hello");
    expect(chips).toEqual([true]);
  });

  it("auto-writes and hides the chip when the Clipboard API works", async () => {
    const written: string[] = [];
    vi.stubGlobal("navigator", {
      clipboard: { writeText: async (t: string) => void written.push(t) },
    });
    const chips: boolean[] = [];
    const hc = new HostClipboard((v) => chips.push(v));
    await hc.receive("auto");
    expect(written).toEqual(["auto"]);
    expect(chips).toEqual([false]);
  });

  it("falls back to the chip when writeText rejects", async () => {
    vi.stubGlobal("navigator", {
      clipboard: {
        writeText: async () => {
          throw new Error("denied");
        },
      },
    });
    const chips: boolean[] = [];
    const hc = new HostClipboard((v) => chips.push(v));
    await hc.receive("denied");
    expect(chips).toEqual([true]);
  });
});
