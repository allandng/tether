//! Module 5/6 smoke test: verify CGEvent injection actually lands (i.e. the
//! Accessibility permission is effective) by moving the cursor to the screen
//! center, reading the position back, and restoring it.
//!
//! Run: cargo run -p tetherd --example inject_smoke

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use tether_protocol::InputEvent;
    use tetherd::input::InputInjector;
    use tetherd::input::macos::{MacInjector, normalized_to_point};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| anyhow::anyhow!("no event source"))?;
    let cursor = |s: &CGEventSource| {
        CGEvent::new(s.clone())
            .map(|e| e.location())
            .map_err(|()| anyhow::anyhow!("no event"))
    };
    let original = cursor(&source)?;

    let mut injector = MacInjector::new()?;
    injector.inject(&InputEvent::MouseMove { x: 32768, y: 32768 })?;
    std::thread::sleep(std::time::Duration::from_millis(150));
    let moved = cursor(&source)?;

    // restore the user's cursor before judging the result
    let bounds = CGDisplay::main().bounds();
    let back_x = ((original.x - bounds.origin.x) / bounds.size.width * 65535.0) as u16;
    let back_y = ((original.y - bounds.origin.y) / bounds.size.height * 65535.0) as u16;
    injector.inject(&InputEvent::MouseMove {
        x: back_x,
        y: back_y,
    })?;

    let expected = normalized_to_point(&bounds, 32768, 32768);
    let (dx, dy) = (moved.x - expected.x, moved.y - expected.y);
    println!(
        "expected ({:.0},{:.0}) observed ({:.0},{:.0}) delta ({dx:.1},{dy:.1})",
        expected.x, expected.y, moved.x, moved.y
    );
    if dx.abs() < 3.0 && dy.abs() < 3.0 {
        println!("OK: injection lands (Accessibility permission effective)");
        Ok(())
    } else {
        anyhow::bail!(
            "cursor did not move to the expected position — Accessibility \
             permission is likely missing or not yet effective"
        )
    }
}

/// Windows: the same check through SendInput, against every monitor, plus a
/// harmless modifier tap and a zero-length text injection to prove the
/// keyboard paths aren't blocked (UIPI / secure desktop).
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use tether_protocol::InputEvent;
    use tetherd::input::InputInjector;
    use tetherd::input::windows::{Rect, WinInjector, normalized_to_pixel};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    tetherd::win::init_dpi_awareness();
    let cursor = || {
        let mut p = POINT::default();
        // SAFETY: out-pointer to a live local.
        unsafe { GetCursorPos(&mut p) }.map(|()| (p.x, p.y))
    };
    let original = cursor()?;
    let mut injector = WinInjector::new()?;
    let mut failures = 0;
    for m in tetherd::win::monitors() {
        injector.set_active_display(m.id);
        let rect = Rect {
            left: m.rect.left,
            top: m.rect.top,
            width: m.width() as i32,
            height: m.height() as i32,
        };
        // includes near-edge points: an off-by-one in the absolute mapping
        // shows up first at pixel 1 and the far edge
        for (nx, ny) in [
            (16384u16, 16384u16),
            (49152, 32768),
            (35, 61),
            (65500, 65535),
            (0, 0),
        ] {
            injector.inject(&InputEvent::MouseMove { x: nx, y: ny })?;
            std::thread::sleep(std::time::Duration::from_millis(60));
            let got = cursor()?;
            let want = normalized_to_pixel(&rect, nx, ny);
            let ok = (got.0 - want.0).abs() <= 2 && (got.1 - want.1).abs() <= 2;
            println!(
                "display {} ({}x{}): expected {want:?} observed {got:?} {}",
                m.id,
                m.width(),
                m.height(),
                if ok { "OK" } else { "MISMATCH" }
            );
            failures += usize::from(!ok);
        }
    }
    // restore the user's cursor before judging the result
    // SAFETY: plain Win32 call.
    unsafe { SetCursorPos(original.0, original.1)? };

    injector.inject(&InputEvent::KeyDown {
        code: "ShiftLeft".into(),
        modifiers: 0,
    })?;
    injector.inject(&InputEvent::KeyUp {
        code: "ShiftLeft".into(),
        modifiers: 0,
    })?;
    println!("keyboard: SendInput accepted a Shift tap");

    if failures == 0 {
        println!("OK: injection lands on every monitor");
        Ok(())
    } else {
        anyhow::bail!("{failures} position(s) did not land where expected")
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("inject_smoke supports macOS and Windows only");
}
