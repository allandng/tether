//! Windows input injection via `SendInput`.
//!
//! - Mouse: absolute coordinates with `MOUSEEVENTF_VIRTUALDESK`, so the
//!   0..65535 range spans the whole virtual desktop and clicks land on
//!   whichever monitor is being captured, not always the primary.
//! - Keyboard: scan codes (`KEYEVENTF_SCANCODE`), mirroring the physical-key
//!   semantics of the DOM `code` the controller sends; Windows tracks modifier
//!   state from the injected keys itself, so no flag stamping is needed.
//! - Text: `KEYEVENTF_UNICODE`, one UTF-16 unit per event (surrogate pairs as
//!   two), which bypasses the keyboard layout — emoji and any script work.
//!
//! Limitation (documented in the README): User Interface Privilege Isolation
//! drops input aimed at elevated windows unless `tetherd` itself runs
//! elevated, and nothing reaches the secure desktop (UAC prompts, Ctrl+Alt+Del).

pub mod keymap;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::bail;
use tether_protocol::{InputEvent, MouseButton};
use tracing::{debug, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use self::keymap::{ScanCode, dom_code_to_scancode};
use super::InputInjector;
use crate::win;

/// One mouse wheel notch. Browsers report ~100 px per notch on Windows, so
/// DOM pixels scale by 120/100; sub-notch remainders are delivered as-is
/// (modern apps scroll smoothly on them, legacy ones accumulate).
const WHEEL_DELTA: i32 = 120;
const DOM_PIXELS_PER_NOTCH: i32 = 100;

/// A rectangle in physical virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

pub struct WinInjector {
    display_id: u32,
    display: Rect,
    /// When `display` was last re-read from the OS. Resolution changes,
    /// re-arranged or unplugged monitors all move the rect under us.
    refreshed: Option<Instant>,
    held_buttons: [bool; 3],
    held_keys: BTreeSet<(u16, bool)>,
    warned_blocked: bool,
}

impl WinInjector {
    /// Never fails for want of a monitor: with none attached yet (display
    /// off, odd session state) the rect resolves on the first event, the same
    /// way the capturer retries instead of giving up.
    pub fn new() -> anyhow::Result<Self> {
        let mut injector = WinInjector {
            display_id: 0,
            display: Rect {
                left: 0,
                top: 0,
                width: 0,
                height: 0,
            },
            refreshed: None,
            held_buttons: [false; 3],
            held_keys: BTreeSet::new(),
            warned_blocked: false,
        };
        injector.refresh_display();
        Ok(injector)
    }

    /// Re-read the active monitor's rect; fall back to the primary if it was
    /// unplugged (the capturer falls back the same way).
    fn refresh_display(&mut self) {
        let m = win::monitor(self.display_id).or_else(win::primary_monitor);
        if let Some(m) = m {
            self.display_id = m.id;
            self.display = monitor_rect(&m);
        }
        self.refreshed = Some(Instant::now());
    }

    fn refresh_if_stale(&mut self, force: bool) {
        let stale = self
            .refreshed
            .is_none_or(|at| at.elapsed() > Duration::from_secs(1));
        if force || stale || self.display.width == 0 {
            self.refresh_display();
        }
    }

    fn send(&mut self, inputs: &[INPUT]) -> anyhow::Result<()> {
        // SAFETY: a slice of fully-initialised INPUT structs and their size.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if (sent as usize) < inputs.len() {
            if !self.warned_blocked {
                self.warned_blocked = true;
                warn!(
                    "SendInput was blocked — the focused window is elevated (run tetherd as \
                     administrator to control it) or the secure desktop is showing"
                );
            }
            bail!("SendInput injected {sent}/{} events", inputs.len());
        }
        Ok(())
    }

    fn move_to(&mut self, x: u16, y: u16, extra: MOUSE_EVENT_FLAGS) -> anyhow::Result<()> {
        let (px, py) = normalized_to_pixel(&self.display, x, y);
        let (ax, ay) = pixel_to_absolute(&virtual_desktop(), px, py);
        self.send(&[mouse(
            ax,
            ay,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | extra,
        )])
    }

    fn key(&mut self, scan: ScanCode, down: bool) -> anyhow::Result<()> {
        if down {
            self.held_keys.insert((scan.code, scan.extended));
        } else {
            self.held_keys.remove(&(scan.code, scan.extended));
        }
        self.send(&[key_input(scan, down)])
    }
}

impl InputInjector for WinInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        // Cheap (EnumDisplayMonitors); always fresh for a click, at most once
        // a second for the stream of moves.
        self.refresh_if_stale(matches!(event, InputEvent::MouseDown { .. }));
        match *event {
            InputEvent::MouseMove { x, y } => self.move_to(x, y, MOUSE_EVENT_FLAGS(0))?,
            InputEvent::MouseDown { button, x, y } => {
                self.held_buttons[button_index(button)] = true;
                // Move and press in one event, so the press can't land at a
                // stale position if another input sneaks in between.
                self.move_to(x, y, button_flags(button, true))?;
            }
            InputEvent::MouseUp { button, x, y } => {
                self.held_buttons[button_index(button)] = false;
                self.move_to(x, y, button_flags(button, false))?;
            }
            InputEvent::Scroll { dx, dy } => {
                let mut events = Vec::with_capacity(2);
                if dy != 0 {
                    // DOM: positive = content scrolls down. Wheel: positive =
                    // away from the user (scroll up). Negate.
                    events.push(mouse(0, 0, -dom_to_wheel(dy), MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    // Horizontal wheel: positive = right, same as the DOM.
                    events.push(mouse(0, 0, dom_to_wheel(dx), MOUSEEVENTF_HWHEEL));
                }
                if !events.is_empty() {
                    self.send(&events)?;
                }
            }
            InputEvent::KeyDown { ref code, .. } => match dom_code_to_scancode(code) {
                Some(scan) => self.key(scan, true)?,
                None => debug!(code, "no Windows scan code for DOM code, dropping"),
            },
            InputEvent::KeyUp { ref code, .. } => {
                if let Some(scan) = dom_code_to_scancode(code) {
                    self.key(scan, false)?;
                }
            }
        }
        Ok(())
    }

    fn inject_text(&mut self, text: &str) -> anyhow::Result<()> {
        let inputs = text_inputs(text);
        if inputs.is_empty() {
            return Ok(());
        }
        self.send(&inputs)
    }

    fn release_all(&mut self) {
        let mut inputs = Vec::new();
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            if std::mem::take(&mut self.held_buttons[button_index(button)]) {
                inputs.push(mouse(0, 0, 0, button_flags(button, false)));
            }
        }
        for (code, extended) in std::mem::take(&mut self.held_keys) {
            inputs.push(key_input(ScanCode { code, extended }, false));
        }
        if !inputs.is_empty() {
            let _ = self.send(&inputs);
        }
    }

    fn set_active_display(&mut self, display_id: u32) {
        if display_id == self.display_id {
            return;
        }
        if win::monitor(display_id).is_some() {
            self.display_id = display_id;
            self.refresh_display();
        }
    }
}

fn monitor_rect(m: &win::Monitor) -> Rect {
    Rect {
        left: m.rect.left,
        top: m.rect.top,
        width: m.width() as i32,
        height: m.height() as i32,
    }
}

fn virtual_desktop() -> Rect {
    // SAFETY: plain metric queries.
    unsafe {
        Rect {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Protocol-normalized (0..=65535 across the active display) → a physical
/// pixel on the virtual desktop. 65535 is the display's last pixel.
pub fn normalized_to_pixel(display: &Rect, x: u16, y: u16) -> (i32, i32) {
    let span = |n: u16, len: i32| (i64::from(n) * i64::from((len - 1).max(0)) / 65535) as i32;
    (
        display.left + span(x, display.width),
        display.top + span(y, display.height),
    )
}

/// A virtual-desktop pixel → `SendInput`'s 0..=65535 absolute space (with
/// `MOUSEEVENTF_VIRTUALDESK`, that space spans the virtual desktop).
///
/// `ceil(p · 65536 / w)`: Windows converts back with `abs · w / 65536`, and
/// rounding *up* here is what makes that land exactly on pixel `p` — a
/// rounded `p · 65535 / (w − 1)` sends some pixels one to the left/up.
/// (Exact under round-to-nearest conversion too.)
pub fn pixel_to_absolute(desk: &Rect, px: i32, py: i32) -> (i32, i32) {
    let norm = |p: i32, origin: i32, len: i32| {
        let len = i64::from(len.max(1));
        let off = i64::from(p - origin).clamp(0, len - 1);
        ((off * 65536 + len - 1) / len).min(65535) as i32
    };
    (
        norm(px, desk.left, desk.width),
        norm(py, desk.top, desk.height),
    )
}

fn dom_to_wheel(delta: i16) -> i32 {
    i32::from(delta) * WHEEL_DELTA / DOM_PIXELS_PER_NOTCH
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as _,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(scan: ScanCode, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if scan.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    keybd(0, scan.code, flags)
}

fn keybd(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Committed text → a down/up pair per UTF-16 unit. Newline and tab go as the
/// real Enter / Tab keys: apps act on those keys, not on a packeted '\n'.
pub fn text_inputs(text: &str) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let key = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next(); // CRLF is one Enter
                }
                dom_code_to_scancode("Enter")
            }
            '\n' => dom_code_to_scancode("Enter"),
            '\t' => dom_code_to_scancode("Tab"),
            _ => None,
        };
        if let Some(scan) = key {
            out.push(key_input(scan, true));
            out.push(key_input(scan, false));
            continue;
        }
        let mut units = [0u16; 2];
        for &unit in c.encode_utf16(&mut units).iter() {
            out.push(keybd(0, unit, KEYEVENTF_UNICODE));
            out.push(keybd(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
    }
    out
}

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn button_flags(button: MouseButton, down: bool) -> MOUSE_EVENT_FLAGS {
    match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: Rect = Rect {
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn normalized_coordinates_span_the_display() {
        assert_eq!(normalized_to_pixel(&DISPLAY, 0, 0), (0, 0));
        assert_eq!(normalized_to_pixel(&DISPLAY, 65535, 65535), (1919, 1079));
        let (cx, cy) = normalized_to_pixel(&DISPLAY, 32768, 32768);
        assert!((959..=960).contains(&cx) && (539..=540).contains(&cy));
    }

    #[test]
    fn secondary_monitor_offsets_apply() {
        // A second 1280x1024 monitor to the left of the primary.
        let left = Rect {
            left: -1280,
            top: 0,
            width: 1280,
            height: 1024,
        };
        assert_eq!(normalized_to_pixel(&left, 0, 0), (-1280, 0));
        assert_eq!(normalized_to_pixel(&left, 65535, 65535), (-1, 1023));

        // ...and it maps onto the left edge of the virtual desktop.
        let desk = Rect {
            left: -1280,
            top: 0,
            width: 1280 + 1920,
            height: 1080,
        };
        assert_eq!(pixel_to_absolute(&desk, -1280, 0), (0, 0));
        let (ax, _) = pixel_to_absolute(&desk, 0, 0); // primary's top-left
        assert!((26200..=26240).contains(&ax), "got {ax}");
    }

    #[test]
    fn every_pixel_round_trips_through_absolute_coordinates() {
        // Windows maps absolute → pixel as abs * width / 65536 (floor); check
        // every pixel lands back on itself, on a few awkward widths.
        for width in [1280, 1920, 3200, 3840, 5120, 1366] {
            let desk = Rect {
                left: -width / 3,
                top: 0,
                width,
                height: 1,
            };
            for p in 0..width {
                let (abs, _) = pixel_to_absolute(&desk, desk.left + p, 0);
                assert!((0..=65535).contains(&abs));
                let floor = (i64::from(abs) * i64::from(width) / 65536) as i32;
                let nearest = ((i64::from(abs) * i64::from(width - 1) + 32767) / 65535) as i32;
                assert_eq!(floor, p, "width {width}: pixel {p} → abs {abs} → {floor}");
                assert!((nearest - p).abs() <= 1, "width {width}: {p} → {nearest}");
            }
        }
    }

    #[test]
    fn wheel_scaling_and_direction() {
        assert_eq!(dom_to_wheel(100), 120);
        assert_eq!(dom_to_wheel(-50), -60);
        assert_eq!(dom_to_wheel(0), 0);
    }

    #[test]
    fn text_becomes_unicode_pairs_and_surrogates() {
        // SAFETY (test): reading the `ki` arm of inputs we built as keyboard.
        let units = |inputs: &[INPUT]| -> Vec<(u16, bool)> {
            inputs
                .iter()
                .map(|i| unsafe {
                    (
                        i.Anonymous.ki.wScan,
                        i.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP),
                    )
                })
                .collect()
        };
        assert_eq!(
            units(&text_inputs("hi")),
            vec![
                (u16::from(b'h'), false),
                (u16::from(b'h'), true),
                (u16::from(b'i'), false),
                (u16::from(b'i'), true),
            ]
        );
        // U+1F600 is a surrogate pair: two units, each a down/up.
        let emoji = text_inputs("😀");
        assert_eq!(emoji.len(), 4);
        assert_eq!(units(&emoji)[0].0, 0xD83D);
        assert_eq!(units(&emoji)[2].0, 0xDE00);
        // newline is the Enter key (scan code, not KEYEVENTF_UNICODE)
        let enter = text_inputs("\r\n");
        assert_eq!(enter.len(), 2);
        assert_eq!(units(&enter)[0].0, 0x1C);
    }
}
