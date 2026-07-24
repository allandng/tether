# Phase 6 gate results — reachable from the internet

Built per [phase6-plan.md](phase6-plan.md). All four modules landed: TLS end to
end, a deployable signal + TURN stack, an authenticated signal directory, and
token expiry with a revocation CLI.

## What shipped

### M1 — TLS end to end

The controller hardcoded `` `ws://${signal}/ws` `` (`main.ts:200`). That was not
merely insecure, it was a hard blocker: the controller must be served over HTTPS
because pairing needs WebCrypto, and an `https://` page is forbidden by
mixed-content rules from opening a `ws://` socket. Serving the controller
properly would have broken signaling outright.

`signalUrl()` in `signaling.ts` now derives the scheme from the page's own
(`wss:` from `https:`), passes an explicit `ws://`/`wss://` URL through
untouched, and maps a pasted `http(s)://` URL onto the matching WebSocket
scheme. `tokio-tungstenite` gained the `native-tls` feature so the host can
reach a `wss://` server — chosen over a rustls feature deliberately, since
webrtc-rs already links rustls and two providers in one process is a runtime
panic waiting to happen. native-tls also means the system trust store, which is
what you want for a Let's Encrypt certificate on macOS and Windows alike.

### M2 — deploy stack

`deploy/` holds a compose file with Caddy, the signal server, and coturn, plus
an operator runbook. Two decisions worth recording:

- **Single origin.** Caddy serves the controller bundle at `/` and proxies
  `/ws` to the signal server on the same hostname, so the page, signaling, and
  pairing share one certificate. A phone opens `https://tether.example.com` and
  everything works from there.
- **coturn runs with `network_mode: host`**, which is required rather than
  convenient: Docker's userland proxy cannot forward a 16k-port UDP range, and
  coturn must bind the real public IP because it advertises it in relay
  candidates.

### M3 — authenticated signal directory

The `device_id` squatting hole, open since Phase 5 and documented in
`deferred.md`: any holder of the shared secret could register a live host's id
and evict it.

The server now issues a random 32-byte nonce on connect (`ServerMessage::
Challenge`), and `Register` carries an Ed25519 public key and a signature over
`"tether-signal-register-v1" \0 nonce \0 device_id`. `device_id -> pubkey` is
pinned on first use and persisted to `--identity-store`; a later registration
with a different key is refused with `IdentityMismatch` and the incumbent keeps
its registration.

Three details that carry the security weight:

- **The payload binds the device id, not just the nonce.** Otherwise a
  signature captured for one device could claim another on the same connection.
- **Signature verification happens before the pin comparison.** Comparing first
  would let a squatter probe whether an id is pinned without holding any key.
- **A pinned id can never drop back to no identity.** Without that, omitting
  the fields would be a one-line bypass of the whole mechanism.

Hosts must always sign. A controller registering an id nobody has claimed may
register bare, which is what lets a browser without WebCrypto Ed25519 (below
Safari 17 / Chrome 137 / Firefox 129) still connect.

The host derives its key from the existing `host.key` —
`SHA256(host_key || "tether-signal-identity-v1")` — so there is no new
persisted secret, and domain separation keeps the HMAC token key and the
signing key in different cryptographic domains.

### M4 — token expiry and revocation

Tokens were unexpiring bearer credentials in browser `localStorage`, and
`PairingAuth::revoke` existed but nothing outside the process called it.

- Tokens are now `"<expiry>.<mac>"` with a 90-day TTL. The expiry is covered by
  the MAC, so a controller cannot extend its own token by editing it. A
  pre-Phase-6 token has no expiry segment and is refused, which forces one
  re-pair on upgrade.
- `tetherd devices list` and `tetherd devices revoke <id>` operate on
  `paired.json`. The daemon re-reads the file when its mtime moves (one `stat`
  per auth attempt, and auth happens once per session), so a revocation takes
  effect at the device's next connect **without a restart** — without that,
  the CLI would silently do nothing until restart, which is worse than having
  no CLI.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | Phone on cellular reaches the host via `https://` + `wss://` | ⏸ **Human check** — needs the stack deployed on a real VPS with a real domain |
| 2 | Selected candidate pair is host/srflx (genuinely P2P) | ⏸ **Human check** — same |
| 3 | Forced `iceTransportPolicy: "relay"` still works | ⏸ **Human check** — same |
| 4 | A squatter cannot evict a registered host | ✅ `squatter_cannot_evict_a_registered_host` — the imposter is refused with `IdentityMismatch` and the incumbent still receives offers |
| 5 | Revocation takes effect without a daemon restart; expired tokens are refused | ✅ `external_edit_to_paired_json_is_picked_up`, `token_expires_and_the_expiry_cannot_be_edited` |
| 6 | No regressions; the LAN path still works over plain HTTP | ✅ full suites green |

**Criteria 1–3 remain the outstanding human checks.** They are the same WAN and
TURN verification debt carried since Phases 2 and 5, and this phase builds the
infrastructure that makes them testable rather than discharging them — the
deploy stack has not been stood up on real hardware. `docker compose config`
validates and the image builds locally, but a real `docker build` was not
possible in the development environment (the egress proxy refuses registry blob
fetches).

## Test coverage added

| Where | Tests |
|---|---|
| `tether-signal/src/identity.rs` | 9 unit tests: TOFU pinning, mismatch refusal, no-identity bypass, nonce/device binding, malformed input, file round trip, cross-implementation payload vector |
| `tether-signal/tests/relay.rs` | 4 integration tests: squatter eviction refused, pin bypass refused, captured-signature replay refused, unsigned host refused / unsigned controller allowed |
| `tetherd/src/auth.rs` | 3 unit tests: token expiry + forged expiry, external `paired.json` edit, identity-seed derivation |
| `controller/src/identity.test.ts` | 13 tests: payload vector, UTF-8 encoding, key stability, signature verification under WebCrypto, corrupt-key recovery, `signalUrl` scheme handling |
| `controller/src/signaling.test.ts` | Challenge + signed-register JSON vectors |

Totals: 33 Rust tests in the GUI-free crates (was 20), 114 controller tests
(was 100).

## Deferred from this phase

| Decision | Choice | Revisit when |
|---|---|---|
| **Trust on first use** | The first registration for a fresh `device_id` is taken on faith. A squatter who wins the race to a never-registered id pins their own key and locks the real host out. | If it matters, pre-seed the identity store from the host's public key out of band. |
| **Identity optional for fresh controllers** | Needed so a browser without WebCrypto Ed25519 can connect. A squatter can therefore claim an unpinned *controller* id — availability only; they still cannot pass a host's pairing gate. | When the Ed25519 baseline is safe to require outright. |
| **A lost browser identity is unrecoverable** | Clearing `localStorage` mints a new key, and the server refuses it for the pinned id. Recovery means removing the id from `--identity-store` by hand. | Phase 8, alongside the machine list — that is where a "forget this device" surface belongs. |
| **`tetherd devices` edits the file, not the daemon** | No IPC yet: the CLI writes `paired.json` and the daemon notices via mtime. It cannot arm a pairing code, which still needs `--pair` at startup. | Phase 9 — a background service has no terminal to print a code to, which forces a real control socket. |
| **Revocation does not cut an active session** | It takes effect at the next connect. | If immediate kick-off is wanted; the session layer would need to re-check mid-stream. |
