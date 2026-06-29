# Phase 5 Plan — secure-over-internet slice

**Scope (architect-chosen):** device **pairing auth** (replacing the single
shared secret), **TURN** configuration, and **adaptive bitrate**. Deferred to a
later Phase 5 pass: multi-monitor, client-drawn cursor, multiple controllers.

TURN ships **code-complete with a documented real-NAT verification step** — it
can't be gate-verified on a single machine (needs a relay + a symmetric-NAT
path). Pairing and adaptive bitrate are fully verifiable on loopback.

Design was de-risked with a research+security workflow; the pairing scheme
below is the *hardened* version from an adversarial security review, not the
naive first draft.

---

## 1. Device pairing auth (the security-critical piece)

Today auth is one shared `--secret` known to every device: anyone who has it
can connect to any host and even take over a session. We replace that with
per-device pairing the host controls and can revoke.

### Trust model
The signal server relays SDP/ICE and is **semi-trusted** — it may be malicious
or logged. The naive "pairing rides inside DTLS so the relay can't see it" is
**false against an active relay**, which can double-DTLS-MITM by rewriting the
DTLS fingerprints in the SDP it forwards. So the pairing proof is **bound to
the negotiated DTLS channel**, which makes that MITM fail.

### Scheme
- **Host identity:** on first run, `host_key = 32 random bytes` →
  `~/.config/tether/host.key` (file 0600, dir 0700, atomic write, ownership +
  perms checked on load, hard-fail if RNG fails). Never transmitted.
- **Allowlist:** persisted `paired.json` (0600): `device_id → {name, paired_at}`.
  Removing an entry revokes — even though the HMAC still verifies, the
  membership check fails.
- **Pairing code:** operator-armed on the host ("pair a device"). 8-char
  Crockford base32 (~40 bits) from `getrandom`, rejection-sampled, displayed
  grouped `XXXX-XXXX`, **5-min TTL, one outstanding, single-use, consumed on
  first *attempt*** (a wrong guess invalidates it). Global attempt budget:
  after 3 fails, 60-s cooldown. (Consume-on-attempt is the load-bearing
  control; entropy is the backstop.)
- **Channel binding:** `chan = SHA256(sorted(localFp, remoteFp))` where the
  fingerprints are the SHA-256 DTLS fingerprints each peer reads from its own
  local + received-remote SDP. Honest case: both ends derive the same `chan`.
  Under a relay MITM the two fingerprint pairs differ → `chan` differs →
  pairing fails. For the LAN WebSocket transport (no relay, direct TCP) `chan`
  is a fixed constant — the MITM vector doesn't exist there.
- **Pairing handshake** (ctl channel, after Hello, before Resolution):
  controller → `PairRequest { device_id (16 random bytes hex), name,
  proof = HMAC_SHA256(code, chan) }`. Host recomputes with its own `code` and
  `chan`, constant-time compares (`subtle`, on raw bytes). Success → add to
  allowlist, `token = base64(HMAC_SHA256(host_key, device_id || ":" ||
  paired_at))`, reply `PairResult { ok, token }`. Failure → consume code,
  bump counter, close.
- **Authenticated connect:** controller → `Auth { device_id, token }`. Host
  recomputes the token from the allowlist's `paired_at`, ct-compares **and**
  checks allowlist membership. Pass → stream. Fail → `AuthResult { ok:false }`,
  close, controller offers to pair.
- **Downgrade closure:** once the allowlist is non-empty (or `--require-pairing`),
  unauthenticated sessions are refused on **both** transports. `--allow-unpaired`
  is the only escape hatch, default off, for LAN/dev.

### Documented residual limitations
- Token is a bearer credential stored in the browser (same exposure class as
  today's `--secret` in localStorage); theft = takeover until revoked; rotate
  by re-pairing (the `paired_at` binding makes re-pair yield a new token).
- A malicious relay can still DoS (drop/replay) and squat a `device_id` at the
  signal directory; pairing closes *impersonation/takeover*, not availability.

### Layout
- `crates/tether-protocol`: new messages `0x07 PairRequest`, `0x08 PairResult`,
  `0x09 Auth`, `0x0A AuthResult` (length-prefixed; old peers skip via the
  existing `Unknown` path).
- `crates/tetherd/src/auth.rs`: host_key + allowlist persistence, code
  gen/verify (consume-on-attempt, lockout), token mint/verify, channel-binding
  hash, Crockford base32 — all pure and unit-tested.
- ctl handshake gate factored to run on **both** transports (session.rs +
  webrtc.rs), mirroring `validate_controller_hello`.
- Controller (M2): `pairing.ts` state machine mirroring proof computation;
  per-host token storage; pair screen.

New deps (tetherd): `hmac`, `sha2`, `subtle`, `getrandom` (base32 + config dir
hand-rolled to keep the surface small).

## 2. TURN configuration

- coturn REST ephemeral creds: `username = "<unix_expiry>:<userid>"`,
  `credential = base64(HMAC_SHA1(static_secret, username))` — **SHA1**,
  standard base64 of raw bytes, **absolute expiry** (now+ttl), not a duration.
- Signal server gains `--turn-url` (repeatable), `--turn-secret`, `--turn-ttl`,
  `--stun-url`; mints a fresh credential per registration keyed to `device_id`.
- `ServerMessage::Registered` changes from a unit variant to
  `Registered { ice_servers: Vec<IceServer> }` (STUN entries omit
  username/credential). **Wire-breaking** — update the Rust vectors, the TS
  mirror + tests, and tetherd's host-side signaling client together.
- Controller applies the received `iceServers` to its `RTCPeerConnection`
  (replacing the hardcoded STUN); tetherd does the same on its side (relay
  needs TURN advertised to *both* peers).
- New deps (tether-signal): `hmac`, `sha1`, `base64`.
- **Verification:** config plumbing + credential format unit-tested; live
  relay traversal documented as needing real coturn + NAT.

## 3. Adaptive bitrate

- `VtH264Encoder::set_bitrate(kbps)` updates `kVTCompressionPropertyKey_AverageBitRate`
  (+ `DataRateLimits` hard ceiling) on the **live** session via
  `VTSessionSetProperty` — confirmed safe between `encode_frame` calls on the
  capture thread; takes effect on subsequent frames.
- Congestion signal: data channels are SCTP (no REMB/transport-cc), so the
  reliable signal is `RTCDataChannel::buffered_amount()` trend. Control loop in
  the media pump samples every ~300 ms: **AIMD** — buffer above a high-water
  mark → multiplicative decrease (×0.7) toward a floor (~600 kbps); buffer low
  for K samples → additive increase (+500 kbps) toward the `--bitrate-kbps`
  ceiling. The AIMD step is a pure function (sample → new target), unit-tested.
- Only meaningful with `--codec h264`; JPEG ignores it.

## 4. Module order
1. **M1 — pairing auth**: protocol messages + `auth.rs` (token/code/binding,
   unit-tested) + both-transport ctl gate + downgrade closure.
2. **M2 — controller pairing UX**: `pairing.ts` + pair screen + per-host token
   storage; vitest for the proof/state machine.
3. **M3 — TURN**: signal-server minting + `Registered{ice_servers}` + both-end
   ICE config; cred-format + vector tests.
4. **M4 — adaptive bitrate**: `set_bitrate` + AIMD loop; control-loop tests.
5. **M5 — gate + adversarial review**: security-focused review of the built
   auth code, e2e (pair → connect → revoke; bitrate under simulated backpressure),
   gate write-up, deferred/README, push.

## 5. Gate criteria (proposed)
1. A controller pairs with a host using a one-time code, then reconnects with
   no code (token); an unpaired controller is refused.
2. Revoking a device (allowlist removal) drops it at next auth; it can't
   re-auth without re-pairing.
3. Wrong code is single-use; brute force is rate-limited/locked out.
4. A simulated fingerprint mismatch (relay-MITM stand-in) makes pairing fail.
5. TURN credentials mint in coturn's exact format; both ends carry the
   advertised `iceServers` (live relay = documented human check).
6. Under sustained send-buffer backpressure the encoder bitrate drops and
   recovers (AIMD), bounded by floor/ceiling.

---

**Status: scope approved ("secure-internet slice first"); building per the
gated module rhythm. Security design above reflects the adversarial review.**
