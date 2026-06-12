# Tether wire protocol — version 1

Transport-agnostic binary protocol between a **host** (`tetherd`, the machine
being controlled) and a **controller** (the viewer). Phase 1 carries it over
WebSocket; Phase 2 will carry the identical bytes over a WebRTC data channel.

All multi-byte integers are **little-endian**.

## Framing

Every message:

```
offset  size  field
0       4     total_len : u32   — length of everything after this field
4       1     msg_type  : u8
5       n     payload           — n = total_len - 1
```

- `total_len` must be ≥ 1 and ≤ `67108864` (64 MiB); anything else is corrupt
  and the connection should be closed.
- **Byte-stream transports** (TCP): messages are concatenated; the length
  prefix delimits them.
- **Message-oriented transports** (WebSocket binary frames, WebRTC data
  channels): exactly one protocol message per transport message, length prefix
  included. The bytes are identical on every transport.
- **Unknown `msg_type`**: skip `total_len` bytes and continue. This is the
  forward-compatibility mechanism — new message types can be added without a
  version bump as long as ignoring them is safe.

## Versioning

`Hello.version` (u16) is the protocol version, currently **1**. Each side
sends `Hello` first; a side that does not support the peer's version closes
the connection. Version bumps are reserved for changes where ignoring unknown
message types is *not* a safe fallback.

## Connection sequence

```
controller → host : Hello { version, role=controller, caps }
host       → controller : Hello { version, role=host, caps }     (or close: bad version/role)
host       → controller : Resolution { width, height }
host       → controller : FrameData ...                          (continuous)
controller → host : InputEvent ...                               (as input happens)
host       → controller : Resolution                             (again, if capture size changes)
```

The host validates that the peer's `Hello` has `role = controller` and the
`can_control` capability before streaming anything.

## Messages

### 0x01 — Hello (both directions, once)

```
0  u16  version
2  u8   role          0 = host, 1 = controller
3  u8   capabilities  bit 0 (0x01) = can_host, bit 1 (0x02) = can_control
```

Capabilities describe the *peer device*, not the current session. A
control-only device (phone/tablet) never sets `can_host` — and its build
contains no host code path at all.

### 0x02 — Resolution (host → controller)

```
0  u32  width    capture width in pixels
4  u32  height   capture height in pixels
```

Sent once after `Hello`, and again any time the capture dimensions change.
Controllers must treat it as authoritative for aspect ratio; input coordinates
are normalized (below) and unaffected.

### 0x03 — FrameData (host → controller)

```
0   u8   codec              0 = JPEG, 1 = H.264
1   u32  seq                monotonically increasing per session
5   u64  timestamp_micros   capture time, host clock — diagnostics only,
                            never compare across machines
13  …    frame bytes        one complete encoded frame (JPEG image or H.264
                            access unit)
```

Frames are independently decodable in JPEG mode. Receivers should render
latest-wins and tolerate gaps in `seq` (the host drops frames under
backpressure by design).

### 0x04 — InputEvent (controller → host)

```
0  u8  kind
1  …   body (per kind)
```

| kind | name | body |
|---|---|---|
| 0 | MouseMove | `u16 x, u16 y` |
| 1 | MouseDown | `u8 button, u16 x, u16 y` |
| 2 | MouseUp | `u8 button, u16 x, u16 y` |
| 3 | Scroll | `i16 dx, i16 dy` |
| 4 | KeyDown | `u8 modifiers, u8 code_len, code_len×u8 code` |
| 5 | KeyUp | `u8 modifiers, u8 code_len, code_len×u8 code` |

**Coordinates** (`x`, `y`): normalized fixed-point. `0..=65535` spans the
host's capture area (`0,0` = top-left, `65535,65535` = bottom-right). The
controller computes them over the *displayed video rectangle* (excluding
letterbox bars); the host multiplies by its own screen dimensions. This makes
mismatched resolutions and Retina scale factors a non-issue by construction.

**button**: DOM numbering — 0 left, 1 middle, 2 right.

**Scroll deltas**: pixels (DOM `WheelEvent` with `deltaMode = 0`), positive =
content moves down/right (natural reading of DOM deltas).

**code**: UTF-8 W3C [`KeyboardEvent.code`](https://www.w3.org/TR/uievents-code/)
string (physical key, keyboard-layout independent), max 32 bytes. The host owns
the mapping to platform virtual key codes. Modifier keys (ShiftLeft, MetaRight,
…) travel as ordinary KeyDown/KeyUp events; combos work because the host holds
the modifier key down for as long as the controller does.

**modifiers** bitmask (informational; redundant with modifier key events):
bit 0 shift, bit 1 ctrl, bit 2 alt/option, bit 3 meta/cmd.

### 0x05 — ClipboardData (both directions)

```
0  u8  kind     0 = UTF-8 text (only kind defined; others reserved for
                images/files — receivers reject unknown kinds)
1  …   payload  the text, ≤ 262144 bytes (256 KiB)
```

Oversized content must be refused by sender and receiver, never truncated.
Clipboard rides the ordered control path; receivers should apply it before
processing any input event that arrives after it (this ordering is what makes
"sync clipboard, then press Cmd+V" paste the right thing). Loop prevention is
the application's job: don't re-send content identical to what was last
applied or sent.

## WebRTC transport mapping (Phase 2)

Two data channels, both carrying the wire format above unchanged:

- **`tether-ctl`** (reliable, ordered): `Hello`, `Resolution`, `InputEvent`.
- **`tether-media`** (reliable, ordered): `FrameData` only, split into chunks
  because browsers cap data-channel message sizes. Each chunk is
  `[u32 LE frame_seq][u16 LE chunk_idx][u16 LE chunk_count]` followed by a
  slice of the complete wire message; payload ≤ 64 KiB − 8. Reassembly is
  latest-wins. The chunk header is transport framing, not part of the tether
  protocol (reference impls: `controller/src/chunks.ts`,
  `crates/tetherd/src/webrtc.rs`).

H.264 `FrameData` payloads are Annex B access units (4-byte start codes,
SPS/PPS in-band on keyframes); each payload is one complete, independently
parseable access unit, and keyframes recur every ~2 s.

## Reference implementations

- Rust: `crates/tether-protocol` (canonical)
- TypeScript: `controller/src/protocol.ts` (must round-trip byte-identically;
  both sides carry the same test vectors)
