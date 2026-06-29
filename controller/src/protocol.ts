// Tether wire protocol v1 — TypeScript mirror of crates/tether-protocol.
// Must stay byte-identical to the Rust implementation; docs/protocol.md is
// the spec, and protocol.test.ts pins cross-implementation test vectors.

export const PROTOCOL_VERSION = 1;
export const MAX_MESSAGE_LEN = 64 * 1024 * 1024;
export const MAX_KEY_CODE_LEN = 32;
export const MAX_CLIPBOARD_LEN = 256 * 1024;
export const MAX_TEXT_INPUT_LEN = 1024;

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
  ClipboardData = 0x05,
  TextInput = 0x06,
  PairRequest = 0x07,
  PairResult = 0x08,
  Auth = 0x09,
  AuthResult = 0x0a,
  Displays = 0x0b,
  SelectDisplay = 0x0c,
}

export const MAX_AUTH_FIELD_LEN = 512;

const enum ClipboardKind {
  Text = 0,
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

export interface ClipboardData {
  type: "clipboard";
  text: string;
}

export interface TextInput {
  type: "text";
  text: string;
}

export interface PairRequest {
  type: "pair_request";
  deviceId: string;
  name: string;
  proof: Uint8Array;
}

export interface PairResult {
  type: "pair_result";
  ok: boolean;
  token: string;
}

export interface Auth {
  type: "auth";
  deviceId: string;
  token: string;
}

export interface AuthResult {
  type: "auth_result";
  ok: boolean;
}

export interface DisplayInfo {
  id: number;
  width: number;
  height: number;
  active: boolean;
  name: string;
}

export interface Displays {
  type: "displays";
  displays: DisplayInfo[];
}

export interface SelectDisplay {
  type: "select_display";
  id: number;
}

export type Message =
  | Hello
  | Resolution
  | FrameData
  | InputEvent
  | ClipboardData
  | TextInput
  | PairRequest
  | PairResult
  | Auth
  | AuthResult
  | Displays
  | SelectDisplay;

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

export function encodeClipboardData(c: Omit<ClipboardData, "type">): Uint8Array {
  const text = textEncoder.encode(c.text);
  if (text.length > MAX_CLIPBOARD_LEN) {
    throw new Error(`clipboard too large: ${text.length} bytes`);
  }
  return finish(
    (view, bytes) => {
      view.setUint8(4, MsgType.ClipboardData);
      view.setUint8(5, ClipboardKind.Text);
      bytes.set(text, 6);
      return 2 + text.length;
    },
    2 + text.length,
  );
}

export function encodeTextInput(t: Omit<TextInput, "type">): Uint8Array {
  const text = textEncoder.encode(t.text);
  if (text.length > MAX_TEXT_INPUT_LEN) {
    throw new Error(`text input too large: ${text.length} bytes`);
  }
  return finish(
    (view, bytes) => {
      view.setUint8(4, MsgType.TextInput);
      bytes.set(text, 5);
      return 1 + text.length;
    },
    1 + text.length,
  );
}

/** Append a u16-length-prefixed field; returns bytes written. */
function fieldLen(...parts: Uint8Array[]): number {
  return parts.reduce((n, p) => n + 2 + p.length, 0);
}
function writeField(view: DataView, bytes: Uint8Array, at: number, field: Uint8Array): number {
  view.setUint16(at, field.length, true);
  bytes.set(field, at + 2);
  return at + 2 + field.length;
}

export function encodePairRequest(p: Omit<PairRequest, "type">): Uint8Array {
  const id = textEncoder.encode(p.deviceId);
  const name = textEncoder.encode(p.name);
  const cap = fieldLen(id, name, p.proof);
  return finish((view, bytes) => {
    view.setUint8(4, MsgType.PairRequest);
    let at = 5;
    at = writeField(view, bytes, at, id);
    at = writeField(view, bytes, at, name);
    at = writeField(view, bytes, at, p.proof);
    return at - 4;
  }, 1 + cap);
}

export function encodeAuth(a: Omit<Auth, "type">): Uint8Array {
  const id = textEncoder.encode(a.deviceId);
  const token = textEncoder.encode(a.token);
  const cap = fieldLen(id, token);
  return finish((view, bytes) => {
    view.setUint8(4, MsgType.Auth);
    let at = 5;
    at = writeField(view, bytes, at, id);
    at = writeField(view, bytes, at, token);
    return at - 4;
  }, 1 + cap);
}

export function encodeSelectDisplay(s: Omit<SelectDisplay, "type">): Uint8Array {
  return finish((view) => {
    view.setUint8(4, MsgType.SelectDisplay);
    view.setUint32(5, s.id, true);
    return 5;
  }, 5);
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
    case "clipboard":
      return encodeClipboardData(m);
    case "text":
      return encodeTextInput(m);
    case "pair_request":
      return encodePairRequest(m);
    case "auth":
      return encodeAuth(m);
    case "select_display":
      return encodeSelectDisplay(m);
    case "pair_result":
    case "auth_result":
    case "displays":
      // host→controller only; the controller never sends these
      throw new Error(`controller does not send ${m.type}`);
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
    case MsgType.ClipboardData: {
      if (payloadLen < 1) return corrupt("empty ClipboardData");
      if (view.getUint8(5) !== ClipboardKind.Text) return corrupt("bad clipboard kind");
      if (payloadLen - 1 > MAX_CLIPBOARD_LEN) return corrupt("clipboard too large");
      let text: string;
      try {
        text = textDecoder.decode(bytes.subarray(6));
      } catch {
        return corrupt("clipboard not UTF-8");
      }
      return ok({ type: "clipboard", text });
    }
    case MsgType.TextInput: {
      if (payloadLen > MAX_TEXT_INPUT_LEN) return corrupt("text input too large");
      let text: string;
      try {
        text = textDecoder.decode(bytes.subarray(5));
      } catch {
        return corrupt("text input not UTF-8");
      }
      return ok({ type: "text", text });
    }
    case MsgType.PairResult: {
      if (payloadLen < 1) return corrupt("empty PairResult");
      const okFlag = view.getUint8(5) !== 0;
      const r = readField(bytes, view, 6);
      if (!r) return corrupt("bad PairResult token");
      let token: string;
      try {
        token = textDecoder.decode(r.field);
      } catch {
        return corrupt("PairResult token not UTF-8");
      }
      return ok({ type: "pair_result", ok: okFlag, token });
    }
    case MsgType.AuthResult: {
      if (payloadLen !== 1) return corrupt("bad AuthResult length");
      return ok({ type: "auth_result", ok: view.getUint8(5) !== 0 });
    }
    case MsgType.Displays: {
      if (payloadLen < 1) return corrupt("empty Displays");
      const count = view.getUint8(5);
      const displays: DisplayInfo[] = [];
      let at = 6;
      for (let i = 0; i < count; i++) {
        if (at + 13 > bytes.length) return corrupt("truncated Displays entry");
        const id = view.getUint32(at, true);
        const width = view.getUint32(at + 4, true);
        const height = view.getUint32(at + 8, true);
        const active = view.getUint8(at + 12) !== 0;
        const r = readField(bytes, view, at + 13);
        if (!r) return corrupt("bad Displays name");
        let name: string;
        try {
          name = textDecoder.decode(r.field);
        } catch {
          return corrupt("Displays name not UTF-8");
        }
        displays.push({ id, width, height, active, name });
        at = r.next;
      }
      return ok({ type: "displays", displays });
    }
    case MsgType.SelectDisplay: {
      if (payloadLen !== 4) return corrupt("bad SelectDisplay length");
      return ok({ type: "select_display", id: view.getUint32(5, true) });
    }
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

/** Read a u16-length-prefixed field at `at`; null if it overruns the buffer. */
function readField(
  bytes: Uint8Array,
  view: DataView,
  at: number,
): { field: Uint8Array; next: number } | null {
  if (at + 2 > bytes.length) return null;
  const len = view.getUint16(at, true);
  if (len > MAX_AUTH_FIELD_LEN || at + 2 + len > bytes.length) return null;
  return { field: bytes.subarray(at + 2, at + 2 + len), next: at + 2 + len };
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
