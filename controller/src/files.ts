// File transfer, controller side (Phase 11). Transport-agnostic: the
// transport supplies a sink that carries file messages (WebRTC: the chunked
// tether-bulk channel; LAN: the one WebSocket) and a `drain()` that resolves
// when it can take more data — the backpressure that keeps a large upload
// from ballooning the send buffer.
//
// Uploads stream from the File handle slice by slice, so their size is not
// limited by memory. Downloads either stream to disk (a writer from the File
// System Access API, Chromium) or collect in memory and are saved as a Blob;
// in-memory downloads are capped and refused up front in FileAccept with a
// readable reason, because Safari holds the whole file before it can save it.

import { FILE_CHUNK_LEN, type FileMessage, FileStatus } from "./protocol";

export interface FileSink {
  send(msg: FileMessage): void;
  /** Resolves when the transport has room for more file data. */
  drain(): Promise<void>;
}

/** Where a streamed download goes (FileSystemWritableFileStream-shaped). */
export interface DownloadWriter {
  write(data: Uint8Array): Promise<void>;
  close(): Promise<void>;
  abort(): Promise<void>;
}

export type TransferState = "waiting" | "active" | "complete" | "cancelled" | "error";

export interface Transfer {
  id: number;
  direction: "upload" | "download";
  name: string;
  size: number;
  done: number;
  state: TransferState;
  error?: string;
}

export interface FileOptions {
  /** Largest download collected in memory (no streaming writer). */
  maxMemoryDownload: number;
  /** Hand a completed in-memory download to the user. */
  save(name: string, blob: Blob): void;
}

interface Upload {
  t: Transfer;
  file: Blob;
}

interface Download {
  t: Transfer;
  writer: DownloadWriter | null;
  parts: Uint8Array[];
  nextSeq: number;
  chain: Promise<void>;
}

const STATUS_TEXT: Record<number, string> = {
  [FileStatus.Cancelled]: "cancelled by the host",
  [FileStatus.Error]: "the host reported an error",
};

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** Last path component of a host path, either separator style. */
export function baseName(path: string): string {
  const parts = path.trim().replace(/^"|"$/g, "").split(/[\\/]/);
  return parts[parts.length - 1] || "download";
}

export class FileTransfers {
  private nextId = (Math.random() * 0x1000_0000) >>> 0;
  private readonly uploads = new Map<number, Upload>();
  private readonly downloads = new Map<number, Download>();

  constructor(
    private readonly sink: FileSink,
    private readonly onChange: (t: Transfer) => void,
    private readonly opts: FileOptions,
  ) {}

  private allocId(): number {
    this.nextId = (this.nextId + 1) >>> 0;
    return this.nextId;
  }

  private update(t: Transfer, patch: Partial<Transfer>): void {
    Object.assign(t, patch);
    this.onChange({ ...t });
  }

  upload(file: Blob, name: string): Transfer {
    const t: Transfer = {
      id: this.allocId(),
      direction: "upload",
      name,
      size: file.size,
      done: 0,
      state: "waiting",
    };
    this.uploads.set(t.id, { t, file });
    this.onChange({ ...t });
    this.sink.send({ type: "file_offer", transferId: t.id, size: file.size, name });
    return t;
  }

  download(path: string, writer: DownloadWriter | null = null): Transfer {
    const t: Transfer = {
      id: this.allocId(),
      direction: "download",
      name: baseName(path),
      size: 0,
      done: 0,
      state: "waiting",
    };
    this.downloads.set(t.id, { t, writer, parts: [], nextSeq: 0, chain: Promise.resolve() });
    this.onChange({ ...t });
    this.sink.send({ type: "file_request", transferId: t.id, path });
    return t;
  }

  cancel(id: number): void {
    const up = this.uploads.get(id);
    if (up) {
      this.uploads.delete(id);
      this.sink.send({ type: "file_end", transferId: id, status: FileStatus.Cancelled });
      this.update(up.t, { state: "cancelled" });
      return;
    }
    const dl = this.downloads.get(id);
    if (dl) {
      this.downloads.delete(id);
      this.sink.send({ type: "file_end", transferId: id, status: FileStatus.Cancelled });
      void dl.writer?.abort().catch(() => {});
      this.update(dl.t, { state: "cancelled" });
    }
  }

  /** The transport closed: everything in flight has failed. */
  abortAll(reason = "connection lost"): void {
    for (const up of this.uploads.values()) this.update(up.t, { state: "error", error: reason });
    for (const dl of this.downloads.values()) {
      void dl.writer?.abort().catch(() => {});
      this.update(dl.t, { state: "error", error: reason });
    }
    this.uploads.clear();
    this.downloads.clear();
  }

  get busy(): boolean {
    return this.uploads.size + this.downloads.size > 0;
  }

  onMessage(msg: FileMessage): void {
    switch (msg.type) {
      case "file_accept":
        return this.onAccept(msg.transferId, msg.ok, msg.reason);
      case "file_offer":
        return this.onOffer(msg.transferId, msg.size, msg.name);
      case "file_chunk":
        return this.onChunk(msg.transferId, msg.seq, msg.data);
      case "file_end":
        return this.onEnd(msg.transferId, msg.status);
      case "file_request":
        // The host never asks us for files; refuse rather than ignore.
        this.sink.send({ type: "file_end", transferId: msg.transferId, status: FileStatus.Error });
    }
  }

  private onAccept(id: number, ok: boolean, reason: string): void {
    const up = this.uploads.get(id);
    if (!up) return;
    if (!ok) {
      this.uploads.delete(id);
      this.update(up.t, { state: "error", error: reason || "refused by the host" });
      return;
    }
    this.update(up.t, { state: "active" });
    void this.pump(up);
  }

  private async pump(up: Upload): Promise<void> {
    const id = up.t.id;
    let seq = 0;
    let offset = 0;
    try {
      while (offset < up.file.size) {
        await this.sink.drain();
        if (!this.uploads.has(id)) return; // cancelled meanwhile
        const end = Math.min(offset + FILE_CHUNK_LEN, up.file.size);
        const data = new Uint8Array(await up.file.slice(offset, end).arrayBuffer());
        if (!this.uploads.has(id)) return;
        this.sink.send({ type: "file_chunk", transferId: id, seq, data });
        seq = (seq + 1) >>> 0;
        offset = end;
        this.update(up.t, { done: offset });
      }
      this.sink.send({ type: "file_end", transferId: id, status: FileStatus.Complete });
      // stays active until the host confirms it saved the file
    } catch (e) {
      if (!this.uploads.has(id)) return;
      this.uploads.delete(id);
      this.sink.send({ type: "file_end", transferId: id, status: FileStatus.Error });
      this.update(up.t, { state: "error", error: `could not read the file: ${e}` });
    }
  }

  private onOffer(id: number, size: number, name: string): void {
    const dl = this.downloads.get(id);
    if (!dl) {
      // An offer we never asked for: refuse it.
      this.sink.send({ type: "file_accept", transferId: id, ok: false, reason: "unexpected offer" });
      return;
    }
    if (!dl.writer && size > this.opts.maxMemoryDownload) {
      this.downloads.delete(id);
      const reason = `too large for this browser (${formatBytes(size)}; limit ${formatBytes(this.opts.maxMemoryDownload)})`;
      this.sink.send({ type: "file_accept", transferId: id, ok: false, reason });
      this.update(dl.t, { state: "error", error: reason, size, name });
      return;
    }
    this.sink.send({ type: "file_accept", transferId: id, ok: true, reason: "" });
    this.update(dl.t, { state: "active", size, name });
  }

  private onChunk(id: number, seq: number, data: Uint8Array): void {
    const dl = this.downloads.get(id);
    if (!dl) return;
    if (seq !== dl.nextSeq || dl.t.done + data.length > dl.t.size) {
      this.fail(dl, "transfer corrupted (chunk out of order)");
      return;
    }
    dl.nextSeq = (dl.nextSeq + 1) >>> 0;
    // The bytes may be a view into a reassembly buffer: copy before keeping.
    const copy = data.slice();
    if (dl.writer) {
      const writer = dl.writer;
      dl.chain = dl.chain.then(() => writer.write(copy));
    } else {
      dl.parts.push(copy);
    }
    this.update(dl.t, { done: dl.t.done + copy.length });
  }

  private fail(dl: Download, error: string): void {
    this.downloads.delete(dl.t.id);
    this.sink.send({ type: "file_end", transferId: dl.t.id, status: FileStatus.Error });
    void dl.writer?.abort().catch(() => {});
    this.update(dl.t, { state: "error", error });
  }

  private onEnd(id: number, status: FileStatus): void {
    const up = this.uploads.get(id);
    if (up) {
      this.uploads.delete(id);
      if (status === FileStatus.Complete) {
        this.update(up.t, { state: "complete", done: up.t.size });
      } else {
        this.update(up.t, { state: "error", error: STATUS_TEXT[status] ?? "failed" });
      }
      return;
    }
    const dl = this.downloads.get(id);
    if (!dl) return;
    if (status !== FileStatus.Complete) {
      this.downloads.delete(id);
      void dl.writer?.abort().catch(() => {});
      const error =
        dl.t.state === "waiting"
          ? "the host could not open that file (check the path)"
          : (STATUS_TEXT[status] ?? "failed");
      this.update(dl.t, { state: "error", error });
      return;
    }
    if (dl.t.done !== dl.t.size) {
      this.fail(dl, `incomplete: got ${dl.t.done} of ${dl.t.size} bytes`);
      return;
    }
    this.downloads.delete(id);
    if (dl.writer) {
      const writer = dl.writer;
      dl.chain
        .then(() => writer.close())
        .then(
          () => this.update(dl.t, { state: "complete" }),
          (e) => this.update(dl.t, { state: "error", error: `could not save: ${e}` }),
        );
    } else {
      this.opts.save(dl.t.name, new Blob(dl.parts as BlobPart[]));
      dl.parts = [];
      this.update(dl.t, { state: "complete" });
    }
  }
}
