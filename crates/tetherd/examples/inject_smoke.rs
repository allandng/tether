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

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("inject_smoke is macOS-only");
}
