# Phase 9 gate results — installers and auto-start

Built per [phase9-plan.md](phase9-plan.md).

## What shipped

- **M1 config file.** Every flag can live in `config.toml` in the state dir
  (`%APPDATA%\tether` / `~/.config/tether`), same names. Precedence: command
  line > file > default (clap's value source decides what the CLI set).
  Unknown keys are rejected so typos fail loudly. On Unix a group/world-readable
  config is refused (it holds the secret), like `host.key`. `tetherd setup
  --signal … [--secret …]` writes keys owner-only, keeping the others, and
  prompts for the secret on stdin when it is omitted. `--config <file>` picks
  another file.
- **M2 control channel.** `control.rs`: a named pipe
  `\\.\pipe\tether-control-<user>` (DACL: this user + SYSTEM only, remote
  clients rejected, first-instance flag against squatting) or
  `~/.config/tether/control.sock` (0600, peer-uid checked). One JSON line each
  way. `tetherd pair` arms a code on the running daemon and prints it;
  `tetherd status` shows transports, controllers, displays, paired devices,
  audio. `tetherd devices list|revoke` still work on the file (the daemon
  notices).
- **M3 permission flow (macOS).** `macos_perms.rs`: at daemon start, preflight
  Screen Recording (request → system prompt) and Accessibility (deep-link the
  pane, then a watcher logs when the grant lands).
  *Reworked in Phase 12* ([details](phase12-gate-results.md#permissions-and-packaging)):
  - Accessibility and event posting are now requested too
    (`AXIsProcessTrustedWithOptions` with the prompt option,
    `CGRequestPostEventAccess`), so tetherd is listed in the pane.
  - Prompting happens once per binary fingerprint and launch context
    (launchd, ssh, terminal, app, job). It is recorded in
    `macos-permissions-prompted` before prompting, so a LaunchAgent restart
    can't become a prompt loop.
  - `tetherd status` shows both grants and upload-folder access.
- **M4 macOS `.pkg`.** `packaging/macos/build-pkg.sh` (pkgbuild; codesign with
  hardened runtime, productsign and notarytool + stapler when the identity /
  profile env vars are set), LaunchAgent plist with no arguments (config file
  only), postinstall that bootstraps it for the console user, `uninstall.sh`
  that asks before deleting pairing state. *Phase 12:*
  - tetherd is always re-signed as `com.tether.daemon` (ad hoc without a
    certificate) and carries an embedded Info.plist.
  - The component is wrapped by `productbuild` with
    `packaging/macos/distribution.xml` (macOS 26+, the build's architecture).
  - The LaunchAgent restarts only after a failure, at most every 30 s.
- **M5 Windows.** `packaging/windows/install.ps1` (per-user, no admin: copies
  binaries to `%LOCALAPPDATA%\Programs\Tether`, runs `tetherd setup` with the
  secret on stdin, registers the "Tether Host" logon task — interactive, no
  time limit, restart on failure — optional LAN firewall rule when elevated),
  `uninstall.ps1` (asks before deleting `%APPDATA%\tether`), `build.ps1`
  (release build, optional `signtool` signing, zip). The task runs
  `tetherd-agent.exe`, a GUI-subsystem launcher: no console window at sign-in,
  tetherd restarted with backoff if it dies, log rotated at 10 MB. An MSI
  (WiX) was not built: the PowerShell installer does the same work and is
  testable without extra tooling.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | Clean-machine install starts at login (`.pkg` and Windows installer) | ⏳ Windows installer dry-run (`-WhatIf`) walks every step; the agent was run directly and verified (hidden window, restart after kill, log). Not registered on this machine without the owner's go-ahead. `.pkg` unbuilt here (needs a Mac). *Phase 12:* the `.pkg` builds on a Mac (`dist/tether-host-macos-0.1.0.pkg`). Not yet installed. |
| 2 | Neither installer trips Gatekeeper / SmartScreen | ❌ Needs paid signing identities (Apple Developer ID, Windows OV/EV cert). The scripts sign when they're provided. |
| 3 | First run walks through both macOS grants, detected without restart | ⏳ Implemented and builds clean in macOS CI; needs a real Mac to exercise. Screen Recording still needs a restart (macOS applies it at launch) — logged as such. *Phase 12:* reworked and unit-tested on a Mac (above). The launchd-context prompts still need a real `.pkg` install. |
| 4 | `tetherd pair` arms and prints a code against a background daemon | ✅ Daemon started from the config file only; `tetherd pair` printed a code via the pipe, the daemon logged "pairing armed via the control channel". |
| 5 | No secret in the process command line; config 0600 | ✅ Windows: process command line shows only `--config …`; config written via `atomic_write`, lives under the owner-only `%APPDATA%`. Pipe ACL verified: SYSTEM + the user, inheritance off. Unix 0600 enforced in code. |
| 6 | Reboot brings the host back, a phone reconnects with no step | ⏳ Follows from the logon task + saved token; not rebooted here. |
| 7 | Uninstall removes service + binary, keeps pairing state unless asked | ⏳ Scripts written and parse-checked; not run here. |
