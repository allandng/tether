import { describe, expect, it } from "vitest";

describe("scaffold", () => {
  it("agrees with the Rust side on the protocol version", async () => {
    // Real shared-constant checks arrive with the protocol port in Module 4.
    const { PROTOCOL_VERSION } = await import("./main");
    expect(PROTOCOL_VERSION).toBe(1);
  });
});
