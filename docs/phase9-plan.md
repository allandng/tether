# Phase 9 Plan — installers and auto-start

**Scope:** turn tether from a source build into something you install. A
configuration file instead of argv, a local control channel so the daemon can
be driven without a terminal, signed installers for macOS and Windows, and
auto-start at login with the permission grants walked through rather than
documented.

---

## 1. Configuration file

Everything is clap flags today. That works for a foreground process and breaks
down for a service, which has no one to type them.

`~/.config/tether/config.toml` on macOS, `%APPDATA%\tether\config.toml` on
Windows, alongside the existing `host.key` and `paired.json`. CLI flags
override the file so development is unchanged; `Args::validate()` runs against
the merged result.

This also fixes a real leak. The LaunchAgent in the README passes `--secret` in
`ProgramArguments`, which puts it in the process's argv — readable by any local
user running `ps`. A 0600 config file is the fix, matching how `host.key` and
`paired.json` are already handled. Storing the secret in the macOS Keychain or
via Windows DPAPI is better still, but it is a refinement, not the fix; the
file permissions are what close the exposure.

## 2. A local control channel

**The blocker nobody notices until the first packaged build.** Arming a pairing
code today is `--pair`, which prints the code to stdout. A LaunchAgent
redirects stdout to `/tmp/tetherd.log`; a Windows Scheduled Task has no console
at all. Once tether is a background service, there is no way to pair a new
phone — you would have to stop the service, run it by hand, pair, and restart.

So packaging requires a way to talk to the running daemon:

- A Unix domain socket at `~/.config/tether/control.sock` (0600), or a named
  pipe on Windows.
- `tetherd pair` arms a code and prints it. `tetherd devices list` and
  `tetherd devices revoke <id>` drive the `PairingAuth` API that already
  exists. `tetherd status` reports connected controllers and the active
  display.

This subsumes the file-watch reload that Phase 6 chose as its minimal
revocation mechanism. If Phase 9 is close behind Phase 6, build the control
socket once, here, and let Phase 6 ship the simpler version knowing it gets
replaced.

## 3. macOS installer

A `.pkg` that installs `tetherd` to `/usr/local/bin`, writes the LaunchAgent to
`~/Library/LaunchAgents/com.tether.daemon.plist`, and bootstraps it.

**Notarization is not optional.** An unsigned or un-notarized binary is blocked
by Gatekeeper, and the workaround — right-click, Open, confirm — is not
something to put in setup instructions for a remote-control daemon. This needs
an Apple Developer Program membership at $99/year. Worth deciding before the
phase starts, because it is the only hard external cost in the roadmap.

**Permissions need a first-run flow, not a README section.** The two TCC grants
behave differently, and the difference matters:

- **Screen Recording** can be checked with `CGPreflightScreenCaptureAccess` and
  requested with `CGRequestScreenCaptureAccess`, which shows a real prompt.
  `tetherd` already exits with an explanatory error when it is missing.
- **Accessibility** does *not* prompt. Without it, macOS silently discards
  every injected event — the screen streams perfectly and nothing responds,
  which is the single most confusing failure mode this project has. Check
  `AXIsProcessTrusted()` at startup and, if false, open the pane directly with
  `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility`
  and wait for it to flip.

Both grants follow the **launching** binary, so moving from a terminal to a
LaunchAgent means re-granting in that context. The installer's first run is
exactly the right moment to walk through it.

## 4. Windows installer

A WiX MSI (or Inno Setup — WiX if the build should be reproducible in CI).
Installs the binary, registers the logon Scheduled Task from Phase 7, adds the
inbound firewall rule for the LAN transport, and creates a Start Menu entry.

Code signing is the equivalent problem to notarization. An unsigned installer
for a remote-control tool triggers SmartScreen prominently, and SmartScreen
reputation accrues per-certificate over time — an OV certificate is cheaper but
takes downloads to warm up, an EV certificate has reputation immediately and
costs more. Either way it is an annual cost alongside the Apple membership.

## 5. Uninstall

Both installers ship a working uninstall that unloads the LaunchAgent or
removes the Scheduled Task, deletes the binary, and **asks** before deleting
`~/.config/tether` — `host.key` and `paired.json` are the pairing state for
every device, and silently discarding them means re-pairing every phone.

Auto-update is explicitly **out of scope**. CRD had it because it shipped to
millions of machines; a self-hosted tool for two computers does not need an
update daemon, and one would be a meaningful new attack surface on a
remote-control host.

## 6. Module order

1. **M1 — config file:** TOML load, flag override, secret out of argv,
   documented precedence.
2. **M2 — control channel:** socket or named pipe, `pair` / `devices` /
   `status` subcommands.
3. **M3 — permission flow:** preflight both grants, request Screen Recording,
   deep-link Accessibility, re-check without restarting.
4. **M4 — macOS `.pkg`:** build, sign, notarize, LaunchAgent, uninstall.
5. **M5 — Windows MSI:** build, sign, Scheduled Task, firewall rule,
   uninstall.
6. **M6 — gate.**

## 7. Gate criteria (proposed)

1. On a machine that has never built tether, the `.pkg` installs it and it
   starts at login. Same for the MSI on a clean Windows machine.
2. Neither installer trips Gatekeeper or SmartScreen.
3. First run walks through both macOS grants, including Accessibility, and
   detects them without a restart.
4. `tetherd pair` arms a code and prints it while the daemon runs as a
   background service, with no terminal attached to the daemon.
5. `ps aux` (macOS) and Task Manager's command line column (Windows) show no
   secret; the config file is 0600 and owned by the user.
6. Reboot brings the host back and a phone reconnects with no manual step.
7. Uninstall removes the service and binary and leaves pairing state alone
   unless asked.

## 8. Risks

- **Signing costs are real and recurring:** ~$99/year for Apple, plus a Windows
  certificate. There is no way to ship a credible remote-control installer
  without both. Decide before M4.
- **Notarization is a CI problem, not just a build problem.** It needs
  credentials in the build environment and a network round trip. Expect the
  first attempt to fail on entitlements.
- **TCC in a packaged context** may behave differently from the terminal-launched
  binary everything has been tested with so far. Criterion 3 is deliberately
  about a clean machine, because a machine that has already granted a terminal
  will mask the problem.
- **The control socket is a privilege boundary.** It arms pairing codes, so
  anything that can write to it can authorise a new device. 0600 and an
  ownership check on connect, in the same posture `auth.rs` already applies to
  `host.key`.

---

**Status: planned, not started. Depends on Phase 7 for the Windows half; the
macOS half depends only on Phase 6.**
