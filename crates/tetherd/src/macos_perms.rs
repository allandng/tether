//! First-run permission flow on macOS (Phase 9 M3).
//!
//! The grants fail differently, which is why they are checked up front
//! rather than left to a README:
//!
//! - **Screen Recording** can be preflighted and requested: macOS shows a
//!   real prompt. (Capture also fails loudly without it — see capture/macos.)
//! - **Accessibility** (and the event-posting grant listed under it): without
//!   it macOS silently drops every injected event — the screen streams and
//!   nothing responds, the most confusing failure this project has. A bare
//!   binary is not even *listed* in System Settings → Accessibility until it
//!   asks, so it asks (`AXIsProcessTrustedWithOptions` with the prompt option,
//!   `CGRequestPostEventAccess`), which registers it and shows the system
//!   alert; then the pane is opened and a watcher logs the moment the grant
//!   lands (no restart needed for input).
//!
//! **Asking happens at most once per installed binary and launch context.**
//! Under the LaunchAgent, tetherd may be (re)started many times while nobody
//! is at the screen; re-prompting on each start would drag System Settings to
//! the front over and over. A marker file in the state directory records each
//! (binary, launch context) that already asked; later starts only log. The
//! binary is identified by version, path, size and mtime: reinstalling or
//! upgrading changes it and asks once more — which is what an ad-hoc-signed
//! binary needs, since macOS ties its grants to the exact build.
//!
//! The launch context matters because TCC attributes a request to the
//! *responsible* process: started from Terminal (or over SSH), the prompts
//! name — and the grants go to — Terminal (or sshd), not tetherd. So a first
//! `tetherd --pair` from a terminal must not stop the LaunchAgent's later
//! start of the same binary from asking for itself ([`LaunchContext`]).
//!
//! The marker is written *before* prompting, falling back to
//! `~/Library/Caches/tether` and then a private temp directory when the state
//! directory is unwritable; if it can't be written anywhere, nothing is
//! prompted (a restart loop must never become a prompt loop) and the log
//! says what to grant by hand.

use std::path::{Path, PathBuf};
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use tracing::{debug, info, warn};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns a `Boolean` (unsigned char).
    fn AXIsProcessTrusted() -> u8;
    /// Returns a `Boolean` (unsigned char).
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Marker file (in the state directory) naming the binary that already asked.
const PROMPT_MARKER: &str = "macos-permissions-prompted";

pub fn screen_recording_granted() -> bool {
    // SAFETY: no arguments, no preconditions (macOS 10.15+).
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn accessibility_granted() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { AXIsProcessTrusted() != 0 }
}

fn post_event_granted() -> bool {
    // SAFETY: no arguments, no preconditions (macOS 10.15+).
    unsafe { CGPreflightPostEventAccess() }
}

/// The grants as observed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    pub screen_recording: bool,
    pub accessibility: bool,
    /// `CGPreflightPostEventAccess`: posting CGEvents (listed under
    /// Accessibility in System Settings).
    pub post_event: bool,
}

impl Grants {
    fn current() -> Self {
        Grants {
            screen_recording: screen_recording_granted(),
            accessibility: accessibility_granted(),
            post_event: post_event_granted(),
        }
    }

    fn input_ok(&self) -> bool {
        self.accessibility && self.post_event
    }
}

/// What the startup check does. Pure output of [`plan_prompts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptPlan {
    /// `CGRequestScreenCaptureAccess`: system alert + listed in the pane.
    pub request_screen_recording: bool,
    /// `AXIsProcessTrustedWithOptions(prompt)`: system alert + listed.
    pub request_accessibility: bool,
    /// `CGRequestPostEventAccess`: same, for the event-posting grant.
    pub request_post_event: bool,
    /// Bring System Settings → Accessibility to the front.
    pub open_accessibility_pane: bool,
    /// Record that this binary has asked, so later starts stay quiet.
    pub write_marker: bool,
}

/// Decide what to ask for. Prompts and the Settings pane happen only when
/// something is missing *and* this binary has not asked before.
pub fn plan_prompts(grants: Grants, already_prompted: bool) -> PromptPlan {
    let all_granted = grants.screen_recording && grants.input_ok();
    if all_granted || already_prompted {
        return PromptPlan::default();
    }
    PromptPlan {
        request_screen_recording: !grants.screen_recording,
        request_accessibility: !grants.accessibility,
        request_post_event: !grants.post_event,
        open_accessibility_pane: !grants.input_ok(),
        write_marker: true,
    }
}

/// Identifies this installed binary: a reinstall or upgrade changes it.
pub fn binary_fingerprint() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let Ok(exe) = std::env::current_exe() else {
        return version.to_owned();
    };
    let (len, mtime) = std::fs::metadata(&exe)
        .map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (m.len(), mtime)
        })
        .unwrap_or((0, 0));
    format!("{version} {len} {mtime} {}", exe.display())
}

/// Who TCC charges this process's permission requests to, as far as the
/// environment tells. Part of the prompt marker: each context asks once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchContext {
    /// Started by launchd (the LaunchAgent): tetherd is its own responsible
    /// process, so its requests list *tetherd* in the Privacy panes.
    Launchd,
    /// Started some other way (a terminal, SSH, an app): the requests — and
    /// any grant — belong to that responsible process. The string tells
    /// such contexts apart.
    Other(String),
}

impl LaunchContext {
    /// This process's context: [`launch_context`] over the real parent pid
    /// and environment.
    pub fn current() -> Self {
        // SAFETY: getppid has no preconditions and cannot fail.
        let ppid = unsafe { libc::getppid() };
        launch_context(ppid, |k| std::env::var(k).ok())
    }

    fn tag(&self) -> String {
        match self {
            LaunchContext::Launchd => "launchd".into(),
            // one marker line per context: no tabs or newlines inside it
            LaunchContext::Other(s) => s
                .chars()
                .map(|c| if c.is_control() { '_' } else { c })
                .collect(),
        }
    }
}

/// Classify the launch from the parent pid and the environment (`env`
/// looks a variable up). launchd is pid 1, and it is the direct parent of
/// every LaunchAgent program. Otherwise the best available names for the
/// responsible process: SSH (sshd), the terminal app (`TERM_PROGRAM`), or the
/// app/job launchd recorded in `XPC_SERVICE_NAME` — `application.<bundle
/// id>.<n>.<n>` for app-spawned processes (the per-launch numbers dropped),
/// a job label for a launchd job that went through a wrapper, `0` for none.
pub fn launch_context(ppid: i32, env: impl Fn(&str) -> Option<String>) -> LaunchContext {
    if ppid == 1 {
        return LaunchContext::Launchd;
    }
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if set("SSH_CONNECTION")
        .or_else(|| set("SSH_CLIENT"))
        .is_some()
    {
        return LaunchContext::Other("ssh".into());
    }
    if let Some(term) = set("TERM_PROGRAM") {
        return LaunchContext::Other(format!("terminal:{term}"));
    }
    match set("XPC_SERVICE_NAME") {
        Some(name) if name != "0" => {
            let other = match name.strip_prefix("application.") {
                Some(app) => {
                    let mut parts: Vec<&str> = app.split('.').collect();
                    while parts.len() > 1
                        && parts
                            .last()
                            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
                    {
                        parts.pop();
                    }
                    format!("app:{}", parts.join("."))
                }
                None => format!("job:{name}"),
            };
            LaunchContext::Other(other)
        }
        _ => LaunchContext::Other("other".into()),
    }
}

/// The marker line for this binary asking in this context.
pub fn marker_line(binary: &str, context: &LaunchContext) -> String {
    format!("{binary}\t{}", context.tag())
}

/// Has this (binary, context) already asked? `markers` are every place a
/// marker may have been written; any of them counts.
pub fn already_prompted(markers: &[PathBuf], line: &str) -> bool {
    markers
        .iter()
        .any(|m| std::fs::read_to_string(m).is_ok_and(|s| s.lines().any(|l| l == line)))
}

/// Record that this (binary, context) has asked, in the first of `markers`
/// that can be written; returns where. Lines for the same binary in other
/// contexts are kept; lines for other (older) binaries are dropped.
pub fn record_prompted(markers: &[PathBuf], binary: &str, line: &str) -> std::io::Result<PathBuf> {
    let mut last_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no marker location");
    for marker in markers {
        match write_marker(marker, binary, line) {
            Ok(()) => return Ok(marker.clone()),
            Err(e) => {
                debug!(error = %e, marker = %marker.display(), "permission marker not writable here");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

fn write_marker(marker: &Path, binary: &str, line: &str) -> std::io::Result<()> {
    if let Some(dir) = marker.parent() {
        private_dir(dir)?;
    }
    let same_binary = format!("{binary}\t");
    let mut lines: Vec<String> = std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with(&same_binary) && *l != line)
        .map(str::to_owned)
        .collect();
    lines.push(line.to_owned());
    std::fs::write(marker, lines.join("\n") + "\n")
}

/// Create `dir` (mode 0700) if needed, and refuse one that isn't a real
/// directory owned by us — the temp-dir fallback may be a shared `/tmp`,
/// where someone else could have planted the name.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a directory owned by this user",
        ));
    }
    Ok(())
}

/// Where the marker may live, in order of preference: the state directory,
/// then `~/Library/Caches/tether`, then a per-user temp directory (no HOME).
fn marker_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(dir) = crate::config::state_dir() {
        paths.push(dir.join(PROMPT_MARKER));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        paths.push(
            PathBuf::from(home)
                .join("Library/Caches/tether")
                .join(PROMPT_MARKER),
        );
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    paths.push(
        std::env::temp_dir()
            .join(format!("tether-{uid}"))
            .join(PROMPT_MARKER),
    );
    paths
}

fn request_accessibility_with_prompt() -> bool {
    // SAFETY: kAXTrustedCheckOptionPrompt is a valid CFString constant owned
    // by HIServices (get rule: we retain our own reference); the options
    // dictionary outlives the call.
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0
    }
}

/// Check the grants at daemon start; ask for what's missing (once per
/// installed binary and launch context — see the module docs) and explain the
/// rest in the log.
pub fn check() {
    let grants = Grants::current();
    let markers = marker_paths();
    let context = LaunchContext::current();
    let binary = binary_fingerprint();
    let line = marker_line(&binary, &context);
    let prompted = already_prompted(&markers, &line);
    let mut plan = plan_prompts(grants, prompted);
    debug!(?grants, ?context, prompted, ?plan, "macOS permission check");

    // Record *before* asking: a crash or kill mid-prompt must not turn into
    // a prompt on every restart, and neither may an unwritable marker.
    let mut recorded_at = None;
    if plan.write_marker {
        match record_prompted(&markers, &binary, &line) {
            Ok(at) => recorded_at = Some(at),
            Err(e) => {
                warn!(
                    error = %e,
                    "could not record the permission prompt anywhere (state directory, \
                     ~/Library/Caches/tether, temp directory); not prompting, so restarts \
                     can't keep re-prompting — grant the permissions below by hand"
                );
                plan = PromptPlan::default();
            }
        }
    }

    if !grants.screen_recording {
        warn!(
            "Screen Recording is not granted to this process: no screen can be captured. \
             Approve tetherd in System Settings → Privacy & Security → Screen Recording, \
             then restart it (`launchctl kickstart -k gui/$(id -u)/com.tether.daemon` for \
             the LaunchAgent) — macOS applies this grant at launch."
        );
    }
    if !grants.input_ok() {
        warn!(
            accessibility = grants.accessibility,
            post_event = grants.post_event,
            "Accessibility is not granted: macOS will silently ignore all remote mouse and \
             keyboard input until it is. Enable tetherd in System Settings → Privacy & \
             Security → Accessibility (no restart needed)."
        );
    }
    if (!grants.screen_recording || !grants.input_ok()) && prompted {
        let found = markers
            .iter()
            .find(|m| already_prompted(std::slice::from_ref(m), &line));
        info!(
            marker = %found.map(|m| m.display().to_string()).unwrap_or_default(),
            ?context,
            "not prompting again: this tetherd binary already asked for its permissions \
             in this launch context (delete the marker file to be asked on the next start)"
        );
    }
    if let Some(at) = &recorded_at {
        debug!(marker = %at.display(), ?context, "recorded the permission prompt");
    }

    // Asking first registers the binary, so it is listed when the pane opens.
    if plan.request_screen_recording {
        // SAFETY: no arguments; shows the system prompt (once per binary).
        let _ = unsafe { CGRequestScreenCaptureAccess() };
    }
    if plan.request_accessibility {
        let _ = request_accessibility_with_prompt();
    }
    if plan.request_post_event {
        // SAFETY: no arguments; shows the system prompt / lists the binary.
        let _ = unsafe { CGRequestPostEventAccess() };
    }
    if plan.open_accessibility_pane {
        let _ = std::process::Command::new("open")
            .arg(ACCESSIBILITY_PANE)
            .status();
    }

    if !grants.input_ok() {
        std::thread::Builder::new()
            .name("tether-ax-watch".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    if accessibility_granted() && post_event_granted() {
                        info!("Accessibility granted — remote input now works");
                        return;
                    }
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Grants = Grants {
        screen_recording: true,
        accessibility: true,
        post_event: true,
    };

    #[test]
    fn nothing_is_asked_when_everything_is_granted() {
        assert_eq!(plan_prompts(ALL, false), PromptPlan::default());
        assert_eq!(plan_prompts(ALL, true), PromptPlan::default());
    }

    #[test]
    fn missing_grants_are_asked_for_once_then_never_again() {
        let none = Grants {
            screen_recording: false,
            accessibility: false,
            post_event: false,
        };
        assert_eq!(
            plan_prompts(none, false),
            PromptPlan {
                request_screen_recording: true,
                request_accessibility: true,
                request_post_event: true,
                open_accessibility_pane: true,
                write_marker: true,
            }
        );
        // The crash-loop case: every later start is silent (log only).
        assert_eq!(plan_prompts(none, true), PromptPlan::default());
    }

    #[test]
    fn only_the_missing_grant_is_requested() {
        let no_screen = Grants {
            screen_recording: false,
            ..ALL
        };
        let p = plan_prompts(no_screen, false);
        assert!(p.request_screen_recording && p.write_marker);
        assert!(!p.request_accessibility && !p.request_post_event);
        assert!(
            !p.open_accessibility_pane,
            "input works: leave Settings alone"
        );

        let no_ax = Grants {
            accessibility: false,
            ..ALL
        };
        let p = plan_prompts(no_ax, false);
        assert!(p.request_accessibility && p.open_accessibility_pane && p.write_marker);
        assert!(!p.request_screen_recording && !p.request_post_event);

        let no_post = Grants {
            post_event: false,
            ..ALL
        };
        let p = plan_prompts(no_post, false);
        assert!(p.request_post_event && p.open_accessibility_pane);
        assert!(!p.request_accessibility);
    }

    /// A fresh scratch directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tether-perms-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    fn other(s: &str) -> LaunchContext {
        LaunchContext::Other(s.into())
    }

    #[test]
    fn launch_context_tells_launchd_from_terminals_and_ssh() {
        // launchd is the parent of every LaunchAgent program, whatever the env
        let agent = [("XPC_SERVICE_NAME", "com.tether.daemon")];
        assert_eq!(launch_context(1, env(&agent)), LaunchContext::Launchd);
        assert_eq!(launch_context(1, env(&[])), LaunchContext::Launchd);
        // the same binary run by hand: charged to the terminal or sshd
        let terminal = [
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("XPC_SERVICE_NAME", "0"),
        ];
        assert_eq!(
            launch_context(4242, env(&terminal)),
            other("terminal:Apple_Terminal")
        );
        let ssh = [
            ("SSH_CONNECTION", "10.0.0.2 50000 10.0.0.1 22"),
            ("TERM_PROGRAM", "iTerm.app"), // forwarded by some clients
        ];
        assert_eq!(launch_context(4242, env(&ssh)), other("ssh"));
        assert_eq!(
            launch_context(4242, env(&[("SSH_CLIENT", "10.0.0.2 50000 22")])),
            other("ssh")
        );
        // an app's child: the per-launch numbers don't make a new context
        let app = [(
            "XPC_SERVICE_NAME",
            "application.com.example.Shell.54187710.54187716",
        )];
        assert_eq!(
            launch_context(4242, env(&app)),
            other("app:com.example.Shell")
        );
        // a launchd job through a wrapper (tetherd is not its direct child)
        assert_eq!(
            launch_context(4242, env(&[("XPC_SERVICE_NAME", "com.tether.daemon")])),
            other("job:com.tether.daemon")
        );
        // nothing to go on
        assert_eq!(launch_context(4242, env(&[])), other("other"));
        assert_eq!(
            launch_context(
                4242,
                env(&[("XPC_SERVICE_NAME", "0"), ("TERM_PROGRAM", "")])
            ),
            other("other")
        );
        // a context string can't break the one-line-per-context marker
        assert_eq!(
            marker_line("1.0 1 2 /bin/t", &other("terminal:a\tb\nc")),
            "1.0 1 2 /bin/t\tterminal:a_b_c"
        );
    }

    #[test]
    fn a_terminal_run_does_not_silence_the_launch_agent() {
        let dir = scratch("context");
        let markers = vec![dir.join("state").join(PROMPT_MARKER)];
        let binary = binary_fingerprint();
        assert!(binary.starts_with(env!("CARGO_PKG_VERSION")));
        let terminal = marker_line(&binary, &other("terminal:Apple_Terminal"));
        let agent = marker_line(&binary, &LaunchContext::Launchd);

        // `tetherd --pair` from Terminal first: it asks (on Terminal's behalf)
        assert!(!already_prompted(&markers, &terminal), "no marker yet");
        record_prompted(&markers, &binary, &terminal).unwrap();
        assert!(already_prompted(&markers, &terminal));
        // ...and the LaunchAgent's start of the same binary still asks
        assert!(!already_prompted(&markers, &agent));
        let plan = plan_prompts(
            Grants {
                screen_recording: false,
                accessibility: false,
                post_event: false,
            },
            already_prompted(&markers, &agent),
        );
        assert!(plan.request_screen_recording && plan.request_accessibility);
        record_prompted(&markers, &binary, &agent).unwrap();
        // both contexts are remembered, so neither asks again
        assert!(already_prompted(&markers, &agent));
        assert!(already_prompted(&markers, &terminal));
        record_prompted(&markers, &binary, &agent).unwrap(); // idempotent
        let text = std::fs::read_to_string(&markers[0]).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");

        // a reinstalled/upgraded binary asks again, in every context, and
        // the old binary's lines are dropped
        let upgraded = "9.9.9 1 2 /usr/local/bin/tetherd";
        let upgraded_agent = marker_line(upgraded, &LaunchContext::Launchd);
        assert!(!already_prompted(&markers, &upgraded_agent));
        record_prompted(&markers, upgraded, &upgraded_agent).unwrap();
        assert!(!already_prompted(&markers, &agent));
        assert!(!already_prompted(&markers, &terminal));
        assert_eq!(
            std::fs::read_to_string(&markers[0]).unwrap(),
            format!("{upgraded_agent}\n")
        );

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(markers[0].parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "state dir created private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_state_dir_falls_back_to_the_next_location() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("fallback");
        // ~/.config/tether owned by root, in effect: exists, not writable
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        // a planted symlink is refused, not followed
        let target = dir.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let fallback = dir.join("caches").join(PROMPT_MARKER);
        let markers = vec![
            locked.join(PROMPT_MARKER),
            link.join(PROMPT_MARKER),
            fallback.clone(),
        ];
        let line = marker_line("1.0 1 2 /usr/local/bin/tetherd", &LaunchContext::Launchd);
        assert_eq!(
            record_prompted(&markers, "1.0 1 2 /usr/local/bin/tetherd", &line).unwrap(),
            fallback
        );
        assert!(!locked.join(PROMPT_MARKER).exists());
        assert!(!target.join(PROMPT_MARKER).exists(), "followed a symlink");
        // the next start finds it there and stays quiet
        assert!(already_prompted(&markers, &line));

        // nowhere writable at all: an error (check() then skips prompting)
        let nowhere = vec![locked.join(PROMPT_MARKER), link.join(PROMPT_MARKER)];
        assert!(record_prompted(&nowhere, "1.0 1 2 /usr/local/bin/tetherd", &line).is_err());
        assert!(record_prompted(&[], "x", &line).is_err());
        assert!(!already_prompted(&[], &line));

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_locations_start_with_the_state_dir_and_end_in_temp() {
        let paths = marker_paths();
        assert!(paths.len() >= 2);
        assert!(paths.iter().all(|p| p.ends_with(PROMPT_MARKER)));
        if let Ok(state) = crate::config::state_dir() {
            assert_eq!(paths[0], state.join(PROMPT_MARKER));
        }
        assert!(paths.last().unwrap().starts_with(std::env::temp_dir()));
    }
}
