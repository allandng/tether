import { describe, expect, it, vi } from "vitest";
import {
  CAP_CAN_HOST,
  Codec,
  PROTOCOL_VERSION,
  Role,
  decodeMessage,
  encodeFrameData,
  encodeHello,
  encodeResolution,
} from "./protocol";
import { type AuthContext, ProtocolSession, type SessionEvents } from "./session";

function makeSession(token: string | null = null) {
  const sent: Uint8Array[] = [];
  let stored = token;
  const events = {
    onConnected: vi.fn(),
    onResolution: vi.fn(),
    onFrame: vi.fn(),
    onClipboard: vi.fn(),
    onPairingRequired: vi.fn(),
    onPairingFailed: vi.fn(),
    onProtocolError: vi.fn(),
  } satisfies SessionEvents;
  const auth: AuthContext = {
    deviceId: "dev1",
    deviceName: "Test",
    getToken: () => stored,
    setToken: (t) => {
      stored = t;
    },
    channelBinding: async () => new Uint8Array(32).fill(1),
  };
  const session = new ProtocolSession(events, (b) => sent.push(b), auth);
  return { session, events, sent, tokenOf: () => stored };
}

const hostHello = () =>
  encodeHello({ version: PROTOCOL_VERSION, role: Role.Host, capabilities: CAP_CAN_HOST });

// host→controller messages the controller can't encode itself
const authResult = (ok: boolean) => new Uint8Array([2, 0, 0, 0, 0x0a, ok ? 1 : 0]);
function pairResult(ok: boolean, token: string): Uint8Array {
  const t = new TextEncoder().encode(token);
  const out = new Uint8Array(4 + 1 + 1 + 2 + t.length);
  const view = new DataView(out.buffer);
  view.setUint32(0, out.length - 4, true);
  out[4] = 0x08;
  out[5] = ok ? 1 : 0;
  view.setUint16(6, t.length, true);
  out.set(t, 8);
  return out;
}

// Read the wire msg_type byte directly: decodeMessage only handles
// host→controller messages, not the Auth/PairRequest the controller sends.
const TYPE: Record<number, string> = {
  0x01: "hello",
  0x04: "input",
  0x07: "pair_request",
  0x09: "auth",
};
function lastType(sent: Uint8Array[]): string | undefined {
  const last = sent[sent.length - 1];
  return last ? TYPE[last[4]!] : undefined;
}

describe("ProtocolSession auth flow", () => {
  it("Hello → Auth → AuthResult(ok) → connected", () => {
    const { session, events, sent } = makeSession("mytoken");
    session.start();
    expect(lastType(sent)).toBe("hello");

    session.onMessage(hostHello());
    expect(lastType(sent)).toBe("auth"); // auto-sends Auth after host Hello
    expect(session.connected).toBe(false);

    session.onMessage(authResult(true));
    expect(session.connected).toBe(true);
    expect(events.onConnected).toHaveBeenCalledOnce();
  });

  it("AuthResult(fail) requests pairing; a code pairs and stores the token", async () => {
    const { session, events, sent, tokenOf } = makeSession(null);
    session.start();
    session.onMessage(hostHello());
    session.onMessage(authResult(false));
    expect(events.onPairingRequired).toHaveBeenCalledOnce();
    expect(session.connected).toBe(false);

    await session.submitPairingCode("ABCD1234");
    expect(lastType(sent)).toBe("pair_request");

    session.onMessage(pairResult(true, "issued-token"));
    expect(session.connected).toBe(true);
    expect(tokenOf()).toBe("issued-token");
    expect(events.onConnected).toHaveBeenCalledOnce();
  });

  it("PairResult(fail) reports failure and stays unconnected", async () => {
    const { session, events } = makeSession(null);
    session.start();
    session.onMessage(hostHello());
    session.onMessage(authResult(false));
    await session.submitPairingCode("WRONGCOD");
    session.onMessage(pairResult(false, ""));
    expect(events.onPairingFailed).toHaveBeenCalledOnce();
    expect(session.connected).toBe(false);
  });

  it("rejects a non-Hello first message", () => {
    const { session, events } = makeSession();
    session.start();
    session.onMessage(encodeResolution({ width: 1, height: 1 }));
    expect(events.onProtocolError).toHaveBeenCalled();
  });

  it("dispatches frames/resolution only once connected", () => {
    const { session, events } = makeSession("t");
    session.start();
    session.onMessage(hostHello());
    session.onMessage(authResult(true));
    session.onMessage(encodeResolution({ width: 640, height: 400 }));
    expect(events.onResolution).toHaveBeenCalledWith(
      expect.objectContaining({ width: 640, height: 400 }),
    );
    session.onMessage(
      encodeFrameData({ codec: Codec.Jpeg, seq: 9, timestampMicros: 5, payload: new Uint8Array([1]) }),
    );
    expect(events.onFrame).toHaveBeenCalledWith(expect.objectContaining({ seq: 9 }));
  });

  it("gates input send on connection", () => {
    const { session, sent } = makeSession("t");
    session.start();
    session.sendInput({ type: "input", kind: "mousemove", x: 1, y: 2 });
    expect(sent).toHaveLength(1); // only Hello; input dropped pre-connect
    session.onMessage(hostHello());
    session.onMessage(authResult(true));
    session.sendInput({ type: "input", kind: "mousemove", x: 1, y: 2 });
    expect(lastType(sent)).toBe("input");
  });
});
