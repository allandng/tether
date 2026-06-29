# Phase 5 gate results — secure-over-internet slice

Scope: device **pairing auth**, **TURN** config, **adaptive bitrate**. Verified
on one machine (signal server + tetherd on loopback, controller in the preview
browser, which is a secure context on localhost so WebCrypto works). TURN's
live relay traversal needs real coturn + a NAT path — a documented human check.

| Gate criterion | Result | Evidence |
|---|---|---|
| Pair with a one-time code, then reconnect with a token; unpaired refused | **PASS** | Live over WebRTC (`--require-pairing`): code `9F8A-7460` → paired → 29 fps; token stored; reconnect needs no code. WS path covered by the `pairing_lifecycle` integration test. |
| Revoking a device drops it; can't re-auth | **PASS** | `pairing_lifecycle` test: allowlist removal → token rejected. `auth.rs` revocation unit test. |
| Wrong code is single-use + rate-limited | **PASS** | `wrong_pairing_code_is_single_use` (host closes after one bad attempt); `auth.rs` consume-on-attempt + escalating-lockout unit tests. |
| Relay-MITM stand-in (fingerprint mismatch) fails pairing | **PASS** | `fingerprint_mismatch_fails_pairing_mitm_regression` — a controller proof over a different channel binding is rejected. |
| TURN credentials mint in coturn's exact format; both ends carry the advertised iceServers | **PASS (format/plumbing)** | Live: the signal server emitted `username:"<abs-expiry>:ipad"`, 28-char base64 credential over the wire; host + controller both apply `iceServers`. **Live relay traversal: human check** (needs coturn + NAT). |
| Bitrate adapts to send-buffer backpressure (AIMD), bounded | **PASS (logic)** | `adaptive.rs` AIMD unit tests (decrease/increase/clamp/hold); `set_bitrate` on a live VT session verified to keep producing valid access units. Live behavior under real WAN congestion: human check. |

Totals: **69 Rust + 95 TypeScript tests**, 0 failures (one in-process WebRTC
e2e is load-sensitive under fully-concurrent `cargo test` and passes reliably
in isolation).

## Adversarial security review — found and fixed a critical hole

A multi-agent review (4 dimensions, each finding independently verified)
confirmed **16 issues; the headline was critical:**

- **CRITICAL — WebRTC media + bulk channels bypassed the auth gate.** Only the
  `tether-ctl` channel ran the pairing/token check; `tether-media` and
  `tether-bulk` pumped the screen and accepted clipboard writes independently.
  An unpaired peer that merely knew the signal `--secret` could open those
  channels and receive the full screen + read/write the host clipboard with no
  token or pairing — defeating the whole phase. **Fixed:** a shared auth gate
  holds the media/bulk pumps closed until the ctl channel authenticates, drops
  inbound bulk clipboard until then, and tears down the *entire* peer
  connection on auth failure. A regression test asserts the media channel
  stays silent before authentication.
- Plus 10 more fixed: single-session lock made cross-transport; escalating
  lockout backoff; AIMD loop exits promptly on channel close; TURN secret
  env-only (not a `ps`-visible flag); `set_bitrate(0)` guard; controller
  handshake watchdog, strict out-of-order auth handling, pairing-code phase
  guard, Crockford typo normalization, actionable WebCrypto error.
- 3 findings were positive "no-fix" confirmations (host applies relay creds;
  STUN-only fallback; Relaxed atomic ordering is correct). 2 low-priority
  polish items deferred (AIMD seed-from-last, skip-AIMD-for-JPEG).

This review is why the gate is trustworthy: the headline criterion ("unpaired
refused") was *false for two of three data channels* until the review caught it.

## Residual security limitations (documented, by design for the slice)

- Pairing token is a bearer credential in browser storage; theft = takeover
  until revoked (rotate by re-pairing). Same exposure class as the prior shared
  secret.
- The signal-server `--secret` is a coarse anti-spam gate; the real per-device
  authorization is the pairing token + allowlist.
- A malicious signal relay can still DoS/squat a device_id (availability), but
  the DTLS-fingerprint-bound pairing prevents it from impersonating/MITMing.

## Remaining human checks

1. Real coturn + a symmetric-NAT WAN path: confirm relay traversal and that
   adaptive bitrate converges sensibly under genuine congestion.
2. Run the signal server on trusted infrastructure (the relay sees SDP/ICE, not
   media; pairing resists an active MITM, but operating it honestly is still
   the recommended posture).
