# Tether wire protocol — version 1

Transport-agnostic binary protocol between a **host** (`tetherd`, the machine
being controlled) and a **controller** (the viewer). The identical bytes ride
a LAN WebSocket or WebRTC data channels (see the transport mapping below).

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
   ...                    Auth / PairRequest ⇄ AuthResult / PairResult
host       → controller : Resolution { width, height }
host       → controller : ClipboardData, Displays, HostInfo      (current state)
host       → controller : FrameData ...                          (continuous)
controller → host : InputEvent ...                               (as input happens)
host       → controller : Resolution                             (again, if capture size changes)
either     → either     : File* ...                              (transfers, any time after auth)
```

The host validates that the peer's `Hello` has `role = controller` and the
`can_control` capability before streaming anything.

## Messages

### 0x01 — Hello (both directions, once)

```
0  u16  version
2  u8   role          0 = host, 1 = controller
3  u8   capabilities  bit 0 (0x01) = can_host, bit 1 (0x02) = can_control,
                      bit 2 (0x04) = audio,    bit 3 (0x08) = files
```

Capabilities describe the *peer device*, not the current session. A
control-only device (phone/tablet) never sets `can_host` — and its build
contains no host code path at all.

- **audio** (`CAP_AUDIO`, Phase 10): controller — can play a WebRTC audio
  track (set on WebRTC only); host — will attach one, so it sets the bit only
  when run with `--audio`, and never in its LAN Hello. See *System audio*
  below.
- **files** (`CAP_FILES`, Phase 11): this peer speaks the file-transfer
  messages `0x0E`–`0x12`. Current hosts and controllers always set it.

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

**modifiers** bitmask. Bits 0-3 are informational (redundant with modifier
key events): bit 0 shift, bit 1 ctrl, bit 2 alt/option, bit 3 meta/cmd.
Bits 4-5 carry the controller's Caps Lock *state* (a lock, not a held key):

- bit 4 `CAPS_LOCK` — Caps Lock is on at the controller.
- bit 5 `CAPS_LOCK_KNOWN` — the sender sampled its lock state for this event
  (browser: `KeyboardEvent.getModifierState("CapsLock")`), so bit 4 is
  authoritative. When clear, bit 4 means nothing and hosts don't sync their
  lock state from it — events without it (older controllers, synthetic
  key-ups such as the blur release) must not read as "Caps Lock off". (A
  `CapsLock` *key* event without it is still a press; see below.)

A host that honours the lock bits syncs its own Caps Lock state to bit 4
whenever bit 5 is set and the two differ, and does not also inject the
`CapsLock` key event (that would toggle twice). The `CapsLock` key event is
still sent, so hosts that don't read the lock bits keep their old behaviour.

The reverse case — a *sender* that predates the lock bits (an old or cached
controller build) talking to a host that reads them — is the host's job: a
`CapsLock` key event **without** bit 5 is that sender's press and must still
change the lock. The Windows host injects the key as it always has. The
macOS host can't (a synthetic Caps Lock key doesn't toggle the lock), so it
toggles its HID lock state instead, once per press. Browsers disagree on
what a press sends — Windows/Linux/ChromeOS: keydown + keyup (plus
auto-repeat keydowns while held); macOS: a keydown when the lock turns on and
only a keyup when it turns off — and the sender's OS isn't on the wire, so
the macOS host applies one rule to both: a keydown toggles unless one is
already outstanding (auto-repeat); a keyup toggles unless it closes an
outstanding keydown that auto-repeated or is under 500 ms old (the release
half of a press). The residual ambiguity — a Mac user turning the lock on and
off again within 500 ms, or a held key that hasn't started repeating after
500 ms — can leave the two lock states inverted until the next press, which
is why current senders set bit 5.

Bits 6-7 are reserved: senders write 0 and receivers ignore them. Hosts must
ignore modifier bits they don't understand — which is what makes adding bits
here backwards compatible without a version bump.

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

### 0x06 — TextInput (controller → host)

```
0  …  payload  committed UTF-8 text, ≤ 1024 bytes
```

Committed text from a soft keyboard (no usable `KeyboardEvent.code`). The host
injects it as Unicode directly (e.g. `CGEventKeyboardSetUnicodeString` on
macOS), bypassing the DOM-code → virtual-key path — which can't express emoji
or non-US layouts. Hardware keyboards still use `InputEvent` KeyDown/KeyUp;
`TextInput` is specifically for on-screen-keyboard character commits. Refused
above the cap, never truncated.

### 0x07–0x0A — pairing / auth (both directions, Phase 5)

`PairRequest` / `PairResult` / `Auth` / `AuthResult` carry the device-pairing
handshake (`u16`-length-prefixed fields). See `docs/phase5-gate-results.md` and
`crates/tetherd/src/auth.rs` for the scheme; they ride the control path before
any frames flow.

### 0x0B — Displays (host → controller, Phase 5b)

```
0  u8  count
1  …   count × { u32 id, u32 width, u32 height, u8 active, u16 name_len, name… }
```

The displays the host can capture. Sent after auth and whenever the set or the
active display changes. `width`/`height` are the captured (native) pixels;
`active` marks the one currently streaming.

### 0x0C — SelectDisplay (controller → host, Phase 5b)

```
0  u32  id   the display id (from a Displays entry) to switch capture to
```

The host switches its single capture to that display and re-announces
`Resolution` + `Displays` to all connected controllers. Shared: all controllers
view the same display.

### Length-prefixed fields

The messages below (like pairing/auth) carry **fields**: `u16 len` then
`len` bytes, UTF-8 for strings. A field is at most 512 bytes; a message may
cap it lower.

### 0x0D — HostInfo (host → controller, Phase 7)

```
0  u8     os        0 = unknown, 1 = macOS, 2 = Windows, 3 = Linux
1  field  version   tetherd's version string (e.g. "0.1.0")
…  field  name      the host's display name
```

Sent once after auth, right after the initial `Displays`, on the control
path. The controller uses `os` for its modifier policy (Cmd → Ctrl against a
Windows host, `controller/src/remap.ts`) and shows `name`.

### 0x0E — FileOffer (either direction, Phase 11)

```
0   u32    transfer_id
4   u64    size        bytes the sender will send
12  field  name        bare file name, ≤ 255 bytes — never a path
```

The receiver chooses where the file lands (the host: the user's Downloads
folder or `--upload-dir`, after reducing `name` to a safe basename or
refusing it).

### 0x0F — FileAccept (either direction, Phase 11)

```
0  u32    transfer_id
4  u8     ok          1 = accepted, 0 = refused
5  field  reason      human-readable refusal reason; empty when ok
```

### 0x10 — FileChunk (either direction, Phase 11)

```
0  u32  transfer_id
4  u32  seq         0, 1, 2, … in order; a gap is an error
8  …    bytes       rest of payload, ≤ 61440 (60 KiB)
```

Receivers reject chunks over 60 KiB (webrtc-rs drops data-channel messages
over 64 KiB). Senders use `16 KiB − 64` = 16320 bytes, so one encoded chunk
fits a single 16 KiB `tether-bulk` message and never holds up the control
traffic sharing its transport for long.

### 0x11 — FileEnd (either direction, Phase 11)

```
0  u32  transfer_id
4  u8   status       0 = complete, 1 = cancelled, 2 = error
```

The single teardown path for success, cancellation and failure. From the
sender, `complete` means every chunk has been sent; from the receiver, that
the file was saved. Either side may send `cancelled` or `error` at any
point. Other status values are corrupt.

### 0x12 — FileRequest (controller → host, Phase 11)

```
0  u32  transfer_id
4  …    path         UTF-8 host path, rest of payload, ≤ 4096 bytes
```

Asks the host to send a file. The host answers with a `FileOffer` carrying
the same id, or a `FileEnd` error (e.g. no such file).

### File transfer flow

The **controller allocates every `transfer_id`** — its uploads and the ids in
its `FileRequest`s — so ids are unique per session in both directions.

```
upload (controller → host):
  controller → FileOffer  { id, size, name }
  host       → FileAccept { id, ok, reason }       (refused → done)
  controller → FileChunk  { id, seq = 0, 1, … } …
  controller → FileEnd    { id, complete }         (all chunks sent)
  host       → FileEnd    { id, complete }         (saved; or error)

download (host → controller):
  controller → FileRequest { id, path }
  host       → FileOffer   { id, size, name }      (or FileEnd error)
  controller → FileAccept  { id, ok, reason }      (refused → done)
  host       → FileChunk   { id, seq = 0, 1, … } …
  host       → FileEnd     { id, complete }
```

The host writes an upload to a hidden temp file and gives it its real name
only once complete (never overwriting: collisions get ` (1)`, ` (2)`, …); a
cancelled, failed or disconnected transfer deletes it. File messages from an
unauthenticated peer are ignored.

## WebRTC transport mapping (Phase 2)

Three data channels, all carrying the wire format above unchanged:

- **`tether-ctl`** (reliable, ordered): `Hello`, `Resolution`, `InputEvent` —
  small messages only (single data-channel messages cap at ~16 KiB for safe
  interop; some stacks silently drop larger ones).
- **`tether-media`** (reliable, ordered): `FrameData` only, chunked.
- **`tether-bulk`** (reliable, ordered): anything oversized or bulky —
  `ClipboardData` and every file-transfer message (`0x0E`–`0x12`) — chunked
  with the same framing. One writer owns each side's send path, so a
  message's chunks always go out back to back (reassembly is latest-wins per
  sequence number); the host pauses file data while more than 1 MiB is
  queued in the SCTP send buffer. `TextInput` and `HostInfo` are small and
  ride `tether-ctl`.

Chunk framing (media and bulk): `[u32 LE seq][u16 LE chunk_idx]
[u16 LE chunk_count]` followed by a slice of the complete wire message;
payload ≤ 16 KiB − 8. Reassembly is latest-wins per channel. The chunk header
is transport framing, not part of the tether protocol (reference impls:
`controller/src/chunks.ts`, `crates/tetherd/src/webrtc.rs`).

On the **LAN WebSocket** there is only the one socket: file messages share it
with everything else, one protocol message per binary frame.

H.264 `FrameData` payloads are Annex B access units (4-byte start codes,
SPS/PPS in-band on keyframes); each payload is one complete, independently
parseable access unit, and keyframes recur every ~2 s.

### System audio (Phase 10)

System audio is a real WebRTC **Opus** media track (48 kHz stereo, 20 ms
frames), not a protocol message. The controller's offer always includes a
`recvonly` audio transceiver; the host attaches its track only when run with
`--audio` *and* the offer has an `m=audio` section (an older controller's
does not), and starts sending only after the controller authenticates. LAN
sessions never carry audio.

## Test vectors (Phase 7/11 messages)

Complete encoded messages, pinned in `host_info_and_file_transfer_vectors`
(`crates/tether-protocol`) and `controller/src/protocol.test.ts`:

| Message | Bytes (hex) |
|---|---|
| `HostInfo { os: 2, version: "1", name: "pc" }` | `09 00 00 00 0D 02 01 00 31 02 00 70 63` |
| `FileOffer { transfer_id: 1, size: 0x0102, name: "a" }` | `10 00 00 00 0E 01 00 00 00 02 01 00 00 00 00 00 00 01 00 61` |
| `FileAccept { transfer_id: 1, ok: true, reason: "" }` | `08 00 00 00 0F 01 00 00 00 01 00 00` |
| `FileChunk { transfer_id: 1, seq: 2, bytes: [AB] }` | `0A 00 00 00 10 01 00 00 00 02 00 00 00 AB` |
| `FileEnd { transfer_id: 1, status: 2 (error) }` | `06 00 00 00 11 01 00 00 00 02` |
| `FileRequest { transfer_id: 1, path: "/a" }` | `07 00 00 00 12 01 00 00 00 2F 61` |

## Signaling (JSON — not this protocol)

WebRTC setup goes through `tether-signal`: JSON text over WebSocket, serde
`tag = "type"`, snake_case (`crates/tether-signal/src/protocol.rs`, mirrored
in `controller/src/signaling.ts`, shapes pinned on both sides). Phase 8 added
an optional **`os`** string — `"macos"`, `"windows"` or `"linux"` — to
`register` and to each `PeerInfo` in `peers`. Hosts send it; the server
echoes it (truncated to 16 chars) so a controller can badge machines before
connecting. It is omitted when empty, and older peers neither send nor
expect it:

```json
{"type":"register","device_id":"pc","name":"PC","caps":{"can_host":true,"can_control":true},
 "auth":"…","pubkey":"…","sig":"…","os":"windows"}
{"type":"peers","peers":[{"device_id":"pc","name":"PC",
 "caps":{"can_host":true,"can_control":true},"os":"windows"}]}
```

## Reference implementations

- Rust: `crates/tether-protocol` (canonical)
- TypeScript: `controller/src/protocol.ts` (must round-trip byte-identically;
  both sides carry the same test vectors)
