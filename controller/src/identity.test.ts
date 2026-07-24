import { beforeEach, describe, expect, it, vi } from "vitest";

import { loadOrCreateIdentity, registerPayload, toHex } from "./identity";
import { signalUrl } from "./signaling";

describe("registerPayload", () => {
  // Pinned against register_payload_cross_implementation_vector in
  // crates/tether-signal/src/identity.rs. Change both or neither.
  it("matches the Rust byte vector", () => {
    expect(toHex(registerPayload("abc", "mac"))).toBe(
      "7465746865722d7369676e616c2d72656769737465722d763100616263006d6163",
    );
  });

  it("keeps the nonce/device boundary unambiguous", () => {
    expect(toHex(registerPayload("a", "bc"))).not.toBe(toHex(registerPayload("ab", "c")));
  });

  it("encodes non-ASCII as UTF-8, matching Rust's str::as_bytes", () => {
    // "café" is 63 61 66 c3 a9 — the é must be two bytes, not one.
    expect(toHex(registerPayload("n", "café")).endsWith("636166c3a9")).toBe(true);
  });
});

describe("loadOrCreateIdentity", () => {
  // vitest runs in the node environment, which has no localStorage. A minimal
  // in-memory stand-in keeps the dependency footprint at zero.
  beforeEach(() => {
    const store = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
      clear: () => store.clear(),
    });
  });

  it("produces a 32-byte key and a 64-byte signature", async () => {
    const id = await loadOrCreateIdentity();
    expect(id).not.toBeNull();
    expect(id!.pubkey).toMatch(/^[0-9a-f]{64}$/);
    expect(await id!.sign("nonce", "device")).toMatch(/^[0-9a-f]{128}$/);
  });

  it("is stable across reloads, or the server's pin would reject us", async () => {
    const first = await loadOrCreateIdentity();
    const second = await loadOrCreateIdentity();
    expect(second!.pubkey).toBe(first!.pubkey);
  });

  it("signs differently per nonce and per device id", async () => {
    const id = (await loadOrCreateIdentity())!;
    const base = await id.sign("n1", "d1");
    expect(await id.sign("n2", "d1")).not.toBe(base);
    expect(await id.sign("n1", "d2")).not.toBe(base);
    // Deterministic for the same inputs (Ed25519 is not randomised).
    expect(await id.sign("n1", "d1")).toBe(base);
  });

  it("verifies under WebCrypto with the exported public key", async () => {
    // lib.dom types BufferSource as ArrayBuffer-backed; Uint8Array is
    // ArrayBufferLike. The cast is the same one identity.ts makes.
    const buf = (b: Uint8Array) => b as unknown as BufferSource;
    const unhex = (s: string) => Uint8Array.from(s.match(/../g)!.map((b) => parseInt(b, 16)));

    const id = (await loadOrCreateIdentity())!;
    const sig = await id.sign("n", "mac");
    const pub = await crypto.subtle.importKey("raw", buf(unhex(id.pubkey)), "Ed25519", true, [
      "verify",
    ]);
    const ok = await crypto.subtle.verify(
      "Ed25519",
      pub,
      buf(unhex(sig)),
      buf(registerPayload("n", "mac")),
    );
    expect(ok).toBe(true);
  });

  it("mints a fresh key rather than throwing on a corrupt stored one", async () => {
    localStorage.setItem("tether-identity-v1", '{"priv":"!!!","pub":"!!!"}');
    const id = await loadOrCreateIdentity();
    expect(id).not.toBeNull();
    expect(id!.pubkey).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe("signalUrl", () => {
  it("mirrors the page scheme for a bare host:port", () => {
    // The load-bearing case: an https:// page cannot open a ws:// socket at
    // all, so guessing ws:// here breaks the deployed controller outright.
    expect(signalUrl("relay.example.com:7879", "https:")).toBe("wss://relay.example.com:7879/ws");
    expect(signalUrl("192.168.1.5:7879", "http:")).toBe("ws://192.168.1.5:7879/ws");
  });

  it("passes an explicit websocket URL through untouched", () => {
    expect(signalUrl("wss://relay.example.com/ws", "http:")).toBe("wss://relay.example.com/ws");
    expect(signalUrl("ws://10.0.0.2:7879/ws", "https:")).toBe("ws://10.0.0.2:7879/ws");
  });

  it("accepts a pasted http(s) URL", () => {
    expect(signalUrl("https://relay.example.com/ws", "http:")).toBe("wss://relay.example.com/ws");
    expect(signalUrl("http://10.0.0.2:7879/ws", "https:")).toBe("ws://10.0.0.2:7879/ws");
  });

  it("does not append /ws when a path is already given", () => {
    expect(signalUrl("relay.example.com/signal", "https:")).toBe("wss://relay.example.com/signal");
  });

  it("trims surrounding whitespace from a pasted value", () => {
    expect(signalUrl("  relay.example.com:7879  ", "https:")).toBe(
      "wss://relay.example.com:7879/ws",
    );
  });
});
