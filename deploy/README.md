# Deploying the Tether signal + TURN stack

This directory stands up the public half of Tether on one small server: TLS,
the controller bundle, the signaling server, and a TURN relay, all on one
hostname. The host daemon (`tetherd`) stays on your Mac and is never exposed —
it dials out to the signal server.

Three containers, defined in `docker-compose.yml`:

| Service | Image | Role | Ports |
|---|---|---|---|
| `caddy` | `caddy:2-alpine` | TLS termination, serves the controller at `/`, proxies `/ws` | 80, 443 |
| `signal` | built from `Dockerfile.signal` | presence + SDP/ICE relay, TURN credential minting | none published |
| `coturn` | `coturn/coturn:4.6` | relay for symmetric-NAT pairs | 3478, 5349, 49152–65535 |

Everything lives behind one origin. The phone opens
`https://tether.example.com`, and the page, the signaling socket, and pairing
all share that origin and one automatically-renewed certificate. That single
decision is what removes the certificate warnings, the mixed-content blocks and
the port juggling that otherwise make this feel like a lab rig.

## What to run it on

One small VPS. A Hetzner CX22 (~€4/mo) or a $6 DigitalOcean droplet is more
than enough — the signal server carries no media and coturn is idle whenever
peer-to-peer works.

Explicitly **not** a managed PaaS. coturn needs a public IPv4 address it can
advertise as its own and a raw UDP range of roughly sixteen thousand ports;
Fly, Railway, Render and friends allocate neither. This is a rented Linux box
with Docker on it, not a platform deployment.

Budget bandwidth before you raise `--bitrate-kbps` on the host. When P2P fails
and media falls back to the relay, the VPS carries the whole stream in both
directions: at the 4 Mbps default that is roughly **1.8 GB per hour** of use.
Hetzner includes 20 TB of traffic, DigitalOcean 1 TB. Fine either way for a
person, worth knowing before you point the ceiling higher.

## Before the first start

**Create the DNS record first.** An A record for `TETHER_DOMAIN` must resolve
to the server's public IP *before* you start the stack. Caddy requests a
certificate on startup, and Let's Encrypt validates by connecting back to that
name over port 80. If the record is missing, or still pointing somewhere else,
the ACME challenge fails and Caddy retries with backoff — the most common way a
first setup stalls. Confirm with `dig +short tether.example.com` from somewhere
other than the server.

**Open the firewall.** In the provider's cloud firewall and in the host's own
(`ufw`, `nftables`), open:

| Port | Protocol | For |
|---|---|---|
| 80 | tcp | ACME HTTP challenge, redirect to HTTPS |
| 443 | tcp (and udp for HTTP/3) | the controller and `wss://` signaling |
| 3478 | udp **and** tcp | TURN |
| 5349 | tcp | TURN over TLS |
| 49152–65535 | udp | the relay's allocation range |

The UDP range is not optional and not negotiable down to something tidier: it
is what `min-port`/`max-port` in `turnserver.conf` declare, and coturn hands
out allocations from it. Because coturn runs with `network_mode: host`, Docker
does not publish these for you — the host firewall is the only thing in the
way.

## Setup

```sh
cp .env.example .env                          # then fill in all four values
cp turnserver.conf.example turnserver.conf    # then set static-auth-secret
```

Generate the two secrets separately, e.g. `openssl rand -hex 32` each. Set
`static-auth-secret` in `turnserver.conf` to exactly the same string as
`TETHER_TURN_SECRET` in `.env` — see the warning at the bottom of this file.

Build the controller bundle into `./site`, which Caddy serves as the site root:

```sh
cd ../controller && npm ci && npm run build && cp -r dist/* ../deploy/site/
```

`site/` is gitignored apart from its `.gitkeep`; the bundle is a build
artifact, not a checked-in file. Rebuild and re-copy it whenever the controller
changes — Caddy serves it from a read-only bind mount, so the files just need
to be on disk, no container restart required.

Then:

```sh
docker compose up -d --build
docker compose logs -f caddy    # watch the certificate get issued
```

The `signal` image builds from the repository root as its context, because the
binary is compiled from the workspace. Only `tether-signal` is built —
`tetherd` is macOS-only and is not, and cannot be, part of this image. On a
working checkout the build context includes `target/`, which is large and slow
to send to the daemon; build from a clean clone on the server, or add a
`.dockerignore` for it.

## Pointing a host at it

On the Mac, give `tetherd` the full URL — scheme included, `/ws` path included:

```sh
tetherd --signal wss://tether.example.com/ws --secret <TETHER_SECRET> \
        --codec h264 --bitrate-kbps 4000
```

In the controller, open `https://tether.example.com`, choose **Signaled**,
enter the same secret and the host's device id. The TURN credentials are minted
by the signal server at registration and handed to both sides — there is
nothing to configure per-device for the relay.

## Verifying the relay actually works

TURN is the part that silently does nothing until you need it, so test it
deliberately rather than discovering it during a real session. In
`chrome://webrtc-internals`, a normal session should show a selected candidate
pair of type **host** or **srflx** — genuinely peer-to-peer. Then force the
client to `iceTransportPolicy: "relay"` and confirm the session still works
with a **relay** pair. If the first works and the second does not, the problem
is almost always the one below.

## The mistake to check first

`static-auth-secret` in `turnserver.conf` must equal `TETHER_TURN_SECRET` in
`.env`. When they differ, everything looks healthy — the site loads, the
certificate is valid, hosts register, and peer-to-peer sessions connect fine.
Only the sessions that need the relay fail, with a generic ICE failure, because
coturn rejects credentials that were HMAC'd under a different secret. A
trailing space or a partially-pasted value produces exactly the same symptom.

## Notes on the shape of this stack

The signal server publishes no ports. It speaks plain `ws://` and is reachable
only from Caddy over the compose network; TLS terminates at Caddy and no
certificate handling happens in Rust.

coturn runs with `network_mode: host`, so it is not on the compose network and
the other services cannot resolve the name `coturn`. That is intended. The
signal server only *mints* relay credentials from the shared secret — it never
connects to the relay. The only things that talk to coturn are browsers and
host daemons, over the public address.

`turns:` on 5349 is declared but not advertised by default, because TLS on the
relay needs a certificate; `turnserver.conf.example` explains how to reuse
Caddy's. Plain TURN over UDP and TCP on 3478 covers the traversal cases that
matter, and the media inside it is DTLS-encrypted end to end regardless.
