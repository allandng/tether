# Phase 2 gate results — 2026-06-12

Verification environment: single MacBook (3420×2214 native capture), signal
server + tetherd (release) on loopback, controller in the embedded preview
browser (Chromium, **no GPU** — relevant caveat below). The Rust e2e test
(`tests/webrtc_e2e.rs`) additionally proves the full stack headlessly:
signaling, loopback ICE, DTLS, both data channels, handshake, chunked frame
reassembly to a valid JPEG, and input round-trip.

| Gate criterion | Result | Evidence |
|---|---|---|
| Connect via signaling, no IP config beyond the signal server | **PASS** | Browser connected with `signal host:port`, `secret`, `target=mac` only. Offer/answer/trickle-ICE relayed; DTLS peer-to-peer. |
| Stream + input parity with Phase 1 over data channels | **PASS** | JPEG over WebRTC: 30 fps at 3420×2214, ~43 ms capture→render. Input events injected through the ctl channel (the display-wake test below ran *through* it). e2e test pins handshake/frames/input. |
| Clean reconnect without restarting tetherd or signal server | **PASS** | New offers replace the active peer session (32 controller sessions against one daemon run during testing). Signal server killed mid-run: tetherd re-registered with 1s→2s→4s backoff; browser auto-reconnected and streamed again. |
| Wrong secret / non-host target refused at signaling | **PASS** | Live: bad secret → refused, closed, never connects (and retry keeps failing). Integration tests: `bad_auth` close, `target_not_host`, `unknown_target`, `not_registered`. |
| H.264 ≤ 3 Mbps at usable quality, hardware decode | **PASS (encode + bandwidth); decode caveat** | Live native-res measurement: **28.7 fps, 2.54 Mbps** at the 2500 kbps target, 31.8 ms hardware encode (~50× less bandwidth than JPEG). Decoded and rendered in the browser via WebCodecs at 29 fps. Caveat: the preview browser has no GPU, so decode ran in software at ~260 ms latency, bounded by the keyframe-resync backlog guard. Real Safari/Chrome hardware-decode — confirm on the iPad. |

## Bonus verified behavior (not in the gate, load-bearing for the product)

- **Connect to a dark Mac**: tetherd starts and registers while the display
  is asleep (capture retries in the background); a controller connects and
  gets `Resolution 0×0`, no frames.
- **Remote input wakes the display**: the injector declares
  `IOPMAssertionDeclareUserActivity`; first mouse move from the controller
  woke the sleeping display, capture started, and the live session picked up
  mid-stream — 30 fps within ~3 s of the first input. This was verified over
  the signaled WebRTC path end to end.

## Operational notes

- H.264 delta frames tolerate **no** loss after encoding, so the media data
  channel is reliable+ordered and frame dropping happens only *before* the
  encoder (capture latest-wins) — plus a 2 s keyframe interval as self-heal.
  Chunking (64 KiB, 8-byte header) remains for the message-size cap.
- The decoder resyncs at the next keyframe if its queue exceeds 8 frames,
  trading a ≤2 s freeze for bounded latency on slow (software) decoders.
- `--codec jpeg` (default) vs `--codec h264 --bitrate-kbps N`: JPEG remains
  the LAN workhorse; H.264 is for WAN. Codec negotiation per controller is
  deferred (a controller without WebCodecs shows nothing in h264 mode).

## Remaining human checks

1. Real WAN run: Mac at home, controller on another network, self-hosted
   signal server (or SSH tunnel) — confirm STUN traversal on your NAT.
2. iPad Safari: WebCodecs H.264 hardware decode + subjective latency.
3. Scroll/input feel over WAN latency.
