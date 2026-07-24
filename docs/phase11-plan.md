# Phase 11 Plan — file transfer

**Scope:** move files between the controller and the host in both directions.
The last of the Chrome Remote Desktop parity features, and the cheapest,
because most of the transport work already exists.

---

## 1. What already exists

- **`tether-bulk`** is a third WebRTC data channel, added in Phase 3 to carry
  oversized clipboard payloads without blocking the control channel. That is
  exactly the right pipe for file bytes.
- **`chunks.ts`** already splits and reassembles payloads at 16 KiB, because
  `webrtc-rs` drops messages over 64 KiB. File chunks reuse it verbatim.
- **Unknown message types are length-skipped**, so the new messages need no
  protocol version bump.

On the LAN WebSocket transport there is no separate bulk channel — everything
shares one socket — so a transfer there interleaves with frames rather than
running beside them. The messages are transport-agnostic either way; only the
multiplexing differs.

## 2. Protocol

Four additive message types (`0x0D` is taken by Phase 7's `HostInfo`):

| Type | Direction | Payload |
|---|---|---|
| `0x0E FileOffer` | either | `transfer_id`, `name`, `size` |
| `0x0F FileAccept` | either | `transfer_id`, `ok`, optional reason |
| `0x10 FileChunk` | either | `transfer_id`, `seq`, bytes |
| `0x11 FileEnd` | either | `transfer_id`, status: complete / cancelled / error |

Offer-then-accept rather than pushing bytes immediately, so the receiver can
refuse on size, disk space, or a bad name before anything is transferred. A
single `FileEnd` covers success, cancellation, and failure — one teardown path
instead of three.

Cross-pinned test vectors in both `tether-protocol` and `protocol.ts`, as every
message type in this project already has.

## 3. Host-side safety

The sharpest part of this phase. A remote peer is supplying a filename, and
filenames are a well-trodden way to write outside where you meant to.

**Incoming files:**

- **Destination is fixed** — `~/Downloads`, chosen by the host, never by the
  controller. The offer carries a name, not a path.
- **Sanitise to a basename:** reject path separators, `..`, NUL, and control
  characters. On Windows additionally reject reserved device names (`CON`,
  `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, with or without an
  extension) and strip trailing dots and spaces, which Windows silently
  discards in a way that defeats naive suffix checks.
- **Never overwrite.** On collision, suffix ` (1)`, ` (2)`, and so on.
- **Write to a temp file in the destination directory, then rename on
  completion.** A cancelled or crashed transfer must never leave a truncated
  file wearing the real name. Same directory so the rename stays atomic.
- **Check free space against the offered size** before accepting, and enforce a
  configurable maximum.
- Pure functions for name sanitisation and collision resolution, unit-tested
  with a table of hostile inputs. This is the part most worth testing
  adversarially and the easiest to test well.

**Outgoing files** raise a question worth answering explicitly rather than
leaving implicit: should the controller be able to name an arbitrary host path
to download?

Yes, and restricting it would be theatre. A paired controller already has full
mouse and keyboard control of the host — it can open a file manager and do
anything a person sitting there could. A path allowlist would add friction
without adding security. What it *does* deserve is an audit trail: log every
transfer with its path, size, and the requesting `device_id`, at `info` level.

## 4. Backpressure

Not optional, and the thing most likely to be got wrong. Writing a large file
into a data channel as fast as `read()` returns will balloon the send buffer
and, past the SCTP limits, drop or stall the connection.

Both ends pause above a high-water mark on the channel's buffered amount and
resume on the low threshold. `adaptive.rs` already samples
`buffered_amount()` for the media channel, so the mechanism is familiar; here
it gates a producer instead of steering a bitrate. On the browser side that is
`bufferedAmountLowThreshold` plus the `bufferedamountlow` event.

Related, and worth stating in the docs rather than discovering in use: bulk and
media share one SCTP association, so a large transfer **will** degrade video
during it. The adaptive-bitrate loop watches the media channel and will see the
effect but not the cause. Accept it, document it, and consider a modest rate
cap on transfers if it turns out to be unpleasant.

## 5. Controller UX

- **Upload:** `<input type="file">`, which on iOS opens the Photos and Files
  sheet — no custom picker needed.
- **Download:** collect chunks, assemble a `Blob`, trigger an `<a download>`.

**The phone-side constraint is memory.** Streaming a download straight to disk
needs the File System Access API, which is Chromium-only — Safari has to hold
the whole file in memory before it can be saved. So cap controller-side
downloads at something like 100 MB for v1, refuse larger ones in `FileAccept`
with a clear reason, and revisit with service-worker streaming only if it
matters. Uploads have no such limit; `File` is already a streamable handle.

Progress and cancel per transfer in the session view. Resume after
disconnection is **out of scope** — a failed transfer restarts.

## 6. Module order

1. **M1 — protocol:** four message types, Rust and TS with cross-pinned
   vectors.
2. **M2 — safety primitives:** name sanitisation, collision resolution, temp
   file plus atomic rename, hostile-input test table.
3. **M3 — host transfer engine:** send and receive over `tether-bulk`,
   backpressure, size and space checks, audit logging.
4. **M4 — controller:** upload picker, download assembly, progress, cancel,
   size cap.
5. **M5 — gate.**

## 7. Gate criteria (proposed)

1. A file uploads from a phone to the host and arrives byte-identical
   (checksum compared) in `~/Downloads`.
2. A file downloads from the host to a phone, byte-identical, on both iOS and
   Android.
3. A large transfer — several hundred MB — completes without unbounded memory
   growth on either side, and the session stays connected throughout.
4. The hostile-name table is all rejected or sanitised, on both platforms: no
   write lands outside the destination directory.
5. Cancelling mid-transfer, and dropping the connection mid-transfer, both
   leave no file wearing the final name.
6. A transfer offered above the size limit, or with insufficient free space, is
   refused at offer time with a reason the user can read.
7. Video degrades during a large transfer but recovers afterwards, and the
   session never drops.

## 8. Risks

- **Path handling is the security-critical surface** and Windows makes it
  worse than it looks — reserved device names and silently-stripped trailing
  characters are the classic traps. Mitigated by pure, table-tested functions
  and by the destination directory never being controller-supplied.
- **Backpressure bugs surface only at scale.** A 1 MB test file will pass while
  a 500 MB file stalls the connection. Criterion 3 exists to force the real
  test.
- **iOS memory limits** on large downloads may bite below the proposed cap;
  tune against a real device rather than a number chosen here.
- **Scope creep into a file browser.** Remote directory listing, rename, and
  delete are a different feature. CRD had a simple transfer, and so should
  this.

---

**Status: planned, not started. Independent of Phases 7–10; depends on Phase 6
only in the sense that everything does.**
