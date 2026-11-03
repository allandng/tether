//! System audio (Phase 10): capture what the host is playing, encode Opus,
//! and fan it out to every WebRTC peer as a real audio track.
//!
//! One capture thread per daemon, its own thread — never the video capture
//! thread: video is latest-wins by design, but a dropped audio buffer is an
//! audible click. It resamples whatever the platform delivers to 48 kHz
//! stereo, cuts 20 ms frames, encodes them, and publishes the packets on a
//! broadcast channel. Peers subscribe once authenticated; a peer that falls
//! behind skips packets rather than stalling the others. With no subscribers
//! the encoder is skipped entirely.
//!
//! Platform capturers: WASAPI loopback on Windows (`audio::windows`),
//! ScreenCaptureKit on macOS (`audio::macos`).
//!
//! The audio clock is kept continuous: loopback capture delivers nothing
//! while nothing plays (WASAPI), and any capturer can stall or cold-start
//! slowly (ScreenCaptureKit), so the gap is filled with silence frames — the RTP
//! timestamps then track wall-clock time and playback resumes in sync.
//!
//! Opt-in (`--audio`): on by default it would stream a room's sound to
//! whoever connects.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms per Opus frame.
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 50) * CHANNELS;
const BITRATE: i32 = 96_000;

/// Platform loopback capture. `read` appends whatever interleaved f32
/// samples are available right now (possibly none) and returns quickly.
pub trait SystemAudioCapturer {
    /// (sample rate, channel count) of what `read` produces.
    fn format(&self) -> (u32, usize);
    fn read(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()>;
    /// True if the default output device changed (rebuild to follow it).
    fn device_changed(&mut self) -> bool {
        false
    }
}

/// Cheap to clone; subscribe for 20 ms Opus packets.
#[derive(Clone)]
pub struct AudioFeed {
    tx: broadcast::Sender<Bytes>,
}

impl AudioFeed {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    /// A feed with no capture thread behind it, for tests: push packets with
    /// the returned sender.
    pub fn manual() -> (Self, broadcast::Sender<Bytes>) {
        let (tx, _) = broadcast::channel(64);
        (AudioFeed { tx: tx.clone() }, tx)
    }
}

/// Linear-interpolating resampler + channel mapper to 48 kHz stereo.
/// Streaming: state carries across calls so frame boundaries are seamless.
pub struct Resampler {
    in_rate: u32,
    in_channels: usize,
    /// Position of the next output sample, in input frames, relative to `prev`.
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(in_rate: u32, in_channels: usize) -> Self {
        Resampler {
            in_rate,
            in_channels: in_channels.max(1),
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    fn stereo(&self, frame: &[f32]) -> [f32; 2] {
        match self.in_channels {
            1 => [frame[0], frame[0]],
            _ => [frame[0], frame[1]], // stereo, or front L/R of a surround mix
        }
    }

    /// Convert interleaved `input` and append 48 kHz stereo to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.in_channels;
        let frames = input.len() / ch;
        if self.in_rate == SAMPLE_RATE {
            for f in 0..frames {
                let s = self.stereo(&input[f * ch..f * ch + ch]);
                out.extend_from_slice(&s);
            }
            return;
        }
        let step = f64::from(self.in_rate) / f64::from(SAMPLE_RATE);
        // Output samples lie between prev (index -1 → 0.0 offset) and the
        // input frames; pos is measured from prev.
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let a = if i == 0 {
                self.prev
            } else {
                self.stereo(&input[(i - 1) * ch..i * ch])
            };
            let b = self.stereo(&input[i * ch..i * ch + ch]);
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev = self.stereo(&input[(frames - 1) * ch..frames * ch]);
        }
    }
}

/// Keep the produced sample count locked to wall-clock time (`expected`),
/// both in frames at 48 kHz; returns the new produced count.
///
/// - Behind by more than 60 ms (nothing playing → loopback delivers nothing):
///   pad with silence, so RTP timestamps keep pace and playback resumes in
///   sync. A gap over 500 ms (machine slept, thread stalled) is *not*
///   synthesised — that would be minutes of silence encoded in a burst, or a
///   multi-gigabyte allocation after a night's sleep — the clock just resyncs.
/// - Ahead by more than 100 ms (real audio arriving on top of padding, or a
///   device clock running fast): drop the oldest pending samples, so latency
///   can't creep up over a long session.
pub fn keep_on_clock(pending: &mut Vec<f32>, produced: u64, expected: u64) -> u64 {
    let ms = |n: u64| u64::from(SAMPLE_RATE) * n / 1000;
    if expected > produced + ms(500) {
        return expected;
    }
    if expected > produced + ms(60) {
        let gap = (expected - produced) as usize;
        pending.extend(std::iter::repeat_n(0.0, gap * CHANNELS));
        return expected;
    }
    if produced > expected + ms(100) {
        let excess = ((produced - expected) as usize).min(pending.len() / CHANNELS);
        pending.drain(..excess * CHANNELS);
        return produced - excess as u64;
    }
    produced
}

/// A capturer that errors sooner than this after it was built counts as a
/// failed start, like `make` failing: a stream that dies right away (or keeps
/// tripping a stall watchdog) is backed off, not rebuilt in a tight loop.
pub const HEALTHY_RUN: Duration = Duration::from_secs(10);
/// First retry delay; it doubles per consecutive failure up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(16);

/// Why a capturer is being rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEnd {
    /// `make` failed.
    StartFailed,
    /// `read` errored after the capturer had run for this long.
    Lost(Duration),
    /// The default output device changed (rebuild to follow it).
    DeviceChanged,
}

/// What the audio thread does before the next `make`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Consecutive failures so far (carried to the next decision).
    pub failures: u32,
    /// Sleep this long first.
    pub delay: Duration,
    /// Log this one at warn level (the rest are throttled to debug).
    pub warn: bool,
}

/// The rebuild policy, given the consecutive failures before this end. A
/// failed start and a capturer lost within [`HEALTHY_RUN`] are both failures:
/// back off (2 s, doubling to 16 s) and warn on the first and every 30th. A
/// capturer lost after a healthy run is rebuilt at once (warned: it's news),
/// and a device change is not a failure at all.
pub fn retry_after(failures: u32, end: CaptureEnd) -> Retry {
    match end {
        CaptureEnd::DeviceChanged => Retry {
            failures,
            delay: Duration::ZERO,
            warn: false,
        },
        CaptureEnd::Lost(ran) if ran >= HEALTHY_RUN => Retry {
            failures: 0,
            delay: Duration::ZERO,
            warn: true,
        },
        CaptureEnd::StartFailed | CaptureEnd::Lost(_) => {
            let failures = failures.saturating_add(1);
            let doublings = (failures - 1).min(16);
            Retry {
                failures,
                delay: FIRST_RETRY.saturating_mul(1 << doublings).min(MAX_RETRY),
                warn: failures == 1 || failures.is_multiple_of(30),
            }
        }
    }
}

/// Start the capture → encode thread. `make` builds the platform capturer on
/// that thread, so the capturer need not be `Send` (WASAPI's COM objects are
/// thread-affine), and is called again to follow a device change or recover
/// from an error — with a growing delay while it keeps failing to start or
/// dying young (e.g. no Screen Recording permission yet, or the display
/// asleep, on macOS); see [`retry_after`].
pub fn start<C, F>(mut make: F) -> std::io::Result<AudioFeed>
where
    C: SystemAudioCapturer,
    F: FnMut() -> anyhow::Result<C> + Send + 'static,
{
    let (tx, _) = broadcast::channel::<Bytes>(64);
    let feed = AudioFeed { tx: tx.clone() };
    std::thread::Builder::new()
        .name("tether-audio".into())
        .spawn(move || {
            let mut encoder = match opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::Audio,
            ) {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "Opus encoder unavailable; no audio");
                    return;
                }
            };
            let _ = encoder.set_bitrate(opus::Bitrate::Bits(BITRATE));
            let mut packet = vec![0u8; 4000];
            let mut raw = Vec::new();
            let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
            let mut failures = 0u32;
            loop {
                let mut capturer = match make() {
                    Ok(c) => c,
                    Err(e) => {
                        let retry = retry_after(failures, CaptureEnd::StartFailed);
                        failures = retry.failures;
                        if retry.warn {
                            warn!(error = %e, failures, "system audio capture unavailable; retrying");
                        } else {
                            debug!(error = %e, failures, "system audio capture unavailable; retrying");
                        }
                        std::thread::sleep(retry.delay);
                        continue;
                    }
                };
                let (rate, channels) = capturer.format();
                if failures <= 1 {
                    info!(rate, channels, "system audio capture started");
                } else {
                    debug!(rate, channels, failures, "system audio capture started");
                }
                let mut resampler = Resampler::new(rate, channels);
                let start = Instant::now();
                let mut produced: u64 = 0; // stereo frames at 48 kHz
                let mut last_device_check = Instant::now();
                let end = loop {
                    std::thread::sleep(Duration::from_millis(10));
                    if failures > 0 && start.elapsed() >= HEALTHY_RUN {
                        info!(after = failures, "system audio capture recovered");
                        failures = 0;
                    }
                    raw.clear();
                    if let Err(e) = capturer.read(&mut raw) {
                        let end = CaptureEnd::Lost(start.elapsed());
                        if retry_after(failures, end).warn {
                            warn!(error = %e, "audio capture lost; rebuilding");
                        } else {
                            debug!(error = %e, failures, "audio capture lost; rebuilding");
                        }
                        break end;
                    }
                    let before = pending.len();
                    resampler.process(&raw, &mut pending);
                    produced += ((pending.len() - before) / CHANNELS) as u64;
                    let expected = (start.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE)) as u64;
                    produced = keep_on_clock(&mut pending, produced, expected);
                    while pending.len() >= FRAME_SAMPLES {
                        if tx.receiver_count() > 0 {
                            match encoder.encode_float(&pending[..FRAME_SAMPLES], &mut packet) {
                                Ok(n) => {
                                    let _ = tx.send(Bytes::copy_from_slice(&packet[..n]));
                                }
                                Err(e) => warn!(error = %e, "Opus encode failed"),
                            }
                        }
                        pending.drain(..FRAME_SAMPLES);
                    }
                    if last_device_check.elapsed() > Duration::from_secs(2) {
                        last_device_check = Instant::now();
                        if capturer.device_changed() {
                            info!("default audio output changed; following it");
                            break CaptureEnd::DeviceChanged;
                        }
                    }
                };
                // Tear the capturer down before any back-off sleep.
                drop(capturer);
                pending.clear();
                let retry = retry_after(failures, end);
                failures = retry.failures;
                std::thread::sleep(retry.delay);
            }
        })?;
    Ok(feed)
}

#[cfg(windows)]
pub mod windows;

/// The platform capturer for `--audio`, if this platform has one.
#[cfg(windows)]
pub fn start_platform() -> std::io::Result<AudioFeed> {
    start(windows::WasapiLoopback::default_device)
}

#[cfg(target_os = "macos")]
pub mod macos;

/// The platform capturer for `--audio`, if this platform has one.
#[cfg(target_os = "macos")]
pub fn start_platform() -> std::io::Result<AudioFeed> {
    start(macos::SckAudio::system_mix)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn start_platform() -> std::io::Result<AudioFeed> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "system audio capture is implemented for Windows and macOS hosts only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_at_48k_stereo() {
        let mut r = Resampler::new(48_000, 2);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn mono_and_surround_map_to_stereo() {
        let mut out = Vec::new();
        Resampler::new(48_000, 1).process(&[0.5, -0.5], &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
        out.clear();
        Resampler::new(48_000, 6).process(&[0.1, 0.2, 9.0, 9.0, 9.0, 9.0], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn resampling_produces_the_right_count_across_chunks() {
        // 44.1 kHz → 48 kHz: 1 s in arbitrary chunk sizes yields ~48000 frames.
        let mut r = Resampler::new(44_100, 2);
        let input: Vec<f32> = (0..44_100 * 2).map(|i| (i as f32 * 0.001).sin()).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(2 * 441 + 6) {
            r.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "got {frames}");
        // and a ramp stays a ramp (interpolation, not garbage)
        let mut r = Resampler::new(24_000, 1);
        let mut out = Vec::new();
        r.process(&[0.0, 1.0, 2.0, 3.0], &mut out);
        // (one input sample of latency: interpolation runs from the previous
        // chunk's last sample)
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, vec![0.0, 0.0, 0.0, 0.5, 1.0, 1.5, 2.0, 2.5]);
    }

    #[test]
    fn clock_pads_short_gaps_resyncs_long_ones_and_trims_drift() {
        let ms = |n: u64| 48 * n;
        // 200 ms behind: padded with exactly that much silence
        let mut p = Vec::new();
        assert_eq!(keep_on_clock(&mut p, 0, ms(200)), ms(200));
        assert_eq!(p.len() as u64, ms(200) * 2);
        // 8 hours behind (slept): resync, allocate nothing
        let mut p = Vec::new();
        let night = ms(8 * 3600 * 1000);
        assert_eq!(keep_on_clock(&mut p, 0, night), night);
        assert!(p.is_empty());
        // 30 ms behind: within slack, untouched
        let mut p = Vec::new();
        assert_eq!(keep_on_clock(&mut p, 0, ms(30)), 0);
        // 300 ms ahead with 250 ms pending: trims what it can
        let mut p = vec![0.5; (ms(250) * 2) as usize];
        assert_eq!(keep_on_clock(&mut p, ms(1300), ms(1000)), ms(1050));
        assert!(p.is_empty());
    }

    #[test]
    fn failed_starts_and_young_deaths_back_off_healthy_losses_do_not() {
        let s = Duration::from_secs;
        // make() failing: 2 s, doubling, capped at 16 s; warn 1st and every 30th
        let mut failures = 0;
        let mut delays = Vec::new();
        let mut warned = Vec::new();
        for _ in 0..60 {
            let r = retry_after(failures, CaptureEnd::StartFailed);
            failures = r.failures;
            delays.push(r.delay.as_secs());
            if r.warn {
                warned.push(failures);
            }
        }
        assert_eq!(delays[..6], [2, 4, 8, 16, 16, 16]);
        assert_eq!(warned, vec![1, 30, 60]);

        // a capturer that started fine but died within HEALTHY_RUN is a
        // failure too — the stream-dies-right-after-starting loop
        assert_eq!(
            retry_after(0, CaptureEnd::Lost(Duration::from_millis(30))),
            Retry {
                failures: 1,
                delay: s(2),
                warn: true
            }
        );
        // ...including one that keeps tripping a 5 s stall watchdog
        let r = retry_after(1, CaptureEnd::Lost(s(5)));
        assert_eq!((r.failures, r.delay, r.warn), (2, s(4), false));
        // and it accumulates across start failures and young deaths alike
        let r = retry_after(r.failures, CaptureEnd::StartFailed);
        assert_eq!((r.failures, r.delay), (3, s(8)));

        // lost after a healthy run: rebuild at once, counter reset, warned
        assert_eq!(
            retry_after(7, CaptureEnd::Lost(HEALTHY_RUN)),
            Retry {
                failures: 0,
                delay: Duration::ZERO,
                warn: true
            }
        );
        // a device change is not a failure
        assert_eq!(
            retry_after(3, CaptureEnd::DeviceChanged),
            Retry {
                failures: 3,
                delay: Duration::ZERO,
                warn: false
            }
        );
        // no overflow at the extremes
        let r = retry_after(u32::MAX, CaptureEnd::StartFailed);
        assert_eq!((r.failures, r.delay), (u32::MAX, s(16)));
    }

    /// Starts fine, errors on the first read.
    struct DiesAtOnce;

    impl SystemAudioCapturer for DiesAtOnce {
        fn format(&self) -> (u32, usize) {
            (48_000, 2)
        }
        fn read(&mut self, _: &mut Vec<f32>) -> anyhow::Result<()> {
            anyhow::bail!("stream stopped")
        }
    }

    #[test]
    fn a_capturer_that_dies_at_once_is_not_rebuilt_in_a_tight_loop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let makes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&makes);
        let _feed = start(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(DiesAtOnce)
        })
        .unwrap();
        // one read (~10 ms) then a 2 s back-off: without it this would be
        // ~50 rebuilds by now
        std::thread::sleep(Duration::from_millis(500));
        let n = makes.load(Ordering::SeqCst);
        assert_eq!(n, 1, "rebuilt {n} times in 500 ms");
    }

    struct Sine {
        t: u64,
        started: Instant,
    }

    impl SystemAudioCapturer for Sine {
        fn format(&self) -> (u32, usize) {
            (44_100, 2)
        }
        fn read(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
            let due = (self.started.elapsed().as_secs_f64() * 44_100.0) as u64;
            while self.t < due {
                let v = (self.t as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.3;
                out.extend_from_slice(&[v, v]);
                self.t += 1;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn feed_publishes_20ms_opus_packets_in_real_time() {
        let feed = start(|| {
            Ok(Sine {
                t: 0,
                started: Instant::now(),
            })
        })
        .unwrap();
        let mut rx = feed.subscribe();
        let t0 = Instant::now();
        let mut packets = 0;
        while t0.elapsed() < Duration::from_millis(600) {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(p)) => {
                    assert!(
                        !p.is_empty() && p.len() < 1500,
                        "opus packet size {}",
                        p.len()
                    );
                    packets += 1;
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("feed stalled: {other:?}"),
            }
        }
        // ~30 packets in 600 ms (50/s), allowing for start-up
        assert!((20..=40).contains(&packets), "got {packets} packets");
    }
}
