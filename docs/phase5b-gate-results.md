# Phase 5b gate results — multi-monitor, multiple controllers, client cursor

Verified on a single-display machine (signal + tetherd on loopback, controller
in the preview browser). Multi-*display* switching is code-complete but can't
be exercised here (one display) — a documented human check, like TURN.

| Gate criterion | Result | Evidence |
|---|---|---|
| Controller lists host displays and switches the active one | **PASS (single-display) / code-complete (multi)** | `Displays`/`SelectDisplay` cross-pinned both impls; `select_display_routes_to_capture` integration test routes a pick to the capture thread; host switches live via `update_content_filter`. Real multi-display switch: human check. |
| `--max-controllers N`: N connect, N+1 refused; default 1 still single | **PASS** | `multiple_controllers_up_to_the_cap` (cap 2: two connect + fan-out frames + a third refused + a freed slot readmits); `second_concurrent_connection_is_rejected` covers the default. |
| Clean reconnect with the default cap | **PASS (critical fix)** | Live over WebRTC: connect → disconnect → reconnect streams again. Host log shows replace → old session ended (shutdown) → reconnect connected, **no "slots full"** — the bug the review caught (leaked permit self-rejecting the reconnect) is gone. |
| Trackpad client cursor tracks with no frame lag, zoom-correct | **PASS** | Live: re-seeds to the view center on first valid bounds, then `+40x`→`+100,+60` tracked exactly; hides on mode switch / disconnect. `zoom.test.ts` covers the mapping invariance; gesture re-seed unit-tested. |
| No regressions | **PASS** | 71 Rust + 100 TS tests green; no warnings. |

## Adversarial review — caught a critical permit leak

A multi-agent review (3 dimensions, each finding independently verified)
confirmed **13 issues; the headline was critical:**

- **CRITICAL — WebRTC permit leak on reconnect.** The replace-active-peer path
  closed the old peer but never ended its control task (the task's own data-
  channel `Arc` keeps its input channel alive, so the recv never returns), so
  its `OwnedSemaphorePermit` leaked. With the default `--max-controllers 1`,
  the reconnect's new offer was then **rejected as "slots full"** — i.e. the
  feature self-defeated after the first disconnect. **Fixed** with a per-peer
  shutdown watch the ctl task selects on; replace/teardown flips it, the task
  exits, the permit drops. Live-verified by a reconnect over WebRTC.
- 8 more fixed: acquire the permit *before* the auth gate (an over-cap peer no
  longer burns a pairing code / persists a token); input maps to the **active**
  display after a switch (was pinned to main); **release held buttons** when a
  controller disconnects (mid-drag disconnect can't strand a button — matters
  with multiple controllers); republish the display list on a *failed* switch;
  TS `Displays` decoder enforces `MAX_DISPLAYS` + rejects trailing bytes (Rust
  parity, bounds a malicious host); trackpad cursor hides on disconnect;
  `setMode` no longer desyncs when deferred mid-gesture.
- 3 findings were "no-fix" (correct as written). Deferred (documented below).

## Deferred (documented, multi-display / multi-controller refinements)

- **Concurrent-input interleaving**: with >1 controller all input merges into
  one host injector; two people moving the mouse at once interleaves (not
  arbitrated). Held-state is released on disconnect, but per-controller input
  partitioning (so A's held button can't mix with B's events) is future work.
- **Display-switch flapping**: any controller can switch the shared display;
  N controllers could fight over it. Last-wins; a view-owner/arbitration model
  is deferred.
- **Switch stalls capture briefly**: `switch_display` blocks the capture thread
  on a synchronous SCK reconfigure (rare, user-initiated). A pathological SCK
  hang would stall frames; bounding/offloading it is deferred.

## Remaining human checks

1. A real multi-monitor Mac: confirm enumeration names/sizes, that switching
   changes the streamed display for all controllers, and that input lands on
   the active display.
2. Two real devices paired to one host with `--max-controllers 2`: confirm
   simultaneous view + the shared-control feel.
