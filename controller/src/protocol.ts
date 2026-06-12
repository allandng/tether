// Tether wire protocol v1 — TypeScript mirror of crates/tether-protocol.
// Must stay byte-identical to the Rust implementation; docs/protocol.md is
// the spec, and protocol.test.ts pins cross-implementation test vectors.

export const PROTOCOL_VERSION = 1;
export const MAX_MESSAGE_LEN = 64 * 1024 * 1024;
export const MAX_KEY_CODE_LEN = 32;

export const CAP_CAN_HOST = 0b01;
export const CAP_CAN_CONTROL = 0b10;

export const enum Role {
  Host = 0,
  Controller = 1,
}

export const enum Codec {
  Jpeg = 0,
  H264 = 1,
}

export const MOD_SHIFT = 0b0001;
export const MOD_CTRL = 0b0010;
export const MOD_ALT = 0b0100;
export const MOD_META = 0b1000;

const enum MsgType {
  Hello = 0x01,
  Resolution = 0x02,
  FrameData = 0x03,
  InputEvent = 0x04,
}

const enum InputKind {
  MouseMove = 0,
  MouseDown = 1,
  MouseUp = 2,
  Scroll = 3,
  KeyDown = 4,
  KeyUp = 5,
}

export interface Hello {
  type: "hello";
  version: number;
  role: Role;
  capabilities: number;
}

export interface Resolution {
  type: "resolution";
  width: number;
  height: number;
}

export interface FrameData {
  type: "frame";
  codec: Codec;
  seq: number;
  /** Host-clock capture time in microseconds. Diagnostics only. */
  timestampMicros: number;
  payload: Uint8Array;
}

export type InputEvent =
  | { type: "input"; kind: "mousemove"; x: number; y: number }
  | { type: "input"; kind: "mousedown"; button: number; x: number; y: number }
  | { type: "input"; kind: "mouseup"; button: number; x: number; y: number }
  | { type: "input"; kind: "scroll"; dx: number; dy: number }
  | { type: "input"; kind: "keydown"; code: string; modifiers: number }
  | { type: "input"; kind: "keyup"; code: string; modifiers: number };

export type Message = Hello | Resolution | FrameData | InputEvent;

export type DecodeResult =
  | { ok: true; message: Message }
  | { ok: false; reason: "unknown-type"; msgType: number }
  | { ok: false; reason: "corrupt"; detail: string };

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

function finish(body: (view: DataView, bytes: Uint8Array) => number, capacity: number): Uint8Array {
  const bytes = new Uint8Array(4 + capacity);
  const view = new DataView(bytes.buffer);
  const used = body(view, bytes);
  view.setUint32(0, used, true);
  return bytes.subarray(0, 4 + used);
}

export function encodeHello(h: Omit<Hello, "type">): Uint8Array {
  return finish((view) => {
    view.setUint8(4, MsgType.Hello);
    view.setUint16(5, h.version, true);
    view.setUint8(7, h.role);
    view.setUint8(8, h.capabilities);
    return 5;
  }, 5);
}

export function encodeResolution(r: Omit<Resolution, "type">): Uint8Array {
  return finish((view) => {
    view.setUint8(4, MsgType.Resolution);
    view.setUint32(5, r.width, true);
    view.setUint32(9, r.height, true);
    return 9;
  }, 9);
}

export function encodeFrameData(f: Omit<FrameData, "type">): Uint8Array {
  return finish(
    (view, bytes) => {
      view.setUint8(4, MsgType.FrameData);
      view.setUint8(5, f.codec);
      view.setUint32(6, f.seq, true);
      setUint64(view, 10, f.timestampMicros);
      bytes.set(f.payload, 18);
      return 14 + f.payload.length;
    },
    14 + f.payload.length,
  );
}

export function encodeInputEvent(ev: InputEvent): Uint8Array {
  switch (ev.kind) {
    case "mousemove":
      return finish((view) => {
        view.setUint8(4, MsgType.InputEvent);
        view.setUint8(5, InputKind.MouseMove);
        view.setUint16(6, ev.x, true);
        view.setUint16(8, ev.y, true);
        return 6;
      }, 6);
    case "mousedown":
    case "mouseup":
      return finish((view) => {
        view.setUint8(4, MsgType.InputEvent);
        view.setUint8(5, ev.kind === "mousedown" ? InputKind.MouseDown : InputKind.MouseUp);
        view.setUint8(6, ev.button);
        view.setUint16(7, ev.x, true);
        view.setUint16(9, ev.y, true);
        return 7;
      }, 7);
    case "scroll":
      return finish((view) => {
        view.setUint8(4, MsgType.InputEvent);
        view.setUint8(5, InputKind.Scroll);
        view.setInt16(6, ev.dx, true);
        view.setInt16(8, ev.dy, true);
        return 6;
      }, 6);
    case "keydown":
    case "keyup": {
      const code = textEncoder.encode(ev.code);
      if (code.length > MAX_KEY_CODE_LEN) {
        throw new Error(`key code too long: ${ev.code}`);
      }
      return finish(
        (view, bytes) => {
          view.setUint8(4, MsgType.InputEvent);
          view.setUint8(5, ev.kind === "keydown" ? InputKind.KeyDown : InputKind.KeyUp);
          view.setUint8(6, ev.modifiers);
          view.setUint8(7, code.length);
          bytes.set(code, 8);
          return 4 + code.length;
        },
        4 + code.length,
      );
    }
  }
}

export function encodeMessage(m: Message): Uint8Array {
  switch (m.type) {
    case "hello":
      return encodeHello(m);
    case "resolution":
      return encodeResolution(m);
    case "frame":
      return encodeFrameData(m);
    case "input":
      return encodeInputEvent(m);
  }
}

/** Decode one complete message (WebSocket: one per binary frame). */
export function decodeMessage(data: ArrayBuffer | Uint8Array): DecodeResult {
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (bytes.length < 5) {
    return corrupt("shorter than header");
  }
  const totalLen = view.getUint32(0, true);
  if (totalLen > MAX_MESSAGE_LEN) {
    return corrupt(`declared length ${totalLen} too large`);
  }
  if (4 + totalLen !== bytes.length) {
    return corrupt(`declared length ${totalLen} != actual ${bytes.length - 4}`);
  }
  const msgType = view.getUint8(4);
  const payloadLen = totalLen - 1;
  switch (msgType) {
    case MsgType.Hello: {
      if (payloadLen !== 4) return corrupt("bad Hello length");
      const role = view.getUint8(7);
      if (role > 1) return corrupt("bad role");
      return ok({
        type: "hello",
        version: view.getUint16(5, true),
        role,
        capabilities: view.getUint8(8),
      });
    }
    case MsgType.Resolution: {
      if (payloadLen !== 8) return corrupt("bad Resolution length");
      return ok({
        type: "resolution",
        width: view.getUint32(5, true),
        height: view.getUint32(9, true),
      });
    }
    case MsgType.FrameData: {
      if (payloadLen < 13) return corrupt("bad FrameData length");
      const codec = view.getUint8(5);
      if (codec > 1) return corrupt("bad codec");
      return ok({
        type: "frame",
        codec,
        seq: view.getUint32(6, true),
        timestampMicros: getUint64(view, 10),
        payload: bytes.subarray(18),
      });
    }
    case MsgType.InputEvent:
      return decodeInputEvent(view, bytes, payloadLen);
    default:
      return { ok: false, reason: "unknown-type", msgType };
  }
}

function decodeInputEvent(view: DataView, bytes: Uint8Array, payloadLen: number): DecodeResult {
  if (payloadLen < 1) return corrupt("empty InputEvent");
  const kind = view.getUint8(5);
  const bodyLen = payloadLen - 1;
  switch (kind) {
    case InputKind.MouseMove:
      if (bodyLen !== 4) return corrupt("bad MouseMove length");
      return ok({
        type: "input",
        kind: "mousemove",
        x: view.getUint16(6, true),
        y: view.getUint16(8, true),
      });
    case InputKind.MouseDown:
    case InputKind.MouseUp: {
      if (bodyLen !== 5) return corrupt("bad MouseDown/Up length");
      const button = view.getUint8(6);
      if (button > 2) return corrupt("bad mouse button");
      return ok({
        type: "input",
        kind: kind === InputKind.MouseDown ? "mousedown" : "mouseup",
        button,
        x: view.getUint16(7, true),
        y: view.getUint16(9, true),
      });
    }
    case InputKind.Scroll:
      if (bodyLen !== 4) return corrupt("bad Scroll length");
      return ok({
        type: "input",
        kind: "scroll",
        dx: view.getInt16(6, true),
        dy: view.getInt16(8, true),
      });
    case InputKind.KeyDown:
    case InputKind.KeyUp: {
      if (bodyLen < 2) return corrupt("bad key event length");
      const modifiers = view.getUint8(6);
      const codeLen = view.getUint8(7);
      if (codeLen > MAX_KEY_CODE_LEN || bodyLen !== 2 + codeLen) {
        return corrupt("bad key code length");
      }
      let code: string;
      try {
        code = textDecoder.decode(bytes.subarray(8, 8 + codeLen));
      } catch {
        return corrupt("key code not UTF-8");
      }
      return ok({
        type: "input",
        kind: kind === InputKind.KeyDown ? "keydown" : "keyup",
        code,
        modifiers,
      });
    }
    default:
      return corrupt("bad input kind");
  }
}

// JS numbers are exact up to 2^53; epoch-microsecond timestamps stay well
// under that, so u64 is split into two u32 reads instead of using BigInt.
function getUint64(view: DataView, offset: number): number {
  const lo = view.getUint32(offset, true);
  const hi = view.getUint32(offset + 4, true);
  return hi * 0x1_0000_0000 + lo;
}

function setUint64(view: DataView, offset: number, value: number): void {
  view.setUint32(offset, value >>> 0, true);
  view.setUint32(offset + 4, Math.floor(value / 0x1_0000_0000), true);
}

function ok(message: Message): DecodeResult {
  return { ok: true, message };
}

function corrupt(detail: string): DecodeResult {
  return { ok: false, reason: "corrupt", detail };
}
