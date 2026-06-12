//! H.264 gate measurement: capture the live display for ~5s, hardware-encode,
//! report fps, average access-unit size, and the effective bitrate.
//!
//! Run: cargo run --release -p tetherd --example h264_smoke [bitrate_kbps]

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tetherd::capture::{FrameEncoder, ScreenCapturer};

    let bitrate_kbps: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(4000);

    let mut capturer = tetherd::capture::macos::SckCapturer::main_display(30)?;
    let mut encoder = tetherd::encode::h264::VtH264Encoder::new(bitrate_kbps)?;
    let resolution = capturer.resolution();
    println!(
        "capturing {}x{} at target {bitrate_kbps} kbps ...",
        resolution.width, resolution.height
    );

    let start = Instant::now();
    let mut frames = 0u32;
    let mut bytes = 0usize;
    let mut encode_total = Duration::ZERO;
    while start.elapsed() < Duration::from_secs(5) {
        let raw = capturer.next_frame()?;
        let t = Instant::now();
        let au = encoder.encode(&raw)?;
        encode_total += t.elapsed();
        frames += 1;
        bytes += au.len();
    }

    let secs = start.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {secs:.2}s = {:.1} fps | avg encode {:.1} ms | avg AU {} KiB | {:.2} Mbps",
        frames as f64 / secs,
        encode_total.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        bytes / frames.max(1) as usize / 1024,
        (bytes as f64 * 8.0) / secs / 1_000_000.0,
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("h264_smoke is macOS-only");
}
