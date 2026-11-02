# Phase 11 gate results — file transfer

Built per [phase11-plan.md](phase11-plan.md).

## What shipped

- **Protocol**: `0x0E FileOffer`, `0x0F FileAccept`, `0x10 FileChunk`,
  `0x11 FileEnd`, plus `0x12 FileRequest` (controller → host: "send me this
  path"), which the plan implied but didn't list. The controller allocates
  every transfer id, so ids are unique per session in both directions.
  Cross-pinned vectors in Rust and TS; hostile lengths rejected on decode.
  Chunks are 16 KiB − 64 so one encoded chunk fits one bulk data-channel
  message.
- **Host engine** (`files.rs`, one per session): uploads land in the user's
  Downloads folder (`--upload-dir` to change), never a controller-chosen path.
  `sanitize_file_name` refuses separators, `.`/`..`, control characters, >255
  bytes, and — on Windows — `< > : " | ? *` and reserved device names; trailing
  dots/spaces are stripped first. Bytes go to `.tether-<random>.part` and get
  their final name via a hard link that fails rather than overwrite
  (`name (1).ext`, …); cancel, error, short transfer or disconnect deletes the
  temp file. Size cap (`--max-upload-mb`, default 4096) and free-space check
  at offer time, refused with readable reasons. Downloads take any host path
  (a paired controller already has full control) and every transfer is
  logged with path, size and peer.
- **Backpressure**: host download reads are paced by a bounded queue and, on
  WebRTC, by the bulk channel's buffered amount (1 MiB high-water); the
  controller awaits `bufferedamountlow` (WebRTC) or polls `bufferedAmount`
  (LAN). On WebRTC one writer task owns `tether-bulk` so each message's chunks
  go out back to back (the reassembler is latest-wins per sequence).
- **Controller** (`files.ts` + Files drawer): upload via file picker or
  drag-and-drop onto the screen; download by host path — streamed straight to
  disk with the File System Access API where available (Chromium), else
  collected and saved as a Blob, capped at 100 MB on touch devices and 1 GB
  elsewhere, refused up front in `FileAccept` with the reason. Per-transfer
  progress and cancel.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | Upload lands byte-identical | ✅ 5 MiB random file over WebRTC: SHA-256 identical, 0.43 s. Also LAN (integration test). |
| 2 | Download byte-identical on iOS and Android | ✅ desktop browser over WebRTC: same 5 MiB file back, SHA-256 identical, 316 ms. ⏳ phones. |
| 3 | Several-hundred-MB transfer without unbounded memory, session stays up | ✅ 300 MiB upload over WebRTC (Windows host, desktop browser): SHA-256 identical, 13.5 s (~22 MB/s); `tetherd` working set sampled every 0.5 s stayed between 118 and 127 MB; the session stayed connected and video was at 25 fps right after. Downloads of that size not measured (browser memory cap applies without the File System Access API). |
| 4 | Hostile-name table rejected/sanitised, both platforms | ✅ 30-entry table under Windows and Unix rules; live: `CON.txt` refused with "reserved Windows device name". |
| 5 | Cancel / disconnect leaves no file under the final name | ✅ Engine tests: cancelled, short, out-of-order, oversize and dropped-session uploads all leave the folder empty. |
| 6 | Over-limit or insufficient-space offer refused at offer time with a reason | ✅ Tested (size cap; free-space path uses `GetDiskFreeSpaceExW` / `statvfs`). |
| 7 | Video degrades during a large transfer but recovers | ⏳ Not measured; bulk and media share one SCTP association, as documented. |
