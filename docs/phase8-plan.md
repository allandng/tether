# Phase 8 Plan — machine list and one-tap connect

**Scope:** replace the three-field connect bar with a list of your machines,
each showing whether it is online, connecting on a tap. Plus PWA packaging so
the controller installs to a phone home screen.

This is the largest remaining *usability* gap against Chrome Remote Desktop —
and the cheapest of the parity phases, because most of the data already flows.

---

## 1. The directory already exists

`tether-signal` maintains a device directory and broadcasts a full
`ServerMessage::Peers` snapshot on every join and leave (`server.rs:301`). The
controller already parses it: `signaling.ts:30` types the message,
`signaling.ts:73` dispatches it, and `webrtc.ts:153` throws it away with
`onPeers: () => {}`.

So the server work here is nearly nil. What is missing is entirely
controller-side: nothing subscribes to the directory, because the controller
only opens a signaling socket at the moment you press Connect, and closes it
once the data channels are up.

Three changes:

- **Connect to the directory at app open, not at session start.** The
  controller registers as a controller-capability peer on launch and stays
  registered, so the machine list is live. It needs the same backoff-reconnect
  treatment the media transport already has in `main.ts` — a phone will drop
  this socket every time the browser backgrounds it, and the list must recover
  silently rather than going permanently stale.
- **Filter to hosts.** `PeerInfo.caps.can_host` already distinguishes them, so
  controllers do not list each other.
- **Add `os` to `PeerInfo`.** A platform badge per machine is most of what
  makes a list feel like CRD's, and `HostInfo` from Phase 7 arrives too late —
  it only exists once a media session is up. The host reports its OS at
  `Register` instead. Additive with a serde default, so it is not wire-breaking
  the way Phase 6's identity fields are.

## 2. Saved machines

Today the controller stores a per-host device token in `localStorage`, keyed by
host. That is already the backbone of a machine list — it means the browser
knows which machines it has paired with, which is exactly the set worth
showing.

Promote it to a proper store: `{ device_id, name, os, token, last_seen }` per
machine. Cross-reference against the live `Peers` snapshot:

- Paired **and** present in the snapshot → online, tap to connect, no code.
- Paired but absent → offline, greyed, with `last_seen`.
- Present but not paired → offered as "new machine", tap starts the pairing
  flow with the code entry that already exists in `pairing.ts`.

Server address and secret move out of the connect bar into a settings screen
and are entered once. With the single-origin deployment from Phase 6, the
signal URL can even default to the origin the page was served from — which
means a fresh phone needs to type the secret and nothing else.

## 3. Phone-first layout

The current bar is a row of inputs and icon buttons that assumes a wide
viewport. Restructure into two views:

- **Machine list** — the landing view. One row per machine: name, platform
  badge, status dot. A settings affordance for the signal server, secret, and
  paired-device management.
- **Session view** — the existing canvas plus the controls that are already
  there (display picker, clipboard, keyboard, pointer mode, fullscreen), with a
  back action to the list.

`main.ts` currently builds the whole UI as one `innerHTML` template and wires it
imperatively. Two views is about the point where that stops being pleasant, but
it does not justify pulling in a framework — the project has deliberately stayed
dependency-light and this is still a small amount of DOM. Split it into two
render functions over a small explicit state object.

## 4. PWA packaging

A manifest, icons, and `display: "standalone"`, plus `apple-touch-icon` for
iOS. Add to Home Screen then gives a chromeless app that launches straight to
the machine list.

This also quietly closes a Phase 4 deferred item: the Fullscreen API is
iPad-only on iOS, so the ⛶ button hides on iPhone. Standalone display mode is
the documented workaround, and it applies on iPhone.

A service worker is only required for Chrome's Android install prompt, and
caching a remote-control app's shell buys little. Ship the manifest; add a
minimal worker only if Android installability turns out to need it.

## 5. Module order

1. **M1 — directory:** persistent signaling registration with reconnect,
   `onPeers` consumed, `os` added to `PeerInfo`.
2. **M2 — store:** saved-machines model, reconciliation against the live
   snapshot, settings screen.
3. **M3 — views:** machine list and session view, phone-first layout, pairing
   entered from a list row.
4. **M4 — PWA:** manifest, icons, standalone verification on iOS and Android.
5. **M5 — gate.**

## 6. Gate criteria (proposed)

1. Opening the controller shows every paired machine with a correct online or
   offline state, without connecting to any of them.
2. Tapping an online machine connects with no further input — no address, no
   secret, no device id.
3. Starting or stopping a host flips its dot within a second or two, with no
   page reload.
4. Backgrounding the phone browser for several minutes and returning restores
   a live list rather than a stale or empty one.
5. A never-before-seen host appears as a new machine and pairs from a list row.
6. Installed to a home screen, the controller launches standalone with no
   browser chrome, on both iOS and Android.
7. No regressions: the LAN WebSocket path still reaches a host by address, and
   the existing token, pairing, and display-picker behaviour is unchanged.

## 7. Risks

- **The directory becomes a presence service.** Controllers now hold a socket
  open for as long as the app is open. Trivial load for a personal deployment,
  but it makes the Phase 6 identity work load-bearing: an unauthenticated
  directory that also advertises presence is a nicer target than one that only
  relays. This phase should not ship before Phase 6's identity pinning does.
- **Mobile socket lifecycle** is the most likely source of bugs — iOS Safari
  is aggressive about suspending backgrounded tabs. Criterion 4 exists
  specifically to catch it.
- **Scope creep into a framework rewrite.** Two views and a list do not need
  one. If the imperative wiring gets genuinely unpleasant, that is a separate
  decision to make deliberately, not a thing to slip into this phase.

---

**Status: planned, not started. Depends on Phase 6 (identity), and reads better
with Phase 7 (`os` badges) but does not require it.**
