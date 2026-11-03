//! H.264 gate measurement: capture the live display for a few seconds,
//! encode (VideoToolbox on macOS, OpenH264 on Windows), and report fps, the
//! capture wait, encode cost (avg / p95 / max), access-unit size, bitrate,
//! process CPU and peak RSS, and the keyframe cadence. Fresh images and idle
//! repeats are counted separately: a static screen only yields repeats (one
//! every 250 ms), so its encode numbers are repeat costs.
//!
//! Run: cargo run --release -p tetherd --example h264_smoke [bitrate_kbps] [seconds]
//! Set `TETHER_H264_DUMP=<path>` to also write the Annex B stream to a file
//! (e.g. for `ffprobe -show_streams`).

#[cfg(any(target_os = "macos", windows))]
fn main() -> anyhow::Result<()> {
    use std::io::Write;
    use std::time::{Duration, Instant};
    use tetherd::capture::{FrameEncoder, RawFrame, ScreenCapturer};

    let mut args = std::env::args().skip(1);
    let bitrate_kbps: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(4000);
    let seconds: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    let mut dump = match std::env::var_os("TETHER_H264_DUMP") {
        Some(path) => Some(std::io::BufWriter::new(std::fs::File::create(path)?)),
        None => None,
    };

    // macOS captures NV12 for H.264 (zero-copy into VideoToolbox), as the
    // daemon does; `TETHER_CAPTURE_FORMAT=bgra` feeds BGRA surfaces instead.
    #[cfg(target_os = "macos")]
    let (mut capturer, mut encoder) = {
        use tetherd::capture::macos::{CaptureFormat, SckCapturer};
        let format = match std::env::var("TETHER_CAPTURE_FORMAT").as_deref() {
            Ok("bgra") => CaptureFormat::Bgra,
            _ => CaptureFormat::Nv12,
        };
        println!("capture format {format:?}");
        (
            SckCapturer::main_display_within(30, format, tetherd::encode::h264::MAX_SIZE)?,
            tetherd::encode::h264::VtH264Encoder::new(bitrate_kbps)?,
        )
    };
    #[cfg(windows)]
    let (mut capturer, mut encoder) = {
        tetherd::win::init_dpi_awareness();
        (
            tetherd::capture::windows::DxgiCapturer::primary(30)?,
            tetherd::encode::h264_soft::OpenH264Encoder::new(bitrate_kbps)?,
        )
    };
    let resolution = capturer.resolution();
    println!(
        "capturing {}x{} for {seconds} s at target {bitrate_kbps} kbps ...",
        resolution.width, resolution.height
    );

    let cpu_before = cpu_time();
    let start = Instant::now();
    let mut bytes = 0usize;
    let mut capture_total = Duration::ZERO;
    // Encode times, split by fresh image vs idle repeat.
    let mut fresh_ms = Vec::new();
    let mut repeat_ms = Vec::new();
    let mut keyframes_at = Vec::new();
    let mut prev: Option<RawFrame> = None;
    while start.elapsed() < Duration::from_secs(seconds) {
        let t = Instant::now();
        let raw = capturer.next_frame()?;
        capture_total += t.elapsed();
        let repeat = prev.as_ref().is_some_and(|p| p.same_image(&raw));
        let t = Instant::now();
        let au = encoder.encode(&raw)?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if repeat {
            repeat_ms.push(ms);
        } else {
            fresh_ms.push(ms);
        }
        bytes += au.len();
        if is_idr(&au) {
            keyframes_at.push(start.elapsed());
        }
        if let Some(out) = dump.as_mut() {
            out.write_all(&au)?;
        }
        prev = Some(raw);
    }
    drop(prev);

    let wall = start.elapsed();
    let secs = wall.as_secs_f64();
    let frames = fresh_ms.len() + repeat_ms.len();
    let mut all_ms: Vec<f64> = fresh_ms.iter().chain(&repeat_ms).copied().collect();
    println!(
        "{frames} frames in {secs:.2}s = {:.1} fps ({} fresh = {:.1} fps, {} repeats) | avg capture wait {:.1} ms | avg AU {} KiB | {:.2} Mbps",
        frames as f64 / secs,
        fresh_ms.len(),
        fresh_ms.len() as f64 / secs,
        repeat_ms.len(),
        capture_total.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        bytes / frames.max(1) / 1024,
        (bytes as f64 * 8.0) / secs / 1_000_000.0,
    );
    for (label, ms) in [
        ("all", &mut all_ms),
        ("fresh", &mut fresh_ms),
        ("repeat", &mut repeat_ms),
    ] {
        if let Some(s) = Stats::of(ms) {
            println!(
                "encode {label:>6}: avg {:.2} ms | p50 {:.2} | p95 {:.2} | max {:.2} (n={})",
                s.avg,
                s.p50,
                s.p95,
                s.max,
                ms.len()
            );
        }
    }
    if let (Some(before), Some(after)) = (cpu_before, cpu_time()) {
        println!(
            "process CPU {:.1}% of one core ({:.2} s CPU over {secs:.2} s) | peak RSS {} MiB",
            (after - before).as_secs_f64() / secs * 100.0,
            (after - before).as_secs_f64(),
            peak_rss_mib().unwrap_or(0),
        );
    }
    // Keyframe cadence, including the stretch after the last one: a
    // controller joining at the worst moment waits this long to decode.
    let mut marks = keyframes_at.clone();
    marks.push(wall);
    let max_gap = marks
        .windows(2)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    println!(
        "{} keyframes at {:?} s | longest keyframe gap {:.2} s",
        keyframes_at.len(),
        keyframes_at
            .iter()
            .map(|d| (d.as_secs_f64() * 100.0).round() / 100.0)
            .collect::<Vec<_>>(),
        max_gap.as_secs_f64(),
    );
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
struct Stats {
    avg: f64,
    p50: f64,
    p95: f64,
    max: f64,
}

#[cfg(any(target_os = "macos", windows))]
impl Stats {
    fn of(ms: &mut [f64]) -> Option<Stats> {
        if ms.is_empty() {
            return None;
        }
        ms.sort_by(f64::total_cmp);
        let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
        Some(Stats {
            avg: ms.iter().sum::<f64>() / ms.len() as f64,
            p50: at(0.5),
            p95: at(0.95),
            max: ms[ms.len() - 1],
        })
    }
}

/// User + system CPU time consumed by this process so far.
#[cfg(unix)]
fn cpu_time() -> Option<std::time::Duration> {
    let usage = rusage()?;
    let tv = |t: libc::timeval| {
        std::time::Duration::from_secs(t.tv_sec as u64)
            + std::time::Duration::from_micros(t.tv_usec as u64)
    };
    Some(tv(usage.ru_utime) + tv(usage.ru_stime))
}

#[cfg(unix)]
fn peak_rss_mib() -> Option<u64> {
    // macOS reports ru_maxrss in bytes (Linux: KiB).
    Some(rusage()?.ru_maxrss as u64 / (1024 * 1024))
}

#[cfg(unix)]
fn rusage() -> Option<libc::rusage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the struct it is given; RUSAGE_SELF is valid.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    // SAFETY: initialised by the successful call above.
    (rc == 0).then(|| unsafe { usage.assume_init() })
}

#[cfg(windows)]
fn cpu_time() -> Option<std::time::Duration> {
    None
}

#[cfg(windows)]
fn peak_rss_mib() -> Option<u64> {
    None
}

/// Annex B access unit containing an IDR slice (NAL type 5). Scanning for
/// the 3-byte start code also finds 4-byte ones.
#[cfg(any(target_os = "macos", windows))]
fn is_idr(au: &[u8]) -> bool {
    au.windows(3)
        .enumerate()
        .any(|(i, w)| w == [0, 0, 1] && au.get(i + 3).is_some_and(|b| b & 0x1F == 5))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("h264_smoke supports macOS and Windows only");
}
