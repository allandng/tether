//! Diagnostic: is Screen Recording access effective, and what does
//! ScreenCaptureKit's shareable content actually contain?

#[cfg(target_os = "macos")]
fn main() {
    // SAFETY: argument-less CoreGraphics FFI returning bool (CGRemoteOperation.h).
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    // SAFETY: see extern block.
    let preflight = unsafe { CGPreflightScreenCaptureAccess() };
    println!("CGPreflightScreenCaptureAccess: {preflight}");
    if !preflight {
        // SAFETY: see extern block.
        let granted = unsafe { CGRequestScreenCaptureAccess() };
        println!("CGRequestScreenCaptureAccess: {granted}");
    }

    use core_graphics::display::CGDisplay;
    println!("CGDisplay::main().id = {}", CGDisplay::main().id);
    println!(
        "CGDisplay active displays: {:?}",
        CGDisplay::active_displays().map(|v| v.len())
    );

    match screencapturekit::shareable_content::SCShareableContent::get() {
        Ok(content) => {
            let displays = content.displays();
            println!("SCK displays: {}", displays.len());
            for d in &displays {
                println!("  id={} {}x{}", d.display_id(), d.width(), d.height());
            }
            println!("SCK windows: {}", content.windows().len());
        }
        Err(e) => println!("SCShareableContent::get failed: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
