// H.264 decode via WebCodecs. The host sends Annex B access units (start
// codes, SPS/PPS in-band on keyframes), which is WebCodecs' format when the
// decoder is configured without a `description`. The codec string is derived
// from the stream's own SPS so the profile always matches.

/** NAL unit types present in an Annex B access unit. */
export function nalTypes(annexB: Uint8Array): number[] {
  const types: number[] = [];
  for (let i = 0; i + 3 < annexB.length; i++) {
    const four = annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 0 && annexB[i + 3] === 1;
    const three = annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 1;
    if (four || three) {
      const header = annexB[i + (four ? 4 : 3)];
      if (header !== undefined) types.push(header & 0x1f);
      i += four ? 3 : 2;
    }
  }
  return types;
}

export function isKeyframe(annexB: Uint8Array): boolean {
  return nalTypes(annexB).includes(5); // IDR
}

/** First SPS payload (NAL type 7) in the access unit, header byte included. */
export function extractSps(annexB: Uint8Array): Uint8Array | null {
  for (let i = 0; i + 4 < annexB.length; i++) {
    const four = annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 0 && annexB[i + 3] === 1;
    const three = annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 1;
    if (!four && !three) continue;
    const start = i + (four ? 4 : 3);
    if (((annexB[start] ?? 0) & 0x1f) !== 7) {
      i = start - 1;
      continue;
    }
    // SPS runs to the next start code (or end)
    let end = annexB.length;
    for (let j = start + 1; j + 2 < annexB.length; j++) {
      if (annexB[j] === 0 && annexB[j + 1] === 0 && (annexB[j + 2] === 1 || (annexB[j + 2] === 0 && annexB[j + 3] === 1))) {
        end = j;
        break;
      }
    }
    return annexB.subarray(start, end);
  }
  return null;
}

/** RFC 6381 codec string from the SPS: avc1.PPCCLL. */
export function codecStringFromSps(sps: Uint8Array): string {
  const hex = (b: number) => b.toString(16).padStart(2, "0").toUpperCase();
  return `avc1.${hex(sps[1] ?? 0)}${hex(sps[2] ?? 0)}${hex(sps[3] ?? 0)}`;
}

export function webCodecsSupported(): boolean {
  return typeof VideoDecoder !== "undefined";
}

/**
 * Streaming decoder: feed every arriving access unit in order; rendered
 * frames come out through `onFrame`. Recovers from errors by waiting for the
 * next keyframe and reconfiguring.
 */
export class H264Stream {
  private decoder: VideoDecoder | null = null;
  private awaitingKeyframe = true;
  private timestamp = 0;
  /** decoder timestamp → caller's capture timestamp, resolved on output. */
  private readonly captureTs = new Map<number, number>();

  constructor(
    private readonly onFrame: (frame: VideoFrame, captureMicros: number) => void,
    private readonly onFatal: (detail: string) => void,
  ) {}

  push(annexB: Uint8Array, captureMicros = 0): void {
    const key = isKeyframe(annexB);
    // Bounded latency: if decode can't keep up with arrival (e.g. software
    // fallback), drop the backlog and resync at the next keyframe instead of
    // drifting seconds behind live.
    if (this.decoder && this.decoder.state === "configured" && this.decoder.decodeQueueSize > 8) {
      console.warn(`decode backlog (${this.decoder.decodeQueueSize}), resyncing at next keyframe`);
      this.reset();
    }
    if (this.awaitingKeyframe) {
      if (!key) return; // can't start mid-GOP
      const sps = extractSps(annexB);
      if (!sps) return;
      this.reset();
      const decoder = new VideoDecoder({
        output: (frame) => {
          const captured = this.captureTs.get(frame.timestamp) ?? 0;
          this.captureTs.delete(frame.timestamp);
          this.onFrame(frame, captured);
        },
        error: (e) => {
          // corrupt stream or unsupported config: resync at next keyframe
          console.warn("VideoDecoder error:", e);
          this.awaitingKeyframe = true;
        },
      });
      // hardwareAcceleration is left at no-preference: "prefer-hardware"
      // hard-fails configure() in software-only environments, and the
      // backlog resync above already bounds latency when decode is slow.
      const config: VideoDecoderConfig = {
        codec: codecStringFromSps(sps),
        optimizeForLatency: true,
      };
      decoder.configure(config);
      this.decoder = decoder;
      this.awaitingKeyframe = false;
    }
    if (!this.decoder || this.decoder.state === "closed") {
      this.awaitingKeyframe = true;
      return;
    }
    try {
      this.captureTs.set(this.timestamp, captureMicros);
      if (this.captureTs.size > 64) {
        // frames dropped without output (resets); don't leak the map
        const oldest = this.captureTs.keys().next().value;
        if (oldest !== undefined) this.captureTs.delete(oldest);
      }
      this.decoder.decode(
        new EncodedVideoChunk({
          type: key ? "key" : "delta",
          timestamp: this.timestamp,
          data: annexB as BufferSource,
        }),
      );
      this.timestamp += 33_333; // synthetic 30fps clock; only ordering matters
    } catch (e) {
      this.onFatal(`decode failed: ${e}`);
      this.awaitingKeyframe = true;
    }
  }

  reset(): void {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        // already closing
      }
    }
    this.decoder = null;
    this.awaitingKeyframe = true;
  }
}
