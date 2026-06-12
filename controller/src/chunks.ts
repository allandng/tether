// Frame chunking for the lossy WebRTC media channel.
//
// Browsers cap data-channel message sizes (64 KiB is the universally safe
// bound), and frames ride an unordered, no-retransmit channel so they can't
// head-of-line-block input. Each chunk carries an 8-byte LE header:
//
//   [ u32 frame_seq ][ u16 chunk_idx ][ u16 chunk_count ] [ slice bytes... ]
//
// over slices of the *complete tether wire message* (the FrameData encoding,
// untouched). The reassembler is latest-wins: a chunk from a newer frame
// discards any partial older frame, and a lost chunk simply drops that frame
// — the tether protocol already tolerates seq gaps. The Rust side mirrors
// this format in tetherd's webrtc module.

export const CHUNK_PAYLOAD = 64 * 1024 - 8;
const HEADER = 8;

export function chunkFrame(frameSeq: number, wire: Uint8Array): Uint8Array[] {
  const count = Math.max(1, Math.ceil(wire.length / CHUNK_PAYLOAD));
  const chunks: Uint8Array[] = [];
  for (let idx = 0; idx < count; idx++) {
    const slice = wire.subarray(idx * CHUNK_PAYLOAD, (idx + 1) * CHUNK_PAYLOAD);
    const chunk = new Uint8Array(HEADER + slice.length);
    const view = new DataView(chunk.buffer);
    view.setUint32(0, frameSeq, true);
    view.setUint16(4, idx, true);
    view.setUint16(6, count, true);
    chunk.set(slice, HEADER);
    chunks.push(chunk);
  }
  return chunks;
}

export class FrameReassembler {
  private seq = -1;
  private count = 0;
  private received = 0;
  private parts: (Uint8Array | null)[] = [];

  /** Returns the complete wire message when the last piece arrives. */
  onChunk(data: ArrayBuffer | Uint8Array): Uint8Array | null {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
    if (bytes.length < HEADER) return null;
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const seq = view.getUint32(0, true);
    const idx = view.getUint16(4, true);
    const count = view.getUint16(6, true);
    if (count === 0 || idx >= count) return null;

    // Unordered channel: chunks of an older frame may trail a newer one.
    // Latest wins; anything older than the frame in progress is noise.
    if (seq !== this.seq) {
      if (this.seq !== -1 && seqOlder(seq, this.seq)) return null;
      this.seq = seq;
      this.count = count;
      this.received = 0;
      this.parts = new Array(count).fill(null);
    }
    if (count !== this.count || this.parts[idx] !== null) return null; // corrupt/duplicate
    this.parts[idx] = bytes.subarray(HEADER);
    this.received++;
    if (this.received < this.count) return null;

    const total = this.parts.reduce((n, p) => n + (p?.length ?? 0), 0);
    const wire = new Uint8Array(total);
    let offset = 0;
    for (const part of this.parts) {
      wire.set(part!, offset);
      offset += part!.length;
    }
    this.seq = -1; // consumed
    return wire;
  }
}

// u32 sequence comparison tolerating wraparound.
function seqOlder(a: number, b: number): boolean {
  return ((b - a) >>> 0) < 0x8000_0000;
}
