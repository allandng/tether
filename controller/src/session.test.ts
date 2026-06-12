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
import { ProtocolSession, type SessionEvents } from "./session";

function makeSession() {
  const sent: Uint8Array[] = [];
  const events = {
    onConnected: vi.fn(),
    onResolution: vi.fn(),
    onFrame: vi.fn(),
    onProtocolError: vi.fn(),
  } satisfies SessionEvents;
  const session = new ProtocolSession(events, (b) => sent.push(b));
  return { session, events, sent };
}

const hostHello = () =>
  encodeHello({ version: PROTOCOL_VERSION, role: Role.Host, capabilities: CAP_CAN_HOST });

describe("ProtocolSession", () => {
  it("sends our Hello on start and connects on a valid host Hello", () => {
    const { session, events, sent } = makeSession();
    session.start();
    expect(sent).toHaveLength(1);
    const decoded = decodeMessage(sent[0]!);
    expect(decoded).toMatchObject({ ok: true, message: { type: "hello", role: Role.Controller } });

    expect(session.connected).toBe(false);
    session.onMessage(hostHello());
    expect(session.connected).toBe(true);
    expect(events.onConnected).toHaveBeenCalledOnce();
  });

  it("rejects a wrong-version host", () => {
    const { session, events } = makeSession();
    session.start();
    session.onMessage(
      encodeHello({ version: 99, role: Role.Host, capabilities: CAP_CAN_HOST }),
    );
    expect(events.onProtocolError).toHaveBeenCalled();
    expect(session.connected).toBe(false);
  });

  it("rejects a non-Hello first message", () => {
    const { session, events } = makeSession();
    session.start();
    session.onMessage(encodeResolution({ width: 1, height: 1 }));
    expect(events.onProtocolError).toHaveBeenCalled();
  });

  it("dispatches resolution and frames after handshake, and gates input on it", () => {
    const { session, events, sent } = makeSession();
    session.start();
    session.sendInput({ type: "input", kind: "mousemove", x: 1, y: 2 });
    expect(sent).toHaveLength(1); // input before handshake is dropped

    session.onMessage(hostHello());
    session.onMessage(encodeResolution({ width: 640, height: 400 }));
    expect(events.onResolution).toHaveBeenCalledWith(
      expect.objectContaining({ width: 640, height: 400 }),
    );
    session.onMessage(
      encodeFrameData({
        codec: Codec.Jpeg,
        seq: 9,
        timestampMicros: 5,
        payload: new Uint8Array([1, 2, 3]),
      }),
    );
    expect(events.onFrame).toHaveBeenCalledWith(expect.objectContaining({ seq: 9 }));

    session.sendInput({ type: "input", kind: "mousemove", x: 1, y: 2 });
    expect(sent).toHaveLength(2);
  });
});
