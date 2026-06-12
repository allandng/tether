//! macOS clipboard via NSPasteboard. Constructed on the sync thread;
//! NSPasteboard is not main-thread-only and these calls are documented safe
//! off the main thread.

use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

use super::Clipboard;

pub struct MacClipboard {
    pasteboard: Retained<NSPasteboard>,
}

impl MacClipboard {
    pub fn new() -> Self {
        // SAFETY: generalPasteboard is callable from any thread; the retained
        // pasteboard stays on this thread.
        MacClipboard { pasteboard: unsafe { NSPasteboard::generalPasteboard() } }
    }
}

impl Default for MacClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for MacClipboard {
    fn change_count(&mut self) -> i64 {
        // SAFETY: simple property read on a valid pasteboard.
        unsafe { self.pasteboard.changeCount() as i64 }
    }

    fn read_text(&mut self) -> Option<String> {
        // SAFETY: NSPasteboardTypeString is a valid static type identifier.
        let s = unsafe { self.pasteboard.stringForType(NSPasteboardTypeString) }?;
        Some(s.to_string())
    }

    fn write_text(&mut self, text: &str) -> i64 {
        let ns = NSString::from_str(text);
        // SAFETY: clearContents must precede setString (pasteboard ownership
        // protocol); both are valid calls on the general pasteboard.
        unsafe {
            let _ = self.pasteboard.clearContents();
            if !self.pasteboard.setString_forType(&ns, NSPasteboardTypeString) {
                tracing::warn!("NSPasteboard setString failed");
            }
            // Read back rather than guess how many bumps the write produced.
            self.pasteboard.changeCount() as i64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips through the real pasteboard, restoring prior content.
    #[test]
    fn pasteboard_round_trip() {
        let mut board = MacClipboard::new();
        let saved = board.read_text();

        let probe = "tether-pasteboard-test-§ünïcode";
        let count_after_write = board.write_text(probe);
        assert_eq!(board.read_text().as_deref(), Some(probe));
        assert_eq!(
            board.change_count(),
            count_after_write,
            "write_text must report the post-write change count"
        );

        match saved {
            Some(old) => {
                board.write_text(&old);
            }
            None => {
                board.write_text("");
            }
        }
    }
}
