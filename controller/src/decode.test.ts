import { describe, expect, it } from "vitest";
import { H264Stream, codecStringFromSps, extractSps, isKeyframe, nalTypes } from "./decode";

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

describe("H264Stream resync", () => {
  // Just enough WebCodecs to see what reaches the decoder.
  const decoded: string[] = [];
  let configured = 0;
  class FakeDecoder {
    state = "unconfigured";
    decodeQueueSize = 0;
    configure() {
      this.state = "configured";
      configured++;
    }
    decode(chunk: { type: string }) {
      decoded.push(chunk.type);
    }
    close() {
      this.state = "closed";
    }
  }
  class FakeChunk {
    type: string;
    constructor(init: { type: string }) {
      this.type = init.type;
    }
  }

  const withFakes = (run: (s: H264Stream) => void) => {
    const g = globalThis as Record<string, unknown>;
    const saved = { VideoDecoder: g.VideoDecoder, EncodedVideoChunk: g.EncodedVideoChunk };
    g.VideoDecoder = FakeDecoder;
    g.EncodedVideoChunk = FakeChunk;
    decoded.length = 0;
    configured = 0;
    try {
      run(new H264Stream(() => {}, () => {}));
    } finally {
      g.VideoDecoder = saved.VideoDecoder;
      g.EncodedVideoChunk = saved.EncodedVideoChunk;
    }
  };

  it("waits for a keyframe after a gap in the host's frame sequence", () => {
    withFakes((s) => {
      s.push(keyframeAu, 0, 10);
      s.push(deltaAu, 0, 11);
      s.push(deltaAu, 0, 13); // 12 never arrived, and 13 references it
      s.push(deltaAu, 0, 14);
      s.push(keyframeAu, 0, 15);
      s.push(deltaAu, 0, 16);
      expect(decoded).toEqual(["key", "delta", "key", "delta"]);
    });
  });

  it("accepts the sequence wrapping around", () => {
    withFakes((s) => {
      s.push(keyframeAu, 0, 0xffffffff);
      s.push(deltaAu, 0, 0);
      expect(decoded).toEqual(["key", "delta"]);
    });
  });

  it("restarts at a keyframe for a new connection", () => {
    withFakes((s) => {
      s.push(keyframeAu, 0, 100);
      s.push(deltaAu, 0, 101);
      s.restart();
      // the new session joins the host's stream mid-GOP, numbered its own way
      s.push(deltaAu, 0, 5000);
      s.push(deltaAu, 0, 5001);
      s.push(keyframeAu, 0, 5002);
      s.push(deltaAu, 0, 5003);
      expect(decoded).toEqual(["key", "delta", "key", "delta"]);
      expect(configured).toBe(2);
    });
  });
});
