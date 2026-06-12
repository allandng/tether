//! Diagnostic for input injection: asks the OS whether this process is
//! actually allowed to post events, and tests both event-tap locations.

#[cfg(target_os = "macos")]
fn main() {
    use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    // SAFETY: plain FFI calls with no arguments returning bool; declared in
    // CoreGraphics (CGRemoteOperation.h) since macOS 10.15.
    unsafe extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
    }
    // SAFETY: see extern block above.
    let preflight = unsafe { CGPreflightPostEventAccess() };
    println!("CGPreflightPostEventAccess: {preflight}");
    if !preflight {
        // SAFETY: see extern block above. Pops the system permission dialog /
        // registers the app in the Accessibility list if absent.
        let granted = unsafe { CGRequestPostEventAccess() };
        println!("CGRequestPostEventAccess: {granted}");
    }

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
    let read_pos = || CGEvent::new(source.clone()).unwrap().location();
    let original = read_pos();

    for (name, tap) in [
        ("HID", CGEventTapLocation::HID),
        ("Session", CGEventTapLocation::Session),
    ] {
        let target = CGPoint::new(original.x + 40.0, original.y);
        let ev = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::MouseMoved,
            target,
            CGMouseButton::Left,
        )
        .unwrap();
        ev.post(tap);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = read_pos();
        println!(
            "tap {name}: moved {}",
            if (now.x - target.x).abs() < 3.0 && (now.y - target.y).abs() < 3.0 {
                "YES"
            } else {
                "no"
            }
        );
        // restore
        let back = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::MouseMoved,
            original,
            CGMouseButton::Left,
        )
        .unwrap();
        back.post(tap);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
