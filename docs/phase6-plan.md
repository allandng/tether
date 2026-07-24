# Phase 6 Plan — reachable from the internet

**Scope:** everything required to expose a host safely to the public internet
and connect to it from a phone on cellular. TLS end to end, a deployable
signal + TURN stack, an authenticated signal directory, and a real revocation
surface.

This phase is **blocking**: Phases 7–11 all assume a reachable host. It is also
the phase that finally makes the WAN and TURN verification debt from Phases 2
and 5 testable, so it discharges both.

---

## 1. TLS end to end (`wss://`)

Today the controller hardcodes its signal URL:

```ts
signalUrl: `ws://${signalInput.value.trim()}/ws`   // main.ts:200
```

`Args::signal_url()` on the host already accepts a full `ws://`/`wss://` URL,
so the host side needs almost nothing. The controller side cannot reach a TLS
signal server at all.

This is not merely a hardening item — it is a hard blocker, for a reason worth
spelling out. The controller **must** be served over HTTPS: pairing uses
WebCrypto and clipboard auto-write needs a secure context, both of which
browsers refuse on plain remote HTTP (already noted in the README). But an
`https://` page is forbidden by mixed-content rules from opening a `ws://`
socket. So the moment the controller is served properly, the current signaling
transport stops working. HTTPS and `wss://` have to land together.

- **Controller:** accept a scheme in the signal field, or infer it —
  `wss://` when `location.protocol === "https:"`, `ws://` otherwise. Keep
  accepting a bare `host:port` for LAN development.
- **Host:** verify `tokio-tungstenite` is built with a TLS feature
  (`rustls-tls-webpki-roots`); today it is a bare dependency, so `wss://` will
  fail to connect at runtime even though `signal_url()` happily returns one.
- **Signal server:** stays plain `ws://` bound to loopback. TLS terminates at
  the reverse proxy — no cert handling in Rust.

## 2. The deployment stack

**Recommendation: one small VPS.** Hetzner CX22 (~€4/mo) or a $6 DigitalOcean
droplet. Explicitly *not* a managed PaaS: coturn needs a public IPv4 and a wide
raw UDP range (49152–65535), which Fly, Railway, and friends will not allocate.

A `deploy/` directory with a `docker-compose.yml` running three services:

| Service | Role | Ports |
|---|---|---|
| `caddy` | TLS termination, serves the controller bundle at `/`, proxies `/ws` to the signal server | 80, 443 |
| `tether-signal` | presence + SDP/ICE relay, TURN credential minting | loopback only |
| `coturn` | relay for symmetric-NAT pairs | 3478 udp/tcp, 5349 tcp, 49152–65535 udp |

Two details that matter:

- **Single origin.** Caddy serves `controller/dist` at `/` and proxies `/ws` to
  the signal server on the same hostname. The phone opens
  `https://tether.example.com` and everything — page, signaling, pairing —
  shares one origin and one automatically-renewed certificate. This is what
  makes it feel like CRD instead of like a lab rig.
- **coturn needs `network_mode: host`.** Docker's userland proxy cannot
  usefully forward a 16k-port UDP range, and coturn must see real client
  addresses for its relay candidates.

Also worth budgeting: when P2P fails and media falls back to the relay, the VPS
carries the full stream. At the 4 Mbps default that is roughly 1.8 GB per hour
of use. Hetzner includes 20 TB, DigitalOcean 1 TB — fine either way, but it is
a real number to know before pointing `--bitrate-kbps` higher.

Ship a `deploy/README.md` with the DNS record, the firewall rules, and the
`turnserver.conf` whose `static-auth-secret` must equal `TETHER_TURN_SECRET`
(that pairing is already documented in the root README and is the single most
common misconfiguration).

## 3. Authenticated signal directory

Documented in `deferred.md` since Phase 5, and it becomes a live problem the
moment the server is public: any holder of the shared `--secret` can
`Register` someone else's `device_id` and evict the real host. The server
takes the newer registration unconditionally (`server.rs:191`) and sends the
displaced peer `ErrorCode::Replaced`. Pairing prevents impersonation of the
*media* session, but not this availability hit.

The shared secret stays as a coarse admission gate — it keeps strangers off the
server entirely. Layered under it, per-device identity:

- **Host identity key.** The host already persists `host.key` (32 random bytes,
  0600). Derive an Ed25519 signing key from it with domain separation —
  `SHA256(host_key || "tether-signal-identity-v1")` as the seed — rather than
  reusing the same bytes across HMAC and Ed25519. No new persisted state; one
  new dependency (`ed25519-dalek`).
- **Challenge/response on register.** The server sends a random nonce on
  connect; `Register` grows `pubkey` and `sig = Sign(identity_key, nonce ||
  device_id)`.
- **Trust on first use.** The server persists `device_id → pubkey` on first
  successful registration. A later registration for that `device_id` with a
  different key is refused with a new `ErrorCode::IdentityMismatch` — the
  squatting attempt fails instead of evicting the host.
- **Controllers register the same way** with a browser-generated key in
  `localStorage`, so the mechanism is uniform and the directory in Phase 8 can
  trust the identities it lists.

Wire-breaking on `Register`, so the Rust vectors, the TypeScript mirror in
`signaling.ts`, and the tests pinned across both change together — the same
coordinated edit Phase 5 did for `Registered { ice_servers }`.

## 4. Token lifetime and revocation

`PairingAuth` already has `revoke()` and `paired_devices()`, but nothing calls
them outside the process — revoking today means stopping the daemon, editing
`~/.config/tether/paired.json`, and starting it again. Tokens are long-lived
bearer credentials in browser `localStorage` with no expiry.

- `tetherd devices list` / `tetherd devices revoke <id>` as clap subcommands
  over the existing API. Because the running daemon holds the allowlist in
  memory, the daemon must reload `paired.json` when it changes (watch the file,
  or re-read on a `SIGHUP`) — otherwise the CLI silently does nothing until the
  next restart, which is worse than no CLI.
- **Token TTL.** Add an expiry to the minted token and a re-pair prompt in the
  controller when it lapses. Ninety days is a reasonable default; the value
  matters less than the fact that a stolen token stops working eventually.

## 5. Module order

1. **M1 — TLS:** controller scheme handling, host `wss://` support, Caddy
   config, controller bundle served from the same origin.
2. **M2 — deploy stack:** `deploy/` compose + coturn + `deploy/README.md`;
   stood up on a real VPS with a real DNS name.
3. **M3 — signal identity:** Ed25519 derivation, challenge/response,
   TOFU pinning, `IdentityMismatch`; cross-pinned JSON vectors.
4. **M4 — revocation:** `devices` subcommands, allowlist reload, token TTL.
5. **M5 — gate:** the live WAN and TURN checks below, gate write-up, README and
   `deferred.md` updates.

## 6. Gate criteria (proposed)

1. A phone on **cellular** (not the home LAN) opens
   `https://tether.example.com`, sees the host, pairs with a code, and controls
   it. This single check discharges the Phase 2 WAN debt.
2. `chrome://webrtc-internals` shows the selected candidate pair is **host or
   srflx** — i.e. genuinely peer-to-peer, not silently relayed.
3. With the client forced to `iceTransportPolicy: "relay"`, the session still
   works and the pair is **relay** — discharges the Phase 5 TURN debt and
   proves the coturn credential format end to end.
4. A second process registering the host's `device_id` with a different
   identity key is refused, and the live host is **not** evicted.
5. `tetherd devices revoke` drops that device at its next connect without a
   daemon restart; an expired token prompts for re-pairing.
6. No regressions: full Rust + TS suites green; the LAN WebSocket path still
   works unchanged over plain HTTP for development.

## 7. Risks

- **Wire-breaking `Register`** touches the Rust protocol, the TS mirror, and
  tetherd's signaling client together. Phase 5 did exactly this successfully;
  the mitigation is the same — change the pinned vectors first, let the tests
  fail loudly on either side, then fix both.
- **Exposing a remote-control daemon to the internet** is the threat model
  stepping up a level. The shared secret is now the only thing between the
  internet and the directory; pairing still gates media. Worth an adversarial
  review of the register path before it goes public, in the rhythm Phases 5 and
  5b used.
- **Certificate and DNS setup is the most likely place a first-time setup
  stalls.** Caddy's automatic HTTPS removes most of it, but the DNS record has
  to exist before the first start or the ACME challenge fails.
- `validate_bind_addr` deliberately refuses public addresses for the LAN
  transport. That guard stays exactly as it is — this phase exposes the
  *signal server*, never `tetherd`'s own listener.

---

**Status: planned, not started.**
