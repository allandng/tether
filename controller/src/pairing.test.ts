import { describe, expect, it } from "vitest";
import { channelBinding, normalizeFp, pairingProof, sdpFingerprint } from "./pairing";

const hex = (b: Uint8Array) => Array.from(b).map((x) => x.toString(16).padStart(2, "0")).join("");

describe("pairing crypto (cross-pinned with crates/tetherd/src/auth.rs)", () => {
  it("pairing_proof matches the Rust vector", async () => {
    // Rust: pairing_proof("ABCD1234", &[7u8; 32])
    const proof = await pairingProof("ABCD1234", new Uint8Array(32).fill(7));
    expect(hex(proof)).toBe("71e230571177d6680a36d8cdcc68c71eb6d9fa5d77be32ea95b5d8c8ade919b5");
  });

  it("channel_binding matches the Rust vector", async () => {
    // Rust: channel_binding("sha-256 AB:CD", "sha-256 EF:01")
    const cb = await channelBinding("sha-256 AB:CD", "sha-256 EF:01");
    expect(hex(cb)).toBe("dff557b1796e114a2fb04378bc4ae25516f7e4789e2ffa3248286dc366db0885");
  });

  it("channel binding is order-independent", async () => {
    const a = await channelBinding("AA:BB", "cc:dd");
    const b = await channelBinding("cc:dd", "AA:BB");
    expect(hex(a)).toBe(hex(b));
  });

  it("normalizeFp strips ':' and whitespace and uppercases", () => {
    expect(normalizeFp("sha-256 ab:CD:ef")).toBe("SHA-256ABCDEF");
  });

  it("extracts a fingerprint from SDP", () => {
    const sdp = "v=0\r\na=group:BUNDLE 0\r\na=fingerprint:sha-256 AB:CD:EF\r\na=setup:actpass\r\n";
    expect(sdpFingerprint(sdp)).toBe("sha-256 AB:CD:EF");
    expect(sdpFingerprint("v=0\r\n")).toBeNull();
  });
});
