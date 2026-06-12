import { describe, expect, it } from "vitest";
import { CHUNK_PAYLOAD, FrameReassembler, chunkFrame } from "./chunks";

function wire(len: number, fill = 7): Uint8Array {
  const bytes = new Uint8Array(len);
  for (let i = 0; i < len; i++) bytes[i] = (i + fill) % 256;
  return bytes;
}

describe("chunking", () => {
  it("small frame is a single chunk and reassembles byte-identically", () => {
    const original = wire(1000);
    const chunks = chunkFrame(1, original);
    expect(chunks).toHaveLength(1);
    const r = new FrameReassembler();
    expect(r.onChunk(chunks[0]!)).toEqual(original);
  });

  it("large frame splits at the chunk payload size and reassembles", () => {
    const original = wire(CHUNK_PAYLOAD * 2 + 500);
    const chunks = chunkFrame(42, original);
    expect(chunks).toHaveLength(3);
    const r = new FrameReassembler();
    expect(r.onChunk(chunks[0]!)).toBeNull();
    expect(r.onChunk(chunks[1]!)).toBeNull();
    expect(r.onChunk(chunks[2]!)).toEqual(original);
  });

  it("tolerates out-of-order chunks (unordered channel)", () => {
    const original = wire(CHUNK_PAYLOAD * 2 + 1);
    const [a, b, c] = chunkFrame(7, original) as [Uint8Array, Uint8Array, Uint8Array];
    const r = new FrameReassembler();
    expect(r.onChunk(c)).toBeNull();
    expect(r.onChunk(a)).toBeNull();
    expect(r.onChunk(b)).toEqual(original);
  });

  it("a newer frame discards a partial older frame (latest wins)", () => {
    const oldFrame = chunkFrame(1, wire(CHUNK_PAYLOAD + 1, 3));
    const newWire = wire(800, 9);
    const r = new FrameReassembler();
    expect(r.onChunk(oldFrame[0]!)).toBeNull(); // partial old
    expect(r.onChunk(chunkFrame(2, newWire)[0]!)).toEqual(newWire);
    // straggler chunk from the old frame must not produce anything
    expect(r.onChunk(oldFrame[1]!)).toBeNull();
  });

  it("a lost chunk means the frame never completes, and the stream recovers", () => {
    const lossy = chunkFrame(5, wire(CHUNK_PAYLOAD * 2 + 1, 1));
    const next = wire(600, 2);
    const r = new FrameReassembler();
    expect(r.onChunk(lossy[0]!)).toBeNull();
    // lossy[1] lost in transit
    expect(r.onChunk(lossy[2]!)).toBeNull();
    expect(r.onChunk(chunkFrame(6, next)[0]!)).toEqual(next);
  });

  it("handles seq wraparound as newer, not older", () => {
    const r = new FrameReassembler();
    const before = chunkFrame(0xffff_fffe, wire(100, 1));
    const after = chunkFrame(1, wire(100, 2)); // wrapped
    expect(r.onChunk(before[0]!)).toEqual(wire(100, 1));
    expect(r.onChunk(after[0]!)).toEqual(wire(100, 2));
  });

  it("ignores garbage", () => {
    const r = new FrameReassembler();
    expect(r.onChunk(new Uint8Array([1, 2, 3]))).toBeNull(); // shorter than header
    const bad = chunkFrame(1, wire(100))[0]!;
    bad.set([0xff, 0xff], 4); // idx >= count
    expect(r.onChunk(bad)).toBeNull();
  });
});
