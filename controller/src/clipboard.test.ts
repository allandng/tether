import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  HostClipboard,
  LARGE_PASTE_BYTES,
  LARGE_PASTE_DELAY_MS,
  PASTE_FALLBACK_MS,
  PasteFlow,
  pasteModifiers,
  readLocalClipboard,
} from "./clipboard";
import { HostOs, MOD_CTRL, MOD_META } from "./protocol";

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

  it("large hardware pastes give the clipboard a head start", () => {
    const { flow, calls } = makeFlow();
    const big = "x".repeat(LARGE_PASTE_BYTES);
    flow.onPasteCombo(8);
    flow.onPasteEvent(big);
    expect(calls).toEqual([`clip:${big}`]);
    vi.advanceTimersByTime(LARGE_PASTE_DELAY_MS);
    expect(calls).toEqual([`clip:${big}`, "tap:KeyV:8"]);
  });
});

describe("PasteFlow.pasteText (Paste button)", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("syncs the clipboard, then taps V with the given modifiers", () => {
    const { flow, calls } = makeFlow();
    flow.pasteText("from phone", MOD_META);
    expect(calls).toEqual(["clip:from phone", `tap:KeyV:${MOD_META}`]);
    vi.runAllTimers();
    expect(calls).toHaveLength(2); // no fallback timer involved
  });

  it("large text: the V tap waits like a hardware paste", () => {
    const { flow, calls } = makeFlow();
    const big = "y".repeat(LARGE_PASTE_BYTES + 1);
    flow.pasteText(big, MOD_CTRL);
    expect(calls).toEqual([`clip:${big}`]);
    vi.advanceTimersByTime(LARGE_PASTE_DELAY_MS - 1);
    expect(calls).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(calls).toEqual([`clip:${big}`, `tap:KeyV:${MOD_CTRL}`]);
  });

  it("empty text sends only the keystroke, as a hardware paste would", () => {
    const { flow, calls } = makeFlow();
    flow.pasteText("", MOD_META);
    expect(calls).toEqual([`tap:KeyV:${MOD_META}`]);
  });

  it("has no keyup to swallow and leaves an armed combo alone", () => {
    const { flow, calls } = makeFlow();
    flow.pasteText("a", MOD_META);
    expect(flow.onPasteKeyUp()).toBe(false); // a later real KeyV keyup still passes
    flow.onPasteCombo(MOD_META);
    flow.pasteText("b", MOD_META);
    expect(flow.onPasteEvent("c")).toBe(true); // the combo is still armed
    expect(calls).toEqual([
      "clip:a",
      `tap:KeyV:${MOD_META}`,
      "clip:b",
      `tap:KeyV:${MOD_META}`,
      "clip:c",
      `tap:KeyV:${MOD_META}`,
    ]);
  });
});

describe("pasteModifiers", () => {
  it("Cmd for Macs (and pre-HostInfo hosts), and when the remap makes it Ctrl", () => {
    expect(pasteModifiers(HostOs.MacOs, false)).toBe(MOD_META);
    expect(pasteModifiers(null, false)).toBe(MOD_META);
    expect(pasteModifiers(HostOs.Unknown, false)).toBe(MOD_META);
    expect(pasteModifiers(HostOs.Windows, true)).toBe(MOD_META); // Apple client: remapped
  });

  it("Ctrl where no remap applies and the host pastes with Ctrl", () => {
    expect(pasteModifiers(HostOs.Windows, false)).toBe(MOD_CTRL);
    expect(pasteModifiers(HostOs.Linux, false)).toBe(MOD_CTRL);
  });
});

describe("readLocalClipboard", () => {
  it("null at once without the async Clipboard API", () => {
    expect(readLocalClipboard({})).toBeNull();
    expect(readLocalClipboard(undefined)).toBeNull();
    expect(readLocalClipboard({ clipboard: {} })).toBeNull();
  });

  it("resolves the clipboard text", async () => {
    await expect(readLocalClipboard({ clipboard: { readText: async () => "hi" } })).resolves.toBe("hi");
  });

  it("a refused read (NotAllowedError) resolves null instead of rejecting", async () => {
    const refused = Object.assign(new Error("denied"), { name: "NotAllowedError" });
    const read = readLocalClipboard({
      clipboard: {
        readText: () => Promise.reject(refused),
      },
    });
    await expect(read).resolves.toBeNull();
  });

  it("a throwing implementation resolves null too", async () => {
    const read = readLocalClipboard({
      clipboard: {
        readText: () => {
          throw new Error("boom");
        },
      },
    });
    await expect(read).resolves.toBeNull();
  });

  it("calls readText synchronously, inside the gesture", () => {
    let called = false;
    readLocalClipboard({
      clipboard: {
        readText: () => {
          called = true;
          return Promise.resolve("");
        },
      },
    });
    expect(called).toBe(true);
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
