# Phase 3 gate results — 2026-06-12

Verification environment: signal server + tetherd (release) on loopback,
controller in the embedded preview browser over the signaled WebRTC path.
WS-transport clipboard relay is covered by `tests/connection.rs`; the
chunked WebRTC path by `tests/webrtc_e2e.rs` (100 KB both directions).

| Gate criterion | Result | Evidence |
|---|---|---|
| Copy on host → paste on controller ≤2 s | **PASS** | `pbcopy` → exact text (Unicode included) at the controller in ~1.5 s. Auto-write fell back to the chip in the preview (its sandbox denies clipboard writes); chip = correct fallback behavior. |
| Copy on controller → Cmd+V pastes on host, first try | **PASS** | Synthetic paste flow → host pasteboard set → injected Cmd+V pasted into TextEdit → saved file matched exactly (emoji included). Ordering guarantee held. |
| No loops or churn | **PASS** | Zero publish/apply events across observed idle windows; self-write suppression and dedupe unit-tested. |
| 100 KB byte-identical; >256 KiB refused | **PASS** | 100,000 bytes exact in both live directions; cap refusal unit-tested both codecs + sync layer (refuse-and-log, never truncate). |
| Works over both transports | **PASS** | WS: integration test. WebRTC: live + e2e test. |

## What the verification flushed out (fixed during M4)

- **Data channels silently cap message sizes.** webrtc-rs refuses sends over
  ~64 KiB and silently drops inbound messages at its 64 KiB buffer boundary —
  and the failed send killed the host's ctl task, leaving a zombie session.
  Fix: a third data channel **`tether-bulk`** (reliable+ordered) carries
  oversized messages through the existing chunk framing; clipboard rides it
  in both directions. Chunk payload shrunk to **16 KiB**, the safe interop
  bound, for all chunked channels.
- **Cross-channel ordering**: clipboard (bulk) and the V keystroke (ctl) ride
  different SCTP streams, so large pastes delay the keystroke 150 ms
  (`LARGE_PASTE_BYTES`); small pastes keep the fast path. Residual race is
  noted in deferred.md with the proper fix (apply-ack or unified host queue).
- A locale footgun during testing (`pbcopy` with `LANG` unset writes mojibake
  to the pasteboard) initially looked like a transport bug; Tether was
  byte-faithful throughout.

## Remaining human checks

1. iPad Safari: chip-tap copy UX (insecure context = no auto-write), paste
   keystroke flow with the on-screen/hardware keyboard.
2. Ctrl+V from non-Mac keyboards pastes only if the host app treats Ctrl+V
   as paste (macOS wants Cmd+V) — remap decision deferred to Phase 4.
