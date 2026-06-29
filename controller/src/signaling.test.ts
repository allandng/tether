import { describe, expect, it } from "vitest";
import { type ClientMessage, parseServerMessage } from "./signaling";

// These JSON shapes are pinned against crates/tether-signal (serde
// `tag = "type"`, snake_case). The Rust side carries the same vectors in
// protocol.rs; change both or neither.
describe("cross-implementation JSON vectors", () => {
  it("register serializes to the Rust serde shape", () => {
    const msg: ClientMessage = {
      type: "register",
      device_id: "ipad",
      name: "iPad",
      caps: { can_host: false, can_control: true },
      auth: "s3cret",
    };
    expect(JSON.parse(JSON.stringify(msg))).toEqual({
      type: "register",
      device_id: "ipad",
      name: "iPad",
      caps: { can_host: false, can_control: true },
      auth: "s3cret",
    });
  });

  it("parses server messages in Rust serde shape", () => {
    expect(
      parseServerMessage('{"type":"registered","ice_servers":[{"urls":["stun:s:3478"]}]}'),
    ).toEqual({ type: "registered", ice_servers: [{ urls: ["stun:s:3478"] }] });
    expect(
      parseServerMessage(
        '{"type":"registered","ice_servers":[{"urls":["turn:t:3478"],"username":"1:u","credential":"abc="}]}',
      ),
    ).toMatchObject({ type: "registered", ice_servers: [{ username: "1:u", credential: "abc=" }] });
    expect(
      parseServerMessage('{"type":"answer","from":"mac","sdp":"v=0..."}'),
    ).toEqual({ type: "answer", from: "mac", sdp: "v=0..." });
    expect(
      parseServerMessage('{"type":"error","code":"bad_auth","message":"bad secret"}'),
    ).toEqual({ type: "error", code: "bad_auth", message: "bad secret" });
    expect(
      parseServerMessage(
        '{"type":"peers","peers":[{"device_id":"mac","name":"Mac","caps":{"can_host":true,"can_control":true}}]}',
      ),
    ).toMatchObject({ type: "peers", peers: [{ device_id: "mac" }] });
  });

  it("rejects garbage without throwing", () => {
    expect(parseServerMessage("not json")).toBeNull();
    expect(parseServerMessage("42")).toBeNull();
    expect(parseServerMessage('{"no_type":1}')).toBeNull();
  });
});
