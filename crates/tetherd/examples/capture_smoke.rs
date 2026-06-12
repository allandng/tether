//! Module 3 smoke test: capture the main display for ~3 seconds, report the
//! achieved fps, and write the last frame to /tmp/tether_capture_smoke.jpg.
//!
//! Run: cargo run -p tetherd --example capture_smoke
//! Requires Screen Recording permission for the invoking terminal.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tetherd::capture::{FrameEncoder, ScreenCapturer};

    let mut capturer = tetherd::capture::macos::SckCapturer::main_display(30)?;
    let mut encoder = tetherd::encode::JpegEncoder::new(75)?;
    let resolution = capturer.resolution();
    println!("capturing {}x{} ...", resolution.width, resolution.height);

    let start = Instant::now();
    let mut frames = 0u32;
    let mut encode_total = Duration::ZERO;
    let mut last_jpeg = None;
    while start.elapsed() < Duration::from_secs(3) {
        let raw = capturer.next_frame()?;
        let t = Instant::now();
        let jpeg = encoder.encode(&raw)?;
        encode_total += t.elapsed();
        frames += 1;
        last_jpeg = Some(jpeg);
    }

    let secs = start.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {secs:.2}s = {:.1} fps (avg encode {:.1} ms, last frame {} KiB)",
        frames as f64 / secs,
        encode_total.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        last_jpeg.as_ref().map(|j| j.len() / 1024).unwrap_or(0),
    );
    if let Some(jpeg) = last_jpeg {
        std::fs::write("/tmp/tether_capture_smoke.jpg", &jpeg)?;
        println!("wrote /tmp/tether_capture_smoke.jpg");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("capture_smoke is macOS-only");
}
