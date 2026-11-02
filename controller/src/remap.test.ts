import { describe, expect, it } from "vitest";
import { HostOs, MOD_CTRL, MOD_META, MOD_SHIFT } from "./protocol";
import { cmdActsAsCtrl, isAppleClient, remapInput } from "./remap";

describe("modifier policy", () => {
  it("only an Apple client against a Windows host remaps", () => {
    expect(cmdActsAsCtrl(HostOs.Windows, true)).toBe(true);
    expect(cmdActsAsCtrl(HostOs.Windows, false)).toBe(false);
    expect(cmdActsAsCtrl(HostOs.MacOs, true)).toBe(false);
    expect(cmdActsAsCtrl(null, true)).toBe(false); // old host: no HostInfo
  });

  it("Cmd+C becomes Ctrl+C; a real Ctrl passes through", () => {
    const down = remapInput({ type: "input", kind: "keydown", code: "MetaLeft", modifiers: MOD_META }, true);
    expect(down).toEqual({ type: "input", kind: "keydown", code: "ControlLeft", modifiers: MOD_CTRL });
    const c = remapInput(
      { type: "input", kind: "keydown", code: "KeyC", modifiers: MOD_META | MOD_SHIFT },
      true,
    );
    expect(c).toEqual({ type: "input", kind: "keydown", code: "KeyC", modifiers: MOD_CTRL | MOD_SHIFT });
    const ctrl = { type: "input" as const, kind: "keyup" as const, code: "ControlRight", modifiers: MOD_CTRL };
    expect(remapInput(ctrl, true)).toEqual(ctrl);
    expect(remapInput({ type: "input", kind: "keyup", code: "MetaRight", modifiers: 0 }, true)).toMatchObject({
      code: "ControlRight",
    });
  });

  it("leaves everything alone when off, and never touches mouse events", () => {
    const meta = { type: "input" as const, kind: "keydown" as const, code: "MetaLeft", modifiers: MOD_META };
    expect(remapInput(meta, false)).toBe(meta);
    const move = { type: "input" as const, kind: "mousemove" as const, x: 1, y: 2 };
    expect(remapInput(move, true)).toBe(move);
  });

  it("detects Apple platforms", () => {
    expect(isAppleClient({ platform: "MacIntel" })).toBe(true);
    expect(isAppleClient({ platform: "iPhone" })).toBe(true);
    expect(isAppleClient({ userAgent: "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)" })).toBe(true);
    expect(isAppleClient({ platform: "Win32", userAgent: "Windows NT 10.0" })).toBe(false);
    expect(isAppleClient({ platform: "Linux armv8l", userAgent: "Android 14" })).toBe(false);
  });
});
