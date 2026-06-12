import { describe, expect, it } from "vitest";
import { codecStringFromSps, extractSps, isKeyframe, nalTypes } from "./decode";

const SC = [0, 0, 0, 1];
// Realistic NAL headers: SPS (0x67), PPS (0x68), IDR (0x65), non-IDR (0x41)
const SPS = [0x67, 0x4d, 0x40, 0x1f, 0xaa, 0xbb];
const PPS = [0x68, 0xee, 0x3c, 0x80];
const IDR = [0x65, 0x88, 0x84, 0x00];
const P = [0x41, 0x9a, 0x02];

const keyframeAu = new Uint8Array([...SC, ...SPS, ...SC, ...PPS, ...SC, ...IDR]);
const deltaAu = new Uint8Array([...SC, ...P]);

describe("annex b parsing", () => {
  it("extracts nal types", () => {
    expect(nalTypes(keyframeAu)).toEqual([7, 8, 5]);
    expect(nalTypes(deltaAu)).toEqual([1]);
  });

  it("detects keyframes", () => {
    expect(isKeyframe(keyframeAu)).toBe(true);
    expect(isKeyframe(deltaAu)).toBe(false);
  });

  it("handles 3-byte start codes too", () => {
    const threeByte = new Uint8Array([0, 0, 1, ...IDR]);
    expect(isKeyframe(threeByte)).toBe(true);
  });

  it("extracts the SPS payload exactly", () => {
    const sps = extractSps(keyframeAu);
    expect(sps).not.toBeNull();
    expect(Array.from(sps!)).toEqual(SPS);
    expect(extractSps(deltaAu)).toBeNull();
  });

  it("derives the RFC 6381 codec string from the SPS", () => {
    expect(codecStringFromSps(new Uint8Array(SPS))).toBe("avc1.4D401F");
    expect(codecStringFromSps(new Uint8Array([0x67, 0x42, 0xe0, 0x1e]))).toBe("avc1.42E01E");
  });
});
