//! Windows clipboard (`CF_UNICODETEXT`).
//!
//! Change detection uses `GetClipboardSequenceNumber`, a system-wide counter
//! that bumps on every clipboard write. It maps one-to-one onto
//! [`Clipboard::change_count`], so the shared polling loop works unchanged.
//!
//! Writing needs an owner window: with a NULL owner, `SetClipboardData` fails
//! after `EmptyClipboard`. And the owner must pump messages — Windows *sends*
//! the owner `WM_DESTROYCLIPBOARD` when another app takes the clipboard, so
//! an owner thread parked in the sync loop's `recv_timeout` would stall every
//! other app's copy for up to a poll interval. So the clipboard lives on its
//! own thread with a message-only window and a real message loop, and reads
//! and writes are marshalled onto it.
//!
//! Text is converted at the boundary: UTF-16 ↔ UTF-8 (the protocol carries
//! UTF-8) and CRLF ↔ LF (Windows text is CRLF; macOS and the browser are LF).

use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use tracing::{debug, warn};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
};
use windows::core::PCWSTR;

use super::Clipboard;
use crate::win::wide;

const WM_RUN: u32 = WM_APP + 1;
const WM_STOP: u32 = WM_APP + 2;

type Job = Box<dyn FnOnce(HWND) + Send>;

pub struct WinClipboard {
    /// The owner window, as a raw value (`HWND` is not `Send`).
    hwnd: isize,
    /// Work for the owner thread. WM_RUN only wakes it; the jobs themselves
    /// never travel through a message, so nothing another process posts to
    /// the window can be mistaken for one.
    jobs: std::sync::mpsc::Sender<Job>,
}

impl WinClipboard {
    pub fn new() -> anyhow::Result<Self> {
        let (tx, rx) = sync_channel::<Result<isize, String>>(1);
        let (jobs, job_rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("tether-clipboard-owner".into())
            .spawn(move || owner_thread(tx, job_rx))?;
        let hwnd = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("clipboard owner thread did not start"))?
            .map_err(anyhow::Error::msg)?;
        Ok(WinClipboard { hwnd, jobs })
    }

    /// Run `f` on the owner thread and wait for its result.
    fn run<R: Send + 'static>(&self, f: impl FnOnce(HWND) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = sync_channel::<R>(1);
        let job: Job = Box::new(move |hwnd| {
            let _ = tx.send(f(hwnd));
        });
        self.jobs.send(job).ok()?;
        // SAFETY: posting a plain wake-up to our own window.
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_RUN,
                WPARAM(0),
                LPARAM(0),
            )
            .ok()?;
        }
        rx.recv_timeout(Duration::from_secs(3)).ok()
    }
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        // SAFETY: posting to our own window; failure just means it's gone.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_STOP,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn owner_thread(ready: SyncSender<Result<isize, String>>, jobs: std::sync::mpsc::Receiver<Job>) {
    let class = wide("STATIC");
    // SAFETY: a message-only STATIC window; all pointers outlive the call.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(format!("CreateWindowExW: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the window's own thread.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_RUN if msg.hwnd == hwnd => {
                    while let Ok(job) = jobs.try_recv() {
                        job(hwnd);
                    }
                }
                WM_STOP if msg.hwnd == hwnd => {
                    let _ = DestroyWindow(hwnd);
                    return;
                }
                _ => {
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

/// Open the clipboard, retrying briefly: another app may hold it open.
fn open(hwnd: HWND) -> bool {
    for _ in 0..20 {
        // SAFETY: plain Win32 call with our own window.
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn read_unicode(hwnd: HWND) -> Option<String> {
    // SAFETY: the clipboard is opened/closed in pairs on this thread; the
    // global handle is only read within its locked extent.
    unsafe {
        if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_err() {
            return None;
        }
        if !open(hwnd) {
            debug!("clipboard busy, skipping read");
            return None;
        }
        let text = (|| {
            let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let max_units = GlobalSize(global) / 2;
            let units = std::slice::from_raw_parts(ptr, max_units);
            let len = units.iter().position(|&u| u == 0).unwrap_or(max_units);
            let s = String::from_utf16_lossy(&units[..len]);
            let _ = GlobalUnlock(global);
            Some(s)
        })();
        let _ = CloseClipboard();
        text.map(|t| crlf_to_lf(&t))
    }
}

fn write_unicode(hwnd: HWND, text: &str) -> bool {
    let units: Vec<u16> = lf_to_crlf(text)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: as in read_unicode; the allocation is handed to the system on
    // a successful SetClipboardData and freed by us otherwise.
    unsafe {
        if !open(hwnd) {
            warn!("clipboard busy, could not apply controller clipboard");
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let global = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2).ok()?;
            let ptr = GlobalLock(global) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            let _ = GlobalUnlock(global);
            if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0))).is_err() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

impl Clipboard for WinClipboard {
    fn change_count(&mut self) -> i64 {
        // SAFETY: plain counter read, valid from any thread.
        i64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn read_text(&mut self) -> Option<String> {
        self.run(read_unicode).flatten()
    }

    fn write_text(&mut self, text: &str) -> i64 {
        let owned = text.to_owned();
        if self.run(move |hwnd| write_unicode(hwnd, &owned)) != Some(true) {
            warn!("writing the host clipboard failed");
        }
        self.change_count()
    }
}

/// Windows line endings → the protocol's LF.
pub fn crlf_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// LF → CRLF, without doubling CRs that are already there.
pub fn lf_to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 16);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_round_trip() {
        assert_eq!(lf_to_crlf("a\nb\n"), "a\r\nb\r\n");
        assert_eq!(lf_to_crlf("a\r\nb"), "a\r\nb", "no doubled CR");
        assert_eq!(crlf_to_lf("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(crlf_to_lf(&lf_to_crlf("x\ny\n\nz")), "x\ny\n\nz");
    }

    /// Touches the real clipboard: run explicitly with `--ignored` on an
    /// interactive desktop. Restores the previous text afterwards.
    #[test]
    #[ignore]
    fn real_clipboard_round_trip() {
        let mut cb = WinClipboard::new().unwrap();
        let before = cb.read_text();
        let c0 = cb.change_count();
        let c1 = cb.write_text("tether ✓ test\nline two");
        assert!(c1 > c0, "write must bump the sequence number");
        assert_eq!(cb.read_text().as_deref(), Some("tether ✓ test\nline two"));
        if let Some(prev) = before {
            cb.write_text(&prev);
        }
    }
}
