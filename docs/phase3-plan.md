# Phase 3 Plan — clipboard sync

**Goal:** Copy on the host, paste on the controller device — and the reverse —
without thinking about it. Text only in Phase 3 (images/files/rich text
deferred).

---

## 1. The constraint that shapes everything: browser clipboard rules

Browsers gate clipboard access hard, and the controller is a web page served
over plain HTTP on the LAN (an *insecure context*), where
`navigator.clipboard` does not exist at all. What's actually available:

| Operation | Mechanism | Works when |
|---|---|---|
| Read controller clipboard | `paste` DOM event's `clipboardData` | Only during a real paste keystroke — which is exactly when we need it |
| Write controller clipboard | `navigator.clipboard.writeText` | Secure context (HTTPS/localhost) + permission |
| Write controller clipboard (fallback) | `execCommand("copy")` on a hidden textarea | Any context, but needs a user gesture (a click/tap) |

So the design leans on *intent moments* instead of background sync:

- **Controller → host** rides the paste keystroke: when the user hits
  Cmd/Ctrl+V over the canvas, we let the browser's paste action fire (instead
  of swallowing the keydown), harvest the text from the `paste` event, send it
  as `ClipboardData`, and *then* send the V keystroke — same reliable ordered
  channel, so the host has set its pasteboard before the paste keystroke
  lands. If no paste event arrives within ~75 ms (empty/denied clipboard),
  the V keystroke is sent anyway so the key isn't dead.
- **Host → controller** is push: tetherd polls `NSPasteboard.changeCount`
  (~600 ms), sends new text to the connected controller. The controller
  auto-writes it to the local clipboard when the context allows
  (localhost/HTTPS); otherwise a **clipboard chip** appears in the bar — one
  tap copies it locally via the textarea fallback (the tap is the gesture).
  On an iPad over LAN HTTP, the chip is the honest UX; it disappears wherever
  auto-write works.

## 2. Protocol addition (no version bump)

New message type — this is exactly what the unknown-type-skip mechanism was
built for; old peers ignore it safely:

```
0x05 — ClipboardData (both directions)
0  u8  kind      0 = UTF-8 text (only kind in Phase 3)
1  …   payload   the text, ≤ 256 KiB
```

Oversized clipboard content is *not synced* (logged host-side, status hint
controller-side) rather than silently truncated. The cap keeps clipboard
bursts from stalling input on the shared ctl channel.

Loop prevention: the host poller records the `changeCount` its own writes
produce and skips them; both sides drop a `ClipboardData` identical to the
last one they sent or applied.

## 3. Changes by component

**tether-protocol / protocol.ts** — `ClipboardData` encode/decode, byte
vectors pinned in both test suites (M1).

**tetherd** (M2):
- `clipboard.rs`: platform trait (`read_text` / `write_text` / `change_count`)
  + macOS impl via `objc2-app-kit` `NSPasteboard` (same objc2 family as the
  H.264 work). Poller thread with self-change suppression.
- `ServerState` grows `clipboard_out: watch::Receiver<Option<String>>` and
  `clipboard_in: mpsc::Sender<String>`; both the WS session and the WebRTC
  ctl channel relay `ClipboardData` in both directions (~30 lines each, same
  shape as input events).

**controller** (M3):
- `clipboard.ts`: the paste-intercept state machine (pure logic, unit-tested
  with fake events), auto-write probe, chip fallback with textarea copy.
- `input.ts`: Cmd/Ctrl+V special case routes through the paste flow.
- Bar UI: clipboard chip (only visible when there's unfetched host clipboard
  in a context where auto-write is unavailable).

## 4. Gate criteria (proposed)

1. Copy text in any host app → paste on the controller device ≤2 s later
   (auto where the context allows; one tap on the chip otherwise).
2. Copy on the controller device → Cmd/Ctrl+V into a host app pastes the
   controller's text, first try (ordering guarantee).
3. No loops or churn: host clipboard untouched while idle; copying the same
   text twice doesn't re-trigger.
4. 100 KB text survives byte-identically; >256 KiB is refused with a log,
   nothing corrupts.
5. Works over both transports (WS and WebRTC ctl channel).

## 5. Module order

1. **M1 — protocol**: `ClipboardData` both implementations + cross-pinned
   vectors.
2. **M2 — host**: NSPasteboard wrapper + poller + session plumbing;
   integration tests over fake channels; a pasteboard round-trip test
   (saves/restores the real pasteboard).
3. **M3 — controller**: paste flow + chip + auto-write; vitest for the state
   machine; live check over the preview.
4. **M4 — gate**: end-to-end verification both directions over both
   transports, gate write-up, deferred.md updates.

## 6. Risks / notes

- `paste` event delivery with a non-editable focused canvas varies by
  browser; if a browser won't deliver it, the fallback timer keeps Cmd+V
  functional (host-side clipboard just stays stale) and the fix is the
  hidden-textarea-focus pattern, which is a contained M3 change.
- NSPasteboard polling is the only watch mechanism macOS offers; 600 ms is
  the latency floor for host→controller.
- The clipboard now transits the signal-relayed DTLS channel — same trust
  model as keystrokes, nothing new to secure beyond Phase 2's notes.

Out of scope, logged to deferred.md on completion: images/files/RTF,
clipboard history, mobile long-press paste UX (Phase 4 territory).

---

**Status: awaiting architect approval. No Phase 3 code written.**
