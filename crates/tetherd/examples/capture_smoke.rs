//! Module 3 smoke test: capture the main display for ~3 seconds, report the
//! achieved fps, and write the last frame to `<temp>/tether_capture_smoke.jpg`.
//!
//! Run: cargo run -p tetherd --example capture_smoke
//! macOS: requires Screen Recording permission for the invoking terminal.
//! Windows: run from an interactive desktop session (not over SSH).

#[cfg(any(target_os = "macos", windows))]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tetherd::capture::{FrameEncoder, ScreenCapturer};

    #[cfg(target_os = "macos")]
    let mut capturer = tetherd::capture::macos::SckCapturer::main_display(
        30,
        tetherd::capture::macos::CaptureFormat::Bgra,
    )?;
    #[cfg(windows)]
    let mut capturer = {
        tetherd::win::init_dpi_awareness();
        tetherd::capture::windows::DxgiCapturer::primary(30)?
    };
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
        let path = std::env::temp_dir().join("tether_capture_smoke.jpg");
        std::fs::write(&path, &jpeg)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("capture_smoke supports macOS and Windows only");
}
