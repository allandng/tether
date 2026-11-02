//! First-run permission flow on macOS (Phase 9 M3).
//!
//! The two TCC grants fail differently, which is why both are checked up
//! front rather than left to a README:
//!
//! - **Screen Recording** can be preflighted and requested: macOS shows a
//!   real prompt. (Capture also fails loudly without it — see capture/macos.)
//! - **Accessibility** never prompts. Without it macOS silently drops every
//!   injected event: the screen streams and nothing responds — the most
//!   confusing failure this project has. So the pane is opened directly and
//!   a watcher logs the moment the grant lands (no restart needed for input).
//!
//! Both grants follow the *launching* binary: moving from a terminal to the
//! LaunchAgent means granting again for that context.

use std::time::Duration;

use tracing::{info, warn};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns a `Boolean` (unsigned char).
    fn AXIsProcessTrusted() -> u8;
}

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

pub fn screen_recording_granted() -> bool {
    // SAFETY: no arguments, no preconditions (macOS 10.15+).
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn accessibility_granted() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Check both grants at daemon start; prompt / deep-link for what's missing.
pub fn check() {
    if !screen_recording_granted() {
        warn!(
            "Screen Recording is not granted to this process. macOS will now ask; \
             approve it in System Settings → Privacy & Security → Screen Recording, \
             then restart tetherd (macOS applies this grant at launch)."
        );
        // SAFETY: as above; shows the system prompt once per binary.
        unsafe {
            CGRequestScreenCaptureAccess();
        }
    }
    if !accessibility_granted() {
        warn!(
            "Accessibility is not granted: macOS will silently ignore all remote mouse \
             and keyboard input until it is. Opening System Settings → Privacy & \
             Security → Accessibility — enable tetherd (or the app that launched it)."
        );
        let _ = std::process::Command::new("open")
            .arg(ACCESSIBILITY_PANE)
            .status();
        std::thread::Builder::new()
            .name("tether-ax-watch".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    if accessibility_granted() {
                        info!("Accessibility granted — remote input now works");
                        return;
                    }
                }
            })
            .ok();
    }
}
