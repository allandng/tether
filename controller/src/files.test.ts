import { describe, expect, it } from "vitest";
import { type FileSink, FileTransfers, type Transfer, baseName, formatBytes } from "./files";
import { FILE_CHUNK_LEN, type FileMessage, FileStatus } from "./protocol";

function harness(maxMemoryDownload = 1 << 20) {
  const sent: FileMessage[] = [];
  const states = new Map<number, Transfer>();
  const saved: { name: string; blob: Blob }[] = [];
  const sink: FileSink = { send: (m) => sent.push(m), drain: () => Promise.resolve() };
  const ft = new FileTransfers(sink, (t) => states.set(t.id, t), {
    maxMemoryDownload,
    save: (name, blob) => saved.push({ name, blob }),
  });
  return { ft, sent, states, saved };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe("uploads", () => {
  it("offer → accept → chunks in order → end → complete on host confirm", async () => {
    const { ft, sent, states } = harness();
    const bytes = new Uint8Array(FILE_CHUNK_LEN * 2 + 10).map((_, i) => i % 251);
    const t = ft.upload(new Blob([bytes]), "photo.jpg");
    expect(sent[0]).toEqual({ type: "file_offer", transferId: t.id, size: bytes.length, name: "photo.jpg" });

    ft.onMessage({ type: "file_accept", transferId: t.id, ok: true, reason: "" });
    for (let i = 0; i < 10; i++) await flush();
    const chunks = sent.filter((m) => m.type === "file_chunk");
    expect(chunks.map((c) => c.type === "file_chunk" && c.seq)).toEqual([0, 1, 2]);
    const joined = new Uint8Array(chunks.flatMap((c) => (c.type === "file_chunk" ? [...c.data] : [])));
    expect(joined).toEqual(bytes);
    expect(sent[sent.length - 1]).toEqual({ type: "file_end", transferId: t.id, status: FileStatus.Complete });
    expect(states.get(t.id)?.state).toBe("active"); // until the host confirms

    ft.onMessage({ type: "file_end", transferId: t.id, status: FileStatus.Complete });
    expect(states.get(t.id)?.state).toBe("complete");
    expect(ft.busy).toBe(false);
  });

  it("a refused offer surfaces the host's reason", () => {
    const { ft, states } = harness();
    const t = ft.upload(new Blob(["x"]), "big.iso");
    ft.onMessage({ type: "file_accept", transferId: t.id, ok: false, reason: "not enough free disk space on the host" });
    expect(states.get(t.id)).toMatchObject({ state: "error", error: "not enough free disk space on the host" });
  });

  it("cancel sends FileEnd(cancelled) and stops the pump", async () => {
    const { ft, sent, states } = harness();
    const t = ft.upload(new Blob([new Uint8Array(FILE_CHUNK_LEN * 5)]), "a.bin");
    ft.onMessage({ type: "file_accept", transferId: t.id, ok: true, reason: "" });
    ft.cancel(t.id);
    for (let i = 0; i < 10; i++) await flush();
    expect(sent.filter((m) => m.type === "file_chunk").length).toBeLessThan(5);
    expect(sent).toContainEqual({ type: "file_end", transferId: t.id, status: FileStatus.Cancelled });
    expect(states.get(t.id)?.state).toBe("cancelled");
  });
});

describe("downloads", () => {
  it("request → offer → accept → chunks → saved blob", async () => {
    const { ft, sent, states, saved } = harness();
    const t = ft.download("C:\\Users\\me\\notes.txt");
    expect(t.name).toBe("notes.txt");
    expect(sent[0]).toEqual({ type: "file_request", transferId: t.id, path: "C:\\Users\\me\\notes.txt" });

    ft.onMessage({ type: "file_offer", transferId: t.id, size: 5, name: "notes.txt" });
    expect(sent[1]).toEqual({ type: "file_accept", transferId: t.id, ok: true, reason: "" });
    ft.onMessage({ type: "file_chunk", transferId: t.id, seq: 0, data: new Uint8Array([104, 105]) });
    ft.onMessage({ type: "file_chunk", transferId: t.id, seq: 1, data: new Uint8Array([33, 33, 33]) });
    expect(states.get(t.id)?.done).toBe(5);
    ft.onMessage({ type: "file_end", transferId: t.id, status: FileStatus.Complete });
    expect(states.get(t.id)?.state).toBe("complete");
    expect(saved[0]?.name).toBe("notes.txt");
    expect(await saved[0]?.blob.text()).toBe("hi!!!");
  });

  it("refuses an in-memory download over the cap with a readable reason", () => {
    const { ft, sent, states } = harness(100);
    const t = ft.download("/big.iso");
    ft.onMessage({ type: "file_offer", transferId: t.id, size: 101, name: "big.iso" });
    const reply = sent[1];
    expect(reply?.type === "file_accept" && reply.ok).toBe(false);
    expect(reply?.type === "file_accept" && reply.reason).toMatch(/too large/);
    expect(states.get(t.id)?.state).toBe("error");
  });

  it("streams to a writer when one is supplied, with no size cap", async () => {
    const { ft, states } = harness(1);
    const written: number[] = [];
    let closed = false;
    const t = ft.download("/v.mp4", {
      write: async (d) => {
        written.push(...d);
      },
      close: async () => {
        closed = true;
      },
      abort: async () => {},
    });
    ft.onMessage({ type: "file_offer", transferId: t.id, size: 3, name: "v.mp4" });
    ft.onMessage({ type: "file_chunk", transferId: t.id, seq: 0, data: new Uint8Array([1, 2, 3]) });
    ft.onMessage({ type: "file_end", transferId: t.id, status: FileStatus.Complete });
    for (let i = 0; i < 5; i++) await flush();
    expect(written).toEqual([1, 2, 3]);
    expect(closed).toBe(true);
    expect(states.get(t.id)?.state).toBe("complete");
  });

  it("a bad path, a short transfer, and a dropped connection all fail cleanly", () => {
    const { ft, states } = harness();
    const missing = ft.download("/nope");
    ft.onMessage({ type: "file_end", transferId: missing.id, status: FileStatus.Error });
    expect(states.get(missing.id)?.error).toMatch(/could not open/);

    const short = ft.download("/short");
    ft.onMessage({ type: "file_offer", transferId: short.id, size: 10, name: "short" });
    ft.onMessage({ type: "file_end", transferId: short.id, status: FileStatus.Complete });
    expect(states.get(short.id)?.error).toMatch(/incomplete/);

    const live = ft.download("/live");
    ft.abortAll();
    expect(states.get(live.id)?.state).toBe("error");
    expect(ft.busy).toBe(false);
  });

  it("an unsolicited offer is refused", () => {
    const { ft, sent } = harness();
    ft.onMessage({ type: "file_offer", transferId: 999, size: 1, name: "x" });
    expect(sent[0]).toMatchObject({ type: "file_accept", transferId: 999, ok: false });
  });
});

describe("helpers", () => {
  it("baseName handles both separators and quotes", () => {
    expect(baseName("C:\\a\\b.txt")).toBe("b.txt");
    expect(baseName("/home/me/c.png")).toBe("c.png");
    expect(baseName('"C:\\x\\quoted.pdf"')).toBe("quoted.pdf");
    expect(baseName("/dir/")).toBe("download");
  });

  it("formatBytes", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(150 * 1024 * 1024)).toBe("150 MB");
  });
});
