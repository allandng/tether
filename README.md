# Tether

[![CI](https://github.com/allandng/tether/actions/workflows/ci.yml/badge.svg)](https://github.com/allandng/tether/actions/workflows/ci.yml)

Tether is a self-hosted remote desktop that replaces Chrome Remote Desktop. It
lets you view and control a macOS or Windows computer from a browser or from an
installed phone app (PWA).

Media travels peer-to-peer over WebRTC, encrypted with DTLS. A small signal
server that you host introduces the two ends and carries no media. There are no
accounts and no cloud relay. Clipboard sync, file transfer in both directions,
system audio, multiple monitors and touch gestures are built in.

All 12 planned phases are built. A pass on real phones, a live internet
(WAN/TURN) run and installer signing certificates are still outstanding: see
[Status](#status).

## Quick start

This runs the signal server, a host and the controller on one computer. It
needs the [prerequisites](#prerequisites). A cold `cargo build --release` took
1 min 36 s on an Apple M3 (8 cores) with the crates already downloaded.

**Start Tether**, each command in its own terminal (macOS shown; Windows below):

1. Build the binaries:
   ```sh
   cargo build --release
   ```
2. Start the signal server. It listens on `127.0.0.1:7879`:
   ```sh
   ./target/release/tether-signal --secret test-secret
   ```
3. Start the host. `--pair` prints a pairing code:
   ```sh
   ./target/release/tetherd --signal 127.0.0.1:7879 --secret test-secret --codec h264 --pair
   ```
   On macOS, the first start asks for Screen Recording and Accessibility for
   your terminal app. Grant both, stop `tetherd` with Ctrl-C, and run this
   command again. See [Run a host on macOS](#run-a-host-on-macos).
4. Start the controller. It serves http://localhost:5173:
   ```sh
   cd controller && npm install && npm run dev
   ```

On Windows, run the same steps in PowerShell:

```powershell
cargo build --release
.\target\release\tether-signal.exe --secret test-secret          # terminal 1
.\target\release\tetherd.exe --signal 127.0.0.1:7879 --secret test-secret --codec h264 --pair   # terminal 2
cd controller; npm install; npm run dev                            # terminal 3
```

**Connect:**

1. Open http://localhost:5173.
2. Tap ⚙ Settings, set the signal server to `127.0.0.1:7879` and the secret to
   `test-secret`, then tap Save.
3. Tap the machine in the list and enter the pairing code that `tetherd`
   printed.

The pairing code is valid for 5 minutes. For a new one, run
`./target/release/tetherd pair` while the host is running.

To connect from a phone or another network, [deploy the signal
server](#deploy-the-signal-server). Browsers require HTTPS for the controller
anywhere except `localhost`.

## Prerequisites

### macOS host

The host binds ScreenCaptureKit, VideoToolbox and AppKit.

- macOS 26 (Tahoe) with Xcode 26. A transitive Metal dependency (`apple-metal`
  via `screencapturekit`) compiles against macOS 26 Metal 4 APIs, so an older
  SDK does not build. Install full Xcode: the Command Line Tools alone do not
  provide the matching SDK.
- Rust stable, edition 2024 (1.85+), from [rustup](https://rustup.rs).
- `cmake` and `nasm` for the JPEG encoder: `brew install cmake nasm`.

### Windows host (Windows 10 1703+ or 11)

- Visual Studio 2022+ Build Tools (MSVC, "Desktop development with C++"). Its
  bundled CMake works, or install CMake separately.
- Rust stable, MSVC toolchain, from [rustup](https://rustup.rs).
- NASM for the JPEG encoder: `winget install NASM.NASM`.

### Signal server and controller

- **Signal server (`tether-signal`):** builds and runs on macOS, Windows or
  Linux with the same Rust toolchain. It needs no Apple frameworks and carries
  no media, so any small machine can host it.
- **Controller (browser app):** Node 18+ (`brew install node`).

`cargo build --release` writes the binaries to
`target/release/{tetherd,tether-signal}`. Some examples below use `cargo run`
for clarity; use the release binaries for real use.

## Connect over your network

1. Start the signal server on a machine both devices can reach:
   ```sh
   cargo run --release -p tether-signal -- --bind 192.168.1.5 --secret <shared-secret>
   ```
2. Start the host (macOS or Windows):
   ```sh
   cargo run --release -p tetherd -- --signal 192.168.1.5:7879 --secret <shared-secret> \
       --codec h264 --bitrate-kbps 4000   # codec flags optional; default jpeg
   ```
3. In the controller, open ⚙ Settings and enter the signal server address and
   the secret. The address defaults to the page's own origin.
4. Tap the host in the machine list. It is listed under its device id: the
   hostname, or the value of `--device-id`.

Media flows peer-to-peer (DTLS-encrypted). The signal server only introduces
the peers. `--signal` takes `host:port` (used as `ws://host:port/ws`) or a full
`ws://` or `wss://` URL.

A controller opened from another device must be served over HTTPS. Use the
[deploy stack](#deploy-the-signal-server) for that.

## Run a host on macOS

`tetherd` needs two privacy (TCC) grants. They go to the app that launches it:
your terminal app, or `tetherd` itself under the LaunchAgent.

1. Start `tetherd`, for example as in the [Quick start](#quick-start). It asks
   for both grants itself: the system prompts appear, then System Settings
   opens the Accessibility pane.
2. Turn on **Screen Recording**: System Settings → Privacy & Security → Screen
   Recording.
3. Turn on **Accessibility**: System Settings → Privacy & Security →
   Accessibility.
4. Restart `tetherd`. macOS applies the Screen Recording grant at launch.
   Accessibility takes effect without a restart.
5. Check the grants:
   ```sh
   tetherd status
   ```
   The output includes:
   ```text
     permission  Screen Recording: granted
     permission  Accessibility: granted
     uploads     /Users/you/Downloads: ok
   ```

Connecting to a Mac whose display is asleep works. The screen stays black until
your first input wakes it.

### If a grant is missing

- **No Screen Recording:** nothing is captured. `tetherd` keeps running (its
  control socket stays up), retries capture in the background and streams
  nothing (resolution 0×0). `--audio` uses the same grant.
- **No Accessibility:** macOS silently discards injected input. The screen
  streams, but nothing responds.

### When macOS asks again

- `tetherd` prompts **once per binary and launch context** (launchd, a
  terminal, SSH, an app). It records this in
  `~/.config/tether/macos-permissions-prompted`, written before prompting, so a
  LaunchAgent that restarts never turns into a prompt loop.
- Reinstalling or upgrading asks once more.
- Grants follow the launching binary. Moving from your terminal to the
  LaunchAgent means granting both again for that context.
- TCC **cannot** be granted over a headless SSH session. Approve it once in a
  logged-in GUI session.
- Without a certificate, builds are signed ad hoc (`build-pkg.sh` gives them
  the identifier `com.tether.daemon`). macOS pins grants to the exact build, so
  **each new build must be granted again**. Only a Developer ID signature lets
  grants survive upgrades.

### Uploads and the Downloads folder

Uploads land in `~/Downloads`, which macOS also protects. Under the
LaunchAgent, do one of these:

- Allow `tetherd` in System Settings → Privacy & Security → Files & Folders
  (Downloads Folder).
- Set `upload_dir` in `config.toml` to a folder outside Desktop, Documents and
  Downloads.

`tetherd status` reports `NO ACCESS`, with this advice, when uploads would
fail.

## Run a host on Windows

Start `tetherd.exe` as in the [Quick start](#quick-start). Windows needs no
permission grants. To start it at sign-in, see [Install as a
service](#install-as-a-service).

Limits:

- UAC prompts, the lock screen and Ctrl+Alt+Del (the secure desktop) cannot be
  captured or controlled.
- Elevated windows ignore input unless `tetherd` runs elevated.
- A sleeping PC cannot be reached. Turn off sleep (Settings → System → Power)
  and stay signed in; the [roadmap](docs/roadmap.md) covers the locked-screen
  constraint.

## Pair a device

1. Arm a pairing code on the host. Either start the daemon with `--pair`, or
   ask a running daemon:
   ```sh
   tetherd pair
   ```
2. Connect from the controller and enter the code when it asks. The code is
   valid for 5 minutes.

The host then issues a per-device token, stored in the browser and valid for
90 days, so later connects need no code. The pairing proof is bound to the DTLS
fingerprints, so a malicious signal relay can't MITM it. The shared `--secret`
only gates the signal server; pairing authorizes each device.

### Who must pair

- Once any device is paired, a controller without a token must pair before it
  can connect.
- `--require-pairing` applies this rule before the first device is paired
  too.
- `--allow-unpaired` keeps the unpaired path open after pairing. It defeats
  pairing, logs a warning, and is meant for development on a LAN only.

### List and revoke devices

```sh
tetherd devices list
tetherd devices revoke <id>
```

A running daemon notices a revocation and refuses that device at its next
connect, with no restart. An active session is not cut off. The device must
pair again with a fresh code.

## Deploy the signal server

`deploy/` has a docker-compose stack: Caddy for TLS, the signal server and
coturn. It puts the controller and the signal endpoint on one hostname, so a
phone opens `https://tether.example.com`.

1. Follow the runbook in [deploy/README.md](deploy/README.md): VPS sizing, DNS,
   firewall rules, then `docker compose up -d --build`.
2. Point the host at it with `--signal wss://tether.example.com/ws`.
3. Open `https://tether.example.com` on the phone. See [Connect from a phone or
   tablet](#connect-from-a-phone-or-tablet).

### Pin device identities

Give the signal server `--identity-store <path>`. It pins each `device_id` to
the public key that first registered it, so nobody else holding the shared
secret can claim your host's id and evict it. Without the flag the pins are
in memory, and a restart reopens the window. The compose stack passes
`--identity-store /data/identities.json`.

### Add a TURN relay

The signal server is STUN-only by default, so symmetric-NAT pairs won't
connect. The compose stack includes coturn. To run a relay yourself:

1. Install coturn: `brew install coturn` (macOS) or `apt install coturn`
   (Linux).
2. Write a minimal `/etc/turnserver.conf`:
   ```ini
   realm=relay.example.com
   use-auth-secret
   static-auth-secret=<S>          # == TETHER_TURN_SECRET
   listening-port=3478
   tls-listening-port=5349         # for turns:
   min-port=49152
   max-port=65535
   external-ip=<public-ip>         # if the relay is behind NAT
   ```
3. Open UDP/TCP 3478 (and 5349 for TLS) plus the `min-port`–`max-port` UDP
   range in the firewall.
4. Start the relay:
   ```sh
   turnserver -c /etc/turnserver.conf
   ```
5. Start the signal server with the same secret and the relay URLs:
   ```sh
   TETHER_TURN_SECRET=<S> cargo run --release -p tether-signal -- \
       --bind 0.0.0.0 --secret <shared-secret> \
       --turn-url 'turn:relay.example.com:3478?transport=udp' \
       --turn-url 'turns:relay.example.com:5349?transport=tcp'
   ```

The signal server mints short-lived credentials per registration:
`username=<expiry>:<id>`, `password=base64(HMAC-SHA1(secret, username))`. That
is coturn's `use-auth-secret` scheme, so coturn's `static-auth-secret` must
equal `TETHER_TURN_SECRET`. The coturn command-line equivalent of the config
lines is `--use-auth-secret --static-auth-secret <S>`.

### Serve the controller yourself

The compose stack serves the controller for you. To host it elsewhere, build
the static bundle and serve `dist/` from any static server or reverse proxy:

```sh
cd controller && npm install && npm run build        # → controller/dist/
npx vite preview --host                               # or: serve dist/ behind nginx/caddy
```

Serve it over **HTTPS** (or `http://localhost`) for anything real. Pairing uses
WebCrypto and clipboard auto-write needs a secure context; the browser blocks
both on plain remote HTTP.

## Connect from a phone or tablet

The phone needs the controller over HTTPS. Set up the [deploy
stack](#deploy-the-signal-server) first.

1. Open `https://tether.example.com` in Safari (iPhone, iPad) or Chrome.
2. Add it to the home screen: Share → Add to Home Screen in iOS Safari, or the
   install prompt in Chrome or Edge.
3. In the installed app, tap ⚙ Settings, enter the secret and tap Save. Leave
   the signal server field at its default, the page's own origin.
4. Tap a machine and enter the code from `tetherd pair`.

Set up and pair inside the installed app. It has its own storage, and Safari
may clear a browser tab's settings and pairings after a week unused.

### Phone and tablet controls

- **Session bar:** one row with ←, the status, ⌨ (on-screen keyboard), 🖱
  (pointer mode) and ⋯. Files 📁, audio, the display picker and ⚙ Settings open
  from ⋯. The auto-hide button ⤒ hides the bar after a few seconds; tap the tab
  at the top to bring it back.
- **Special-keys toolbar** above the on-screen keyboard: Esc, Tab, arrows,
  Home/End/PgUp/PgDn, Del, F1–F12, Paste, and sticky Ctrl/⌥/⌘/⇧ (Alt and Win on
  a Windows host). Tap a modifier to latch it for the next key; double-tap to
  lock it.
- **Gestures:** tap clicks, double-tap double-clicks, long-press then lift
  right-clicks, long-press then move drags, two fingers scroll, pinch zooms the
  local view.
- **Zoom:** the "1×" chip resets zoom.
- **Pointer mode:** in trackpad mode the controller draws a local cursor dot
  for zero-lag aiming.

## Use a session

- **Machine list:** every host on your signal server, with a platform badge
  and live online/offline state. Paired hosts connect in one tap.
- **Clipboard:** text syncs both ways. Pasting on the controller (Cmd/Ctrl+V,
  or Paste on a phone) sends this device's clipboard to the host, then the
  paste keystroke. Host copies arrive automatically where the browser allows
  it; otherwise tap 📋.
- **Files:** the 📁 drawer uploads files (or drop them on the screen) into the
  host's Downloads folder and downloads any host path. Set the folder with
  `--upload-dir` and the size limit with `--max-upload-mb` (default 4096 MiB).
  The host logs every transfer.
- **Audio:** start the host with `--audio` (opt-in) to stream its system audio
  over WebRTC. In the controller, tap 🔇 to unmute; it changes to 🔊. Works on
  Windows and macOS hosts. On macOS it uses the Screen Recording grant (no
  microphone permission). Not available over the LAN transport.
- **Keyboard:** when you control a Windows PC from a Mac, iPhone or iPad, Cmd
  acts as Ctrl (Cmd+C copies). Ctrl still works too.

### Multiple monitors and controllers

- `--max-controllers N` (default 1) lets several paired devices view and
  control at once, shared across both transports. Input from all of them
  interleaves into the host.
- If the host has multiple displays, a picker appears in the controller bar.
  Switching is shared: everyone sees the chosen display.

## Tune video quality

```sh
tetherd --signal wss://tether.example.com/ws --secret <shared-secret> --codec h264 --bitrate-kbps 4000
```

- Use `--codec h264` over WAN: about 2.5 Mbps at native resolution, against
  about 125 Mbps for JPEG. The default codec is `jpeg`.
- `--bitrate-kbps` (default 4000) sets the H.264 target bitrate.
- **Adaptive bitrate:** with `--codec h264`, the host adapts the encoder
  bitrate to WebRTC send-buffer pressure (AIMD between 600 kbps and the
  `--bitrate-kbps` ceiling).
- On a Mac, H.264 is capped at 4096×2304. Larger displays are streamed scaled
  down, because beyond that VideoToolbox drops to a slow software encoder.

## Connect over the LAN without a signal server

1. Start the host on its LAN address:
   ```sh
   cargo run --release -p tetherd -- --bind 192.168.1.10 --allow 192.168.1.20
   # --bind: this machine's LAN address (loopback/private only; 0.0.0.0 is refused)
   # --allow: the controller's IP (repeatable); all other peers are dropped
   # --port: optional, default 7878
   ```
2. Serve the controller:
   ```sh
   cd controller && npm install && npm run dev          # same machine, or:
   npm run dev -- --host                                # reachable from LAN/iPad
   ```
3. Below the machine list, open "Connect by address (LAN)" and enter
   `192.168.1.10:7878`. Or open the controller with `?host=192.168.1.10:7878`.
4. Click the canvas to give it focus. Keyboard and mouse are forwarded while it
   has focus.

The LAN transport can run next to WebRTC: pass `--bind`/`--allow` and
`--signal`/`--secret` together. `--bind` and `--allow` each require the other.
Audio is not sent over the LAN transport.

## Install as a service

`tetherd` and `tether-signal` are foreground processes; a service manager
keeps them running. `tetherd` must run in your logged-in GUI session, so each
platform starts it per user at sign-in.

### Windows: logon task

1. Build the binaries:
   ```powershell
   cargo build --release -p tetherd
   ```
2. Run the installer. It prompts for the secret:
   ```powershell
   powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1 -Signal wss://tether.example.com/ws
   ```
3. Print a pairing code:
   ```powershell
   & "$env:LOCALAPPDATA\Programs\Tether\tetherd.exe" pair     # prints a pairing code
   ```

The installer works per user, starts at sign-in and opens no console window:

- Copies `tetherd.exe` and `tetherd-agent.exe` to
  `%LOCALAPPDATA%\Programs\Tether`.
- Writes `%APPDATA%\tether\config.toml`. The secret goes in on stdin, never on
  a command line.
- Registers the "Tether Host" logon task. It runs `tetherd-agent.exe`, which
  keeps `tetherd` alive and logs to `%LOCALAPPDATA%\Tether\tetherd.log`.
- Starts the task now.

Other installer options: `-Codec h264|jpeg` (default `h264`), `-Audio`,
`-DeviceId <name>`, and `-LanPort <port>` (adds a firewall rule; needs an
elevated PowerShell).

- **Remove:** `packaging\windows\uninstall.ps1`. It asks before deleting
  pairing state.
- **Package:** `packaging\windows\build.ps1` makes a distributable zip,
  `dist\tether-host-windows-<version>.zip`. It signs the binaries when
  `TETHER_SIGN_PFX` and `TETHER_SIGN_PASSWORD` are set.

### macOS: installer package

**Build and install:**

1. Build the package:
   ```sh
   packaging/macos/build-pkg.sh
   ```
   It writes `dist/tether-host-macos-<version>.pkg`.
2. Open the `.pkg` and install it. It installs `tetherd` to `/usr/local/bin`
   and a per-user LaunchAgent.

**Configure and start:**

1. Write the config. It prompts for the secret:
   ```sh
   tetherd setup --signal wss://tether.example.com/ws
   ```
2. Start the LaunchAgent. The installer starts it only if `config.toml`
   already existed:
   ```sh
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.tether.daemon.plist
   ```
3. Grant Screen Recording and Accessibility when asked (see [Run a host on
   macOS](#run-a-host-on-macos)), then restart the agent:
   ```sh
   launchctl kickstart -k gui/$(id -u)/com.tether.daemon
   ```
4. Print a pairing code:
   ```sh
   tetherd pair
   ```

Package details:

- It is a `productbuild` archive. Installer refuses it on anything older than
  macOS 26 or on a CPU architecture other than the one it was built for.
- It is signed and notarized when `TETHER_APP_IDENTITY`,
  `TETHER_INSTALLER_IDENTITY` and `TETHER_NOTARY_PROFILE` are set.
- Logs go to `~/Library/Logs/Tether/tetherd.log`.
- `/usr/local/share/tether/uninstall.sh` removes it and asks before deleting
  pairing state.

### macOS: LaunchAgent by hand

The installer above sets this up for you; this is the manual equivalent. A
per-user **LaunchAgent** fits because `tetherd` needs the window server and the
TCC grants of your logged-in session.

1. Write the config with `tetherd setup --signal …`.
2. Save this as `~/Library/LaunchAgents/com.tether.daemon.plist`:
   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
     "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0"><dict>
     <key>Label</key><string>com.tether.daemon</string>
     <key>ProgramArguments</key>
     <array>
       <string>/usr/local/bin/tetherd</string>
       <!-- no arguments: settings (incl. the secret) come from
            ~/.config/tether/config.toml, written by `tetherd setup` -->
     </array>
     <key>RunAtLoad</key><true/>
     <!-- restart only after a failure, at most every 30 s: a clean exit stays
          down, and a missing permission is a slow retry, not a crash loop -->
     <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
     <key>ThrottleInterval</key><integer>30</integer>
     <key>StandardErrorPath</key><string>/tmp/tetherd.log</string>
   </dict></plist>
   ```
3. Load it:
   ```sh
   launchctl load ~/Library/LaunchAgents/com.tether.daemon.plist
   ```
4. Grant Screen Recording and Accessibility to the launchd context (`tetherd`
   prompts for them once), then restart it:
   ```sh
   launchctl kickstart -k gui/$(id -u)/com.tether.daemon
   ```

`launchctl stop com.tether.daemon` sends SIGTERM, which `tetherd` handles by
closing active sessions cleanly. `KeepAlive {SuccessfulExit=false}` keeps it
down after a clean exit, and `ThrottleInterval 30` limits restarts after a
failure to one every 30 s.

### Config file and control commands

```sh
tetherd setup --signal wss://tether.example.com/ws --codec h264 --audio   # write config.toml
tetherd pair                   # arm a pairing code on the running daemon and print it
tetherd status                 # transports, controllers, displays, grants
tetherd devices list           # paired devices
tetherd devices revoke <id>    # revoke one
```

- **Config file:** every daemon flag except `--pair` and `--config` can be set
  in `config.toml` in the state directory (`~/.config/tether` on macOS,
  `%APPDATA%\tether` on Windows). Keys are the flag names with underscores
  (`--bitrate-kbps` becomes `bitrate_kbps`). Command-line flags override the
  file. `--config <path>` reads another file.
- **`tetherd setup`** writes the file readable by you only and keeps keys it
  does not set. It takes `--signal`, `--secret` (prompted for when omitted),
  `--device-id`, `--codec` and `--audio`.
- **Permissions:** on macOS, `tetherd` refuses a `config.toml` with any group
  or world permission bits. Fix it with `chmod 600 ~/.config/tether/config.toml`.
- **Control channel:** `pair` and `status` talk to the running daemon over a
  local, owner-only channel. `devices` and `setup` work on the files directly.

Example `config.toml`:

```toml
signal = "wss://tether.example.com/ws"
secret = "…"
codec = "h264"
bitrate_kbps = 4000
audio = true
```

### Signal server

The signal server needs no GUI session or TCC grants. On Linux, run it under a
systemd unit or any process supervisor; on macOS, a LaunchDaemon works. The
compose stack in `deploy/` restarts it automatically.

## Develop and test

### Run the tests

> [!WARNING]
> On macOS, `cargo test` includes the real pasteboard round trip
> (`clipboard::macos::tests::pasteboard_round_trip`), which **overwrites your
> clipboard**. Run `cargo test -- --skip pasteboard_round_trip` to leave it
> alone.

```sh
cargo test                      # Rust workspace (protocol, daemon, e2e-with-fake-capture)
cd controller && npm test       # controller (protocol vectors, input mapping)
```

[CONTRIBUTING.md](CONTRIBUTING.md) lists the format and lint checks CI runs.

### Check real hardware

```sh
cargo run -p tetherd --example capture_smoke   # live capture fps check (macOS: needs Screen Recording)
cargo run --release -p tetherd --example h264_smoke  # capture + H.264 fps/cost
cargo run -p tetherd --example inject_smoke    # moves the cursor and reads it back (briefly takes your mouse)
cargo test -p tetherd -- --ignored             # real-hardware checks (below)
cargo test -p tetherd -- --list --ignored      # list them without running
```

The tests that touch real hardware are `#[ignore]`d and run only with
`--ignored`:

- **macOS audio:** ScreenCaptureKit audio flow and the Opus pipeline rate
  (`audio::macos::tests::real_*`). They play a sound.
- **macOS input:** F19 release on disconnect, Caps Lock HID sync, a volume key
  and the IOPM user-activity assertion (`input::macos::tests::*`). These press
  real keys and restore state afterwards. The IOPM test wakes the display.
- **macOS encode:** the zero-copy encode benchmark.
- **Windows:** the loopback audio device and the real clipboard.
- **All platforms:** the BGRA→I420 timing benchmark.

### Test the controller without a host

```sh
cargo run -p tetherd --example fake_host -- --port 7979 --log /tmp/fake_host.jsonl
cd controller && npm run dev     # then open http://localhost:5173/?host=127.0.0.1:7979
```

`fake_host` serves the LAN transport with a synthetic screen. It logs every
input event as a JSON line instead of injecting it: nothing is posted to the
OS, clipboard text is only logged, and uploads go to a temp dir. The iOS
Simulator pass used it on a machine whose real mouse and keyboard must not be
touched.

## Status

| Phase | Scope | State | Details |
|---|---|---|---|
| 1 | LAN MVP: capture, JPEG, mouse and keyboard | Done | [gate results](docs/phase1-gate-results.md) |
| 2 | WebRTC, signal server, hardware H.264 | Done | [gate results](docs/phase2-gate-results.md) |
| 3 | Clipboard sync | Done | [gate results](docs/phase3-gate-results.md) |
| 4 | Touch gestures, phone UI | Done; iPad pass pending | [gate results](docs/phase4-gate-results.md) |
| 5 | Pairing, TURN config, adaptive bitrate | Done; live relay run pending | [gate results](docs/phase5-gate-results.md) |
| 5b | Multi-monitor, multiple controllers, local cursor | Done; real multi-display switch pending | [gate results](docs/phase5b-gate-results.md) |
| 6 | `wss://`, deploy stack, identity pins, revocation CLI | Done; live WAN/TURN run pending | [gate results](docs/phase6-gate-results.md) |
| 7 | Windows host | Done | [gate results](docs/phase7-gate-results.md) |
| 8 | Machine list, PWA install | Done; phone pass pending | [gate results](docs/phase8-gate-results.md) |
| 9 | Config file, control channel, installers | Done; signing needs certificates | [gate results](docs/phase9-gate-results.md) |
| 10 | System audio (Windows; macOS in phase 12) | Done | [gate results](docs/phase10-gate-results.md) |
| 11 | File transfer | Done | [gate results](docs/phase11-gate-results.md) |
| 12 | macOS host completion, iPhone/iPad controller | Done; real-device pass pending | [gate results](docs/phase12-gate-results.md) |

CI builds and tests on macOS and Windows. The whole system is verified end to
end on a single machine, including connecting while the display is asleep
(input wakes it). Phase 12 ran the host hands-on on a real Mac and the
controller in the iOS Simulator.

### Still needs a person

- **Real devices:** an iPhone/iPad pass, and a display switch on real
  multi-monitor hardware.
- **Networks:** a two-device WAN run with live TURN relay traversal.
- **Distribution:** code-signing certificates for the Windows and macOS
  installers.

### Measured

- LAN, JPEG: 29 fps at native resolution, 38–57 ms from capture to render
  on one machine ([phase 1](docs/phase1-gate-results.md)).
- WebRTC, H.264: 2.5 Mbps at native resolution
  ([phase 2](docs/phase2-gate-results.md)).
- Windows host: 30 fps at 1080p, 12.5 ms encode
  ([phase 7](docs/phase7-gate-results.md)).

### Plans and decisions

- [Roadmap](docs/roadmap.md): the Chrome Remote Desktop parity plan (phases
  6–11) and the locked-screen constraint.
- [deferred.md](docs/deferred.md): corners cut on purpose.
- [architecture.md](docs/architecture.md): components, data flow and where
  state is persisted.
- [protocol.md](docs/protocol.md): the wire protocol.

## Repository layout

**Rust crates:**

- `crates/tether-protocol`: transport-agnostic wire protocol (shared types).
- `crates/tether-signal`: signaling server. Presence and SDP/ICE relay, no
  media.
- `crates/tetherd`: host daemon. Capture, encoding, input injection,
  clipboard, audio, file transfer and the control channel (macOS and Windows).

**Everything else:**

- `controller/`: browser-based viewer/controller and PWA (TypeScript + Vite).
- `packaging/`: Windows installer scripts and macOS `.pkg` build.
- `deploy/`: docker-compose stack with Caddy (TLS), the signal server and
  coturn.
- `docs/`: per-phase plans and gate results, protocol spec, deferred
  decisions.

## Reference: command-line flags

`tetherd --help` and `tether-signal --help` list these flags with their defaults.

### `tetherd`

| Flag | `config.toml` key | Default | Purpose |
|---|---|---|---|
| `--signal <addr>` | `signal` | none | Signal server: `host:port`, or a `ws://` / `wss://` URL |
| `--secret <secret>` | `secret` | none | Signal server's shared secret; required with `--signal` |
| `--device-id <id>` | `device_id` | hostname | Name in the machine list |
| `--stun <url>` | `stun` | `stun:stun.l.google.com:19302` | STUN server (repeatable) |
| `--codec <codec>` | `codec` | `jpeg` | `jpeg` or `h264` |
| `--bitrate-kbps <n>` | `bitrate_kbps` | `4000` | H.264 target and adaptive-bitrate ceiling |
| `--max-controllers <n>` | `max_controllers` | `1` | Simultaneous controllers, across both transports |
| `--pair` | none | off | Arm a 5-minute pairing code at startup |
| `--require-pairing` | `require_pairing` | off | Refuse unpaired controllers before the first pairing |
| `--allow-unpaired` | `allow_unpaired` | off | Keep unpaired access open after pairing (LAN development only) |
| `--upload-dir <path>` | `upload_dir` | Downloads folder | Where uploaded files land |
| `--max-upload-mb <n>` | `max_upload_mb` | `4096` | Largest upload, in MiB |
| `--audio` | `audio` | off | Stream system audio (WebRTC only) |
| `--bind <ip>` | `bind` | none | LAN transport: loopback or private address to listen on |
| `--port <n>` | `port` | `7878` | LAN transport: TCP port |
| `--allow <ip>` | `allow` (a list) | none | LAN transport: controller IP allowed to connect (repeatable) |
| `--config <path>` | none | `<state dir>/config.toml` | Read settings from another file |

### `tether-signal`

| Flag or variable | Default | Purpose |
|---|---|---|
| `--bind <ip>` | `127.0.0.1` | Address to listen on |
| `--port <n>` | `7879` | TCP port |
| `--secret <secret>` or `TETHER_SECRET` | required | Shared secret for registration; prefer the variable in deployments (a flag shows in `ps`) |
| `--stun-url <url>` | `stun:stun.l.google.com:19302` | STUN URL advertised to peers (repeatable) |
| `--turn-url <url>` | none | TURN/TURNS URL advertised to peers (repeatable) |
| `TETHER_TURN_SECRET` | none | coturn `static-auth-secret` for minting TURN credentials (environment only) |
| `--turn-ttl <seconds>` | `86400` | TURN credential lifetime |
| `--identity-store <path>` | in memory | File that persists device identity pins |
