//! Windows helpers shared by the capture, input and clipboard modules.
//!
//! Display identity is the GDI device number: `\\.\DISPLAY3` → id 3. GDI
//! (`MONITORINFOEXW::szDevice`) and DXGI (`DXGI_OUTPUT_DESC::DeviceName`) report
//! the same name, so the capturer and the injector agree on ids without
//! sharing state, and an id stays stable while the monitor stays plugged in.

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::BOOL;

/// `MONITORINFOF_PRIMARY` (WinUser.h).
const MONITORINFOF_PRIMARY: u32 = 1;

/// One attached monitor, in physical pixels on the virtual desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub id: u32,
    pub rect: RECT,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> u32 {
        (self.rect.right - self.rect.left).max(0) as u32
    }
    pub fn height(&self) -> u32 {
        (self.rect.bottom - self.rect.top).max(0) as u32
    }
}

/// Opt the process into per-monitor DPI awareness, so every coordinate the
/// OS hands back (monitor rects, cursor position) is in physical pixels — the
/// same space the captured frames are in. Without it, a 150%-scaled display
/// reports virtualised coordinates and injected clicks land in the wrong
/// place. Must run before any window or monitor API is touched; harmless if
/// the awareness was already set (by a manifest, or a second call).
pub fn init_dpi_awareness() {
    // SAFETY: plain Win32 call with a predefined context constant.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// `\\.\DISPLAY12` → 12. `None` for anything not shaped like a GDI display.
pub fn display_number(device: &[u16]) -> Option<u32> {
    let len = device.iter().position(|&c| c == 0).unwrap_or(device.len());
    let name = String::from_utf16_lossy(&device[..len]);
    name.strip_prefix(r"\\.\DISPLAY")?.parse().ok()
}

/// Every monitor attached to the desktop, sorted by id.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the &mut Vec passed by monitors() below, alive for
        // the duration of the synchronous EnumDisplayMonitors call.
        let out = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: cbSize marks the struct as the EX variant it really is.
        let ok = unsafe {
            GetMonitorInfoW(
                hmonitor,
                &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
            )
        };
        if ok.as_bool()
            && let Some(id) = display_number(&info.szDevice)
        {
            out.push(Monitor {
                id,
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        true.into()
    }

    let mut out: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only dereferences `data` as the Vec we own here.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    out.sort_by_key(|m| m.id);
    out
}

/// The monitor with this id, if it is still attached.
pub fn monitor(id: u32) -> Option<Monitor> {
    monitors().into_iter().find(|m| m.id == id)
}

/// The primary monitor (falling back to the first one).
pub fn primary_monitor() -> Option<Monitor> {
    let all = monitors();
    all.iter()
        .find(|m| m.primary)
        .cloned()
        .or_else(|| all.into_iter().next())
}

/// Encode a Rust string as a NUL-terminated UTF-16 buffer for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_numbers_parse_from_gdi_device_names() {
        let mut buf = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY12".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(display_number(&buf), Some(12));
        assert_eq!(display_number(&wide(r"\\.\DISPLAY1")), Some(1));
        assert_eq!(display_number(&wide(r"\\.\DISPLAYX")), None);
        assert_eq!(display_number(&wide("DISPLAY1")), None);
        assert_eq!(display_number(&[]), None);
    }

    #[test]
    fn this_machine_has_a_monitor() {
        // CI runners and interactive desktops both expose at least one.
        init_dpi_awareness();
        let all = monitors();
        assert!(!all.is_empty(), "no monitors enumerated");
        assert!(all.iter().all(|m| m.width() > 0 && m.height() > 0));
        assert!(primary_monitor().is_some());
    }
}
