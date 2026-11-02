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
//! The audio clock is kept continuous: loopback capture delivers nothing
//! while nothing plays, so the gap is filled with silence frames — the RTP
//! timestamps then track wall-clock time and playback resumes in sync.
//!
//! Opt-in (`--audio`): on by default it would stream a room's sound to
//! whoever connects.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{info, warn};

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

/// Start the capture → encode thread. `make` builds the platform capturer on
/// that thread (COM objects are thread-affine) and is called again to follow
/// a device change or recover from an error.
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
                    Ok(c) => {
                        failures = 0;
                        c
                    }
                    Err(e) => {
                        failures += 1;
                        if failures == 1 || failures.is_multiple_of(30) {
                            warn!(error = %e, "system audio capture unavailable; retrying");
                        }
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                let (rate, channels) = capturer.format();
                info!(rate, channels, "system audio capture started");
                let mut resampler = Resampler::new(rate, channels);
                let start = Instant::now();
                let mut produced: u64 = 0; // stereo frames at 48 kHz
                let mut last_device_check = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    raw.clear();
                    if let Err(e) = capturer.read(&mut raw) {
                        warn!(error = %e, "audio capture lost; rebuilding");
                        break;
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
                            break;
                        }
                    }
                }
                pending.clear();
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

#[cfg(not(windows))]
pub fn start_platform() -> std::io::Result<AudioFeed> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "system audio capture is implemented for Windows hosts only so far",
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
