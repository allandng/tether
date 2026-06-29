import { describe, expect, it } from "vitest";
import {
  CAP_CAN_CONTROL,
  Codec,
  type Message,
  PROTOCOL_VERSION,
  Role,
  decodeMessage,
  encodeMessage,
} from "./protocol";

function roundTrip(message: Message) {
  const wire = encodeMessage(message);
  const result = decodeMessage(wire);
  expect(result).toEqual({ ok: true, message });
}

describe("round trips", () => {
  const cases: Message[] = [
    { type: "hello", version: PROTOCOL_VERSION, role: Role.Controller, capabilities: CAP_CAN_CONTROL },
    { type: "resolution", width: 3456, height: 2234 },
    {
      type: "frame",
      codec: Codec.Jpeg,
      seq: 42,
      timestampMicros: 1_718_000_000_123_456,
      payload: new Uint8Array([0xff, 0xd8, 0xff, 0xd9]),
    },
    { type: "input", kind: "mousemove", x: 0, y: 65535 },
    { type: "input", kind: "mousedown", button: 2, x: 32768, y: 1 },
    { type: "input", kind: "mouseup", button: 0, x: 0, y: 0 },
    { type: "input", kind: "scroll", dx: -120, dy: 240 },
    { type: "input", kind: "keydown", code: "KeyA", modifiers: 0b1001 },
    { type: "input", kind: "keyup", code: "MetaLeft", modifiers: 0 },
    { type: "clipboard", text: "héllo 📋" },
    { type: "clipboard", text: "" },
    { type: "text", text: "a" },
    { type: "text", text: "señor 🎯" },
    { type: "text", text: "" },
  ];
  for (const message of cases) {
    it(JSON.stringify(message.type === "input" ? message.kind : `${message.type}:${"text" in message ? message.text : ""}`), () => {
      roundTrip(message);
    });
  }
});

describe("cross-implementation byte vectors (pin the wire format)", () => {
  // Hand-derived from docs/protocol.md; the Rust side encodes identically.
  it("Hello", () => {
    const wire = encodeMessage({
      type: "hello",
      version: 1,
      role: Role.Controller,
      capabilities: CAP_CAN_CONTROL,
    });
    expect(Array.from(wire)).toEqual([
      5, 0, 0, 0, // total_len = 5
      0x01, // Hello
      1, 0, // version 1
      1, // role controller
      2, // can_control
    ]);
  });

  it("Resolution", () => {
    const wire = encodeMessage({ type: "resolution", width: 1920, height: 1080 });
    expect(Array.from(wire)).toEqual([
      9, 0, 0, 0,
      0x02,
      0x80, 0x07, 0, 0, // 1920
      0x38, 0x04, 0, 0, // 1080
    ]);
  });

  it("FrameData", () => {
    const wire = encodeMessage({
      type: "frame",
      codec: Codec.Jpeg,
      seq: 7,
      timestampMicros: 0x01_0000_0002, // exercises both u32 halves
      payload: new Uint8Array([0xab, 0xcd]),
    });
    expect(Array.from(wire)).toEqual([
      16, 0, 0, 0, // 1 + 1 + 4 + 8 + 2
      0x03,
      0, // jpeg
      7, 0, 0, 0,
      2, 0, 0, 0, 1, 0, 0, 0, // 0x0100000002 LE
      0xab, 0xcd,
    ]);
  });

  it("KeyDown shift+cmd KeyA", () => {
    const wire = encodeMessage({
      type: "input",
      kind: "keydown",
      code: "KeyA",
      modifiers: 0b1001,
    });
    expect(Array.from(wire)).toEqual([
      8, 0, 0, 0, // 1 + 1 + 1 + 1 + 4
      0x04,
      4, // KeyDown
      0b1001,
      4, // code_len
      0x4b, 0x65, 0x79, 0x41, // "KeyA"
    ]);
  });

  it("MouseMove extremes", () => {
    const wire = encodeMessage({ type: "input", kind: "mousemove", x: 0, y: 65535 });
    expect(Array.from(wire)).toEqual([6, 0, 0, 0, 0x04, 0, 0, 0, 0xff, 0xff]);
  });

  it("ClipboardData", () => {
    const wire = encodeMessage({ type: "clipboard", text: "hi" });
    expect(Array.from(wire)).toEqual([4, 0, 0, 0, 0x05, 0x00, 0x68, 0x69]);
  });

  it("TextInput", () => {
    const wire = encodeMessage({ type: "text", text: "hi" });
    expect(Array.from(wire)).toEqual([3, 0, 0, 0, 0x06, 0x68, 0x69]);
  });

  it("Auth encodes to the Rust serde shape", () => {
    const wire = encodeMessage({ type: "auth", deviceId: "hi", token: "ok" });
    expect(Array.from(wire)).toEqual([9, 0, 0, 0, 0x09, 2, 0, 0x68, 0x69, 2, 0, 0x6f, 0x6b]);
  });

  it("decodes a host PairResult", () => {
    // Rust: PairResult{ok:true, token:"ok"} → len 6
    const res = decodeMessage(new Uint8Array([6, 0, 0, 0, 0x08, 1, 2, 0, 0x6f, 0x6b]));
    expect(res).toEqual({ ok: true, message: { type: "pair_result", ok: true, token: "ok" } });
  });

  it("SelectDisplay encodes to the Rust serde shape", () => {
    const wire = encodeMessage({ type: "select_display", id: 0x02 });
    expect(Array.from(wire)).toEqual([5, 0, 0, 0, 0x0c, 0x02, 0, 0, 0]);
  });

  it("decodes a host Displays (matches the Rust byte vector)", () => {
    const wire = new Uint8Array([
      18, 0, 0, 0, 0x0b, 1, 1, 0, 0, 0, 0x20, 0x03, 0, 0, 0x58, 0x02, 0, 0, 1, 1, 0, 0x58,
    ]);
    expect(decodeMessage(wire)).toEqual({
      ok: true,
      message: {
        type: "displays",
        displays: [{ id: 1, width: 800, height: 600, active: true, name: "X" }],
      },
    });
  });

  it("decodes a host AuthResult", () => {
    expect(decodeMessage(new Uint8Array([2, 0, 0, 0, 0x0a, 1]))).toEqual({
      ok: true,
      message: { type: "auth_result", ok: true },
    });
    expect(decodeMessage(new Uint8Array([2, 0, 0, 0, 0x0a, 0]))).toEqual({
      ok: true,
      message: { type: "auth_result", ok: false },
    });
  });
});

describe("rejects corrupt input", () => {
  it("truncated buffer", () => {
    expect(decodeMessage(new Uint8Array([5, 0, 0]))).toMatchObject({ ok: false, reason: "corrupt" });
  });

  it("length mismatch", () => {
    const wire = new Uint8Array(encodeMessage({ type: "resolution", width: 1, height: 1 }));
    expect(decodeMessage(wire.subarray(0, wire.length - 1))).toMatchObject({
      ok: false,
      reason: "corrupt",
    });
  });

  it("unknown message type is reported, not fatal", () => {
    expect(decodeMessage(new Uint8Array([3, 0, 0, 0, 0x7f, 1, 2]))).toEqual({
      ok: false,
      reason: "unknown-type",
      msgType: 0x7f,
    });
  });

  it("oversized key code", () => {
    expect(() =>
      encodeMessage({ type: "input", kind: "keydown", code: "x".repeat(40), modifiers: 0 }),
    ).toThrow();
  });

  it("oversized clipboard refused on encode and decode", () => {
    expect(() =>
      encodeMessage({ type: "clipboard", text: "x".repeat(256 * 1024 + 1) }),
    ).toThrow();
    expect(decodeMessage(new Uint8Array([2, 0, 0, 0, 0x05, 9]))).toMatchObject({
      ok: false,
      reason: "corrupt", // unknown clipboard kind
    });
  });

  it("oversized text input refused on encode", () => {
    expect(() => encodeMessage({ type: "text", text: "x".repeat(1025) })).toThrow();
  });
});
