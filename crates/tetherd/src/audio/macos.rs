//! macOS system audio via ScreenCaptureKit.
//!
//! macOS has no loopback device; ScreenCaptureKit is the supported way to
//! capture what the machine is playing. It needs the same Screen Recording
//! permission as video and no microphone entitlement. An `SCStream` always
//! needs a content filter, so this is an *audio-only* stream on the main
//! display: a 2×2, 1 fps video that nothing consumes (no Screen output is
//! registered), plus the Audio output. It is its own stream, separate from
//! the video capturer's, so either can be rebuilt without disturbing the
//! other. The tether process's own audio is excluded.
//!
//! SCK delivers 48 kHz Float32 **planar** audio: each sample buffer is an
//! `AudioBufferList` of one buffer per channel, 960 frames (20 ms) each. The
//! dispatch-queue callback interleaves it ([`to_interleaved`]) and hands it to
//! the pull-based [`SystemAudioCapturer`] through a bounded channel the audio
//! thread drains every 10 ms.
//!
//! Unlike WASAPI loopback, SCK keeps delivering (silent) buffers while
//! nothing plays, so a channel that stays empty for [`STALL`] means the
//! stream is stuck; `read` then errors and the shared thread rebuilds the
//! capturer (which also covers the display going to sleep or the screen
//! locking: no displays → `system_mix` fails → retried with back-off, see
//! `audio::retry_after`). Output device changes need no handling — SCK taps
//! the mix before the device.
//!
//! The crate's blocking SCK calls (`SCShareableContent::get`, start and stop
//! capture) wait on a completion handler with no timeout — and a stuck
//! stream, the very case the watchdog exists for, is when `stop_capture` is
//! least likely to complete. So each runs on a helper thread and is waited
//! for at most a few seconds ([`BoundedCalls`]); one that doesn't finish is
//! abandoned (its thread stays parked) rather than wedging the audio thread.

use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use core_graphics::display::CGDisplay;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::{AudioChannelCount, AudioSampleRate};
use tracing::{debug, warn};

use super::{CHANNELS, SAMPLE_RATE, SystemAudioCapturer};

/// Sample buffers queued between the SCK callback and `read`: 64 × 20 ms =
/// 1.28 s of slack before the callback starts dropping.
const QUEUE: usize = 64;
/// SCK streams silence while nothing plays, so this long with no buffers at
/// all means the stream is stuck. Comfortably above the ~2.5 s a cold start
/// can take to deliver its first buffer.
const STALL: Duration = Duration::from_secs(5);

/// How long a blocking SCK call may take before it is abandoned. Content
/// enumeration and starting normally take well under a second.
const SCK_CALL_LIMIT: Duration = Duration::from_secs(5);
/// Stopping a healthy stream is quick; a stuck one may never answer.
const SCK_STOP_LIMIT: Duration = Duration::from_secs(2);
/// Abandoned SCK calls tolerated at once: past this SCK is evidently wedged,
/// and further calls fail fast instead of parking yet more threads.
const SCK_MAX_PARKED: usize = 4;

/// Every blocking SCK call this module makes goes through here.
static SCK: BoundedCalls = BoundedCalls::new(SCK_MAX_PARKED);

/// Runs blocking calls on helper threads with a deadline (see the module
/// docs), capping how many abandoned ones may pile up.
struct BoundedCalls {
    /// Helper threads whose call hasn't returned yet.
    outstanding: AtomicUsize,
    max_parked: usize,
}

impl BoundedCalls {
    const fn new(max_parked: usize) -> Self {
        BoundedCalls {
            outstanding: AtomicUsize::new(0),
            max_parked,
        }
    }

    /// `f()` if it returns within `limit`; `None` if it didn't (the thread is
    /// left to finish or hang on its own), or wasn't run at all because
    /// `max_parked` earlier calls are still stuck.
    fn run<T: Send + 'static>(
        &'static self,
        what: &'static str,
        limit: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        // The audio thread makes one call at a time, so anything still
        // outstanding here was abandoned.
        let parked = self.outstanding.fetch_add(1, Ordering::SeqCst);
        if parked >= self.max_parked {
            self.outstanding.fetch_sub(1, Ordering::SeqCst);
            debug!(
                what,
                parked, "ScreenCaptureKit wedged; not calling it again"
            );
            return None;
        }
        let (tx, rx) = sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name(format!("tether-sck-{what}"))
            .spawn(move || {
                let v = f();
                self.outstanding.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(v); // the caller may have given up: fine
            });
        if spawned.is_err() {
            self.outstanding.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        match rx.recv_timeout(limit) {
            Ok(v) => Some(v),
            Err(_) => {
                warn!(
                    what,
                    limit_secs = limit.as_secs(),
                    "ScreenCaptureKit call did not return; abandoning it"
                );
                None
            }
        }
    }
}

const FORMAT_UNCHECKED: u8 = 0;
const FORMAT_OK: u8 = 1;
const FORMAT_BAD: u8 = 2;

/// State shared between the SCK callbacks (dispatch queues) and the capturer.
#[derive(Default)]
struct Shared {
    /// First fatal error: the stream stopped, or delivered a format we can't
    /// read. `read` reports it, which makes the audio thread rebuild.
    error: Mutex<Option<String>>,
    /// Buffers dropped because the queue was full (consumer behind).
    dropped: AtomicU64,
    format: AtomicU8,
}

impl Shared {
    fn fail(&self, msg: String) {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(msg);
    }

    fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct AudioOut {
    tx: SyncSender<Vec<f32>>,
    shared: Arc<Shared>,
}

impl SCStreamOutputTrait for AudioOut {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Audio) {
            return;
        }
        match self.shared.format.load(Ordering::Relaxed) {
            FORMAT_UNCHECKED => {
                // first buffer: confirm it's what format() promised
                if let Err(e) = check_format(&sample) {
                    self.shared.format.store(FORMAT_BAD, Ordering::Relaxed);
                    self.shared.fail(e);
                    return;
                }
                self.shared.format.store(FORMAT_OK, Ordering::Relaxed);
            }
            FORMAT_BAD => return,
            _ => {} // FORMAT_OK
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };
        let data: Vec<&[u8]> = list.iter().map(|b| b.data()).collect();
        let channels: Vec<u32> = list.iter().map(|b| b.number_channels).collect();
        let samples = to_interleaved(&data, &channels);
        if samples.is_empty() {
            return;
        }
        match self.tx.try_send(samples) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {} // capturer dropped; stream stopping
        }
    }
}

/// 48 kHz native-endian 32-bit float, as configured; anything else would be
/// misread (or played at the wrong pitch, since `format()` is fixed).
fn check_format(sample: &CMSampleBuffer) -> Result<(), String> {
    let Some(fd) = sample.format_description() else {
        return Ok(()); // nothing to check against; trust the configuration
    };
    let rate = fd.audio_sample_rate();
    let bits = fd.audio_bits_per_channel();
    let flags = fd.audio_format_flags();
    debug!(?rate, channels = ?fd.audio_channel_count(), ?bits, ?flags, "SCK audio format");
    let native_endian = fd.audio_is_big_endian() == cfg!(target_endian = "big");
    if fd.audio_is_float() && native_endian && bits == Some(32) && rate == Some(48_000.0) {
        Ok(())
    } else {
        Err(format!(
            "unsupported ScreenCaptureKit audio format (rate {rate:?}, bits {bits:?}, \
             flags {flags:?}); expected 48 kHz Float32"
        ))
    }
}

/// One SCK sample's `AudioBufferList` → interleaved **stereo** f32.
///
/// `buffers[i]` is raw native-endian f32 bytes holding
/// `channels_per_buffer[i]` interleaved channels, so this covers both layouts
/// Core Audio uses: N planar buffers of one channel each (what SCK sends), or
/// one interleaved buffer (passed through). Left/right are the first two
/// channels across the list; a single (mono) channel is duplicated to both.
/// Lengths are untrusted: the frame count is the shortest of the two source
/// channels, and trailing partial samples/frames are ignored. Bytes are read
/// with `from_ne_bytes`, so no alignment is assumed.
pub(crate) fn to_interleaved(buffers: &[&[u8]], channels_per_buffer: &[u32]) -> Vec<f32> {
    // (buffer index, channel within it, channels in it) for left and right
    let mut picks: Vec<(usize, usize, usize)> = Vec::with_capacity(2);
    'outer: for (i, &n) in channels_per_buffer.iter().enumerate().take(buffers.len()) {
        for c in 0..n as usize {
            if picks.len() == 2 {
                break 'outer;
            }
            picks.push((i, c, n as usize));
        }
    }
    let (left, right) = match picks[..] {
        [] => return Vec::new(),
        [mono] => (mono, mono),
        [l, r, ..] => (l, r),
    };
    let sample = |(buf, ch, stride): (usize, usize, usize), frame: usize| {
        let at = (frame * stride + ch) * 4;
        let b = &buffers[buf][at..at + 4];
        f32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    };
    let frames_in = |(buf, _, stride): (usize, usize, usize)| buffers[buf].len() / 4 / stride;
    let frames = frames_in(left).min(frames_in(right));
    let mut out = Vec::with_capacity(frames * CHANNELS);
    for f in 0..frames {
        out.push(sample(left, f));
        out.push(sample(right, f));
    }
    out
}

/// Detects a stream that has stopped delivering, from poll results.
struct Watchdog {
    last_data: Instant,
}

impl Watchdog {
    fn new(now: Instant) -> Self {
        Watchdog { last_data: now }
    }

    /// Record a poll at `now` (`fed`: it yielded samples); true once nothing
    /// has arrived for longer than [`STALL`].
    fn poll(&mut self, fed: bool, now: Instant) -> bool {
        if fed {
            self.last_data = now;
            return false;
        }
        now.duration_since(self.last_data) > STALL
    }
}

/// System-audio capture of everything the Mac plays (except tetherd itself).
pub struct SckAudio {
    /// `Some` until `Drop` moves it to the stop thread. Never cloned: in
    /// screencapturekit 7.0.1 dropping *any* `SCStream` handle — a clone
    /// included — unregisters the stream's output handlers
    /// (`sc_stream_release` → `removeStreamState`), silencing the original.
    stream: Option<SCStream>,
    rx: Receiver<Vec<f32>>,
    shared: Arc<Shared>,
    watchdog: Watchdog,
    dropped_seen: u64,
}

impl SckAudio {
    /// Start capturing the system audio mix.
    pub fn system_mix() -> anyhow::Result<Self> {
        let content = SCK
            .run("content", SCK_CALL_LIMIT, SCShareableContent::get)
            .ok_or_else(|| anyhow!("ScreenCaptureKit did not list shareable content in time"))?
            .map_err(|e| {
                anyhow!(
                    "ScreenCaptureKit refused ({e}). System audio needs the Screen Recording \
                     permission: System Settings → Privacy & Security → Screen Recording"
                )
            })?;
        let displays = content.displays();
        let main_id = CGDisplay::main().id;
        let display = displays
            .iter()
            .find(|d| d.display_id() == main_id)
            .or_else(|| displays.first())
            .ok_or_else(|| {
                anyhow!("no displays available for audio capture (display asleep or screen locked)")
            })?;
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_fps(1)
            .with_captures_audio(true)
            .with_sample_rate(AudioSampleRate::Rate48000)
            .with_channel_count(AudioChannelCount::Stereo)
            .with_excludes_current_process_audio(true);

        let shared = Arc::new(Shared::default());
        let (tx, rx) = sync_channel(QUEUE);
        let on_error = Arc::clone(&shared);
        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(move |e: SCError| on_error.fail(e.to_string())),
        );
        stream
            .add_output_handler(
                AudioOut {
                    tx,
                    shared: Arc::clone(&shared),
                },
                SCStreamOutputType::Audio,
            )
            .ok_or_else(|| anyhow!("ScreenCaptureKit rejected the audio output"))?;
        // From here on, Drop stops the stream on every path. The capturer
        // itself travels to the start thread and back (the stream can't be
        // cloned, see the field docs); if the start is abandoned, that thread
        // drops it — stopping the stream — once the start finally returns.
        let capturer = SckAudio {
            stream: Some(stream),
            rx,
            shared,
            watchdog: Watchdog::new(Instant::now()),
            dropped_seen: 0,
        };
        let (mut capturer, started) = SCK
            .run("start", SCK_CALL_LIMIT, move || {
                let started = capturer.stream.as_ref().map(SCStream::start_capture);
                (capturer, started)
            })
            .ok_or_else(|| anyhow!("ScreenCaptureKit did not start audio capture in time"))?;
        if let Some(Err(e)) = started {
            bail!("failed to start audio capture: {e}");
        }
        // the cold-start allowance runs from now, not from before the start
        capturer.watchdog = Watchdog::new(Instant::now());
        Ok(capturer)
    }
}

impl SystemAudioCapturer for SckAudio {
    fn format(&self) -> (u32, usize) {
        (SAMPLE_RATE, CHANNELS)
    }

    fn read(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        if let Some(e) = self.shared.error() {
            bail!("ScreenCaptureKit audio stream stopped: {e}");
        }
        let before = out.len();
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => bail!("ScreenCaptureKit audio stream ended"),
            }
        }
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if dropped != self.dropped_seen {
            warn!(
                buffers = dropped - self.dropped_seen,
                "system audio reader fell behind; dropped audio"
            );
            self.dropped_seen = dropped;
        }
        if self.watchdog.poll(out.len() > before, Instant::now()) {
            bail!(
                "no system audio from ScreenCaptureKit for {} s; stream stalled",
                STALL.as_secs()
            );
        }
        Ok(())
    }
}

impl Drop for SckAudio {
    fn drop(&mut self) {
        // Bounded: a stuck stream's stop may never complete (module docs).
        // Moved, not cloned (see the field docs): an abandoned stop keeps the
        // stream on its parked thread, released if the stop ever returns.
        let Some(stream) = self.stream.take() else {
            return;
        };
        match SCK.run("stop", SCK_STOP_LIMIT, move || stream.stop_capture()) {
            Some(Ok(())) | None => {} // None: abandoned, already logged
            // expected if the stream already died (that's why we're rebuilding)
            Some(Err(e)) => debug!("audio stop_capture failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_ne_bytes()).collect()
    }

    #[test]
    fn planar_stereo_is_interleaved() {
        let (l, r) = (bytes(&[0.1, 0.2, 0.3]), bytes(&[-0.1, -0.2, -0.3]));
        assert_eq!(
            to_interleaved(&[&l, &r], &[1, 1]),
            vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3]
        );
    }

    #[test]
    fn interleaved_stereo_passes_through() {
        let b = bytes(&[0.1, -0.1, 0.2, -0.2]);
        assert_eq!(to_interleaved(&[&b], &[2]), vec![0.1, -0.1, 0.2, -0.2]);
    }

    #[test]
    fn mono_is_duplicated_to_stereo() {
        let m = bytes(&[0.5, -0.25]);
        assert_eq!(to_interleaved(&[&m], &[1]), vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn extra_channels_are_dropped() {
        // three planar buffers (say L, R, C): front L/R only
        let (l, r, c) = (bytes(&[1.0, 2.0]), bytes(&[3.0, 4.0]), bytes(&[9.0, 9.0]));
        assert_eq!(
            to_interleaved(&[&l, &r, &c], &[1, 1, 1]),
            vec![1.0, 3.0, 2.0, 4.0]
        );
        // one interleaved 5.1-style buffer: first two of each frame
        let s = bytes(&[1.0, 2.0, 9.0, 9.0, 9.0, 9.0, 3.0, 4.0, 9.0, 9.0, 9.0, 9.0]);
        assert_eq!(to_interleaved(&[&s], &[6]), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn ragged_and_odd_lengths_are_truncated_not_misread() {
        // planar channels of unequal length: the shorter one wins
        let (l, r) = (bytes(&[0.1, 0.2, 0.3]), bytes(&[-0.1, -0.2]));
        assert_eq!(
            to_interleaved(&[&l, &r], &[1, 1]),
            vec![0.1, -0.1, 0.2, -0.2]
        );
        // a trailing partial sample (byte count not a multiple of 4)
        let mut l = bytes(&[0.1, 0.2]);
        l.extend_from_slice(&[0xAB, 0xCD]);
        let r = bytes(&[-0.1, -0.2]);
        assert_eq!(
            to_interleaved(&[&l, &r], &[1, 1]),
            vec![0.1, -0.1, 0.2, -0.2]
        );
        // a trailing partial frame in an interleaved buffer
        let b = bytes(&[0.1, -0.1, 0.2]);
        assert_eq!(to_interleaved(&[&b], &[2]), vec![0.1, -0.1]);
        // nothing usable
        assert!(to_interleaved(&[], &[]).is_empty());
        assert!(to_interleaved(&[&b], &[0]).is_empty());
        assert!(to_interleaved(&[&[0u8; 3][..]], &[1]).is_empty());
        // more channel counts than buffers (or vice versa): only pairs count
        assert_eq!(to_interleaved(&[&b], &[2, 1]), vec![0.1, -0.1]);
    }

    #[test]
    fn unaligned_bytes_are_read_correctly() {
        let mut raw = vec![0u8];
        raw.extend(bytes(&[0.5, -0.5, 0.25, -0.25]));
        let unaligned = &raw[1..]; // f32s at odd addresses
        assert_eq!(
            to_interleaved(&[unaligned], &[2]),
            vec![0.5, -0.5, 0.25, -0.25]
        );
    }

    #[test]
    fn watchdog_trips_only_after_five_quiet_seconds() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut w = Watchdog::new(t0);
        // cold start: nothing for a while is fine...
        assert!(!w.poll(false, t0 + s(3)));
        assert!(!w.poll(false, t0 + s(5)));
        // ...until it's longer than STALL
        assert!(w.poll(false, t0 + s(5) + Duration::from_millis(1)));
        // data resets the clock
        assert!(!w.poll(true, t0 + s(6)));
        assert!(!w.poll(false, t0 + s(10)));
        assert!(w.poll(false, t0 + s(12)));
        // a poll "before" the last data (clock skew) doesn't panic or trip
        assert!(!w.poll(true, t0 + s(20)));
        assert!(!w.poll(false, t0 + s(19)));
    }

    #[test]
    fn bounded_calls_abandon_stuck_calls_and_stop_piling_them_up() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;
        static CALLS: BoundedCalls = BoundedCalls::new(2);
        let ms = Duration::from_millis;

        assert_eq!(CALLS.run("fast", ms(5000), || 7), Some(7));

        // a call that never completes (a stuck stop_capture): given up on
        // after its limit instead of blocking the caller forever
        let (release1, wait1) = channel::<()>();
        let t = Instant::now();
        let stuck1 = CALLS.run("stuck", ms(50), move || {
            let _ = wait1.recv();
            1
        });
        assert_eq!(stuck1, None);
        assert!(t.elapsed() < ms(2000), "waited {:?}", t.elapsed());
        let (release2, wait2) = channel::<()>();
        assert_eq!(
            CALLS.run("stuck", ms(50), move || {
                let _ = wait2.recv();
                2
            }),
            None
        );

        // two abandoned calls parked: the next one isn't even attempted
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&ran);
        assert_eq!(
            CALLS.run("refused", ms(5000), move || flag
                .store(true, Ordering::SeqCst)),
            None
        );
        assert!(!ran.load(Ordering::SeqCst));

        // once the stuck calls finish, calls go through again
        drop((release1, release2));
        let t = Instant::now();
        while CALLS.outstanding.load(Ordering::SeqCst) > 0 {
            assert!(t.elapsed() < ms(5000), "parked calls never finished");
            std::thread::sleep(ms(5));
        }
        assert_eq!(CALLS.run("fast", ms(5000), || 9), Some(9));
    }

    /// Plays a short system sound; returns the player if it could start.
    fn tink() -> Option<std::process::Child> {
        std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Tink.aiff")
            .spawn()
            .ok()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// Real capture: `cargo test -p tetherd audio::macos -- --ignored --nocapture`.
    /// Needs Screen Recording permission and an awake display; plays a short
    /// system sound (audible) to check that real audio comes through.
    #[test]
    #[ignore = "real ScreenCaptureKit capture: needs Screen Recording permission; plays a sound"]
    fn real_sck_audio_flows() {
        let opened = Instant::now();
        let mut cap = SckAudio::system_mix().expect("open ScreenCaptureKit audio");
        assert_eq!(cap.format(), (48_000, 2));
        // cold start: the first buffer can take a few seconds
        let mut buf = Vec::new();
        while buf.is_empty() {
            assert!(
                opened.elapsed() < Duration::from_secs(6),
                "no audio within 6 s"
            );
            std::thread::sleep(Duration::from_millis(10));
            cap.read(&mut buf).expect("read");
        }
        let first = opened.elapsed();

        // one second of steady state, with a sound playing in it
        let mut player = tink();
        buf.clear();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
            cap.read(&mut buf).expect("read");
        }
        let secs = t0.elapsed().as_secs_f64();
        if let Some(p) = player.as_mut() {
            let _ = p.wait();
        }
        let frames_per_sec = buf.len() as f64 / 2.0 / secs;
        let peak = peak(&buf);
        eprintln!(
            "SCK audio: first buffer after {first:?}; {} samples in {secs:.3} s \
             = {frames_per_sec:.0} frames/s (stereo); peak {peak:.4}",
            buf.len()
        );
        assert!(
            buf.len() as f64 >= 0.4 * 48_000.0 * 2.0,
            "only {} samples in {secs:.3} s",
            buf.len()
        );
        if player.is_some() {
            assert!(peak > 1e-4, "played a sound but captured silence");
        }
        // the bounded stop on drop completes (nothing left parked)
        drop(cap);
        assert_eq!(SCK.outstanding.load(Ordering::SeqCst), 0, "stop abandoned");
    }

    /// Real capture through the whole shared pipeline (resample, frame, Opus).
    #[tokio::test]
    #[ignore = "real ScreenCaptureKit capture: needs Screen Recording permission; plays a sound"]
    async fn real_pipeline_publishes_opus_at_50_per_second() {
        use tokio::sync::broadcast::error::RecvError;
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("tetherd=debug")
            .try_init();
        let feed = crate::audio::start(SckAudio::system_mix).expect("start audio thread");
        // let SCK cold-start (the clock pads silence meanwhile)
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut rx = feed.subscribe();

        let mut player = tink();
        let t0 = Instant::now();
        let (mut packets, mut min, mut max) = (0usize, usize::MAX, 0usize);
        while t0.elapsed() < Duration::from_secs(2) {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(p)) => {
                    packets += 1;
                    min = min.min(p.len());
                    max = max.max(p.len());
                }
                Ok(Err(RecvError::Lagged(n))) => panic!("test receiver lagged by {n}"),
                other => panic!("feed stalled: {other:?}"),
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        if let Some(p) = player.as_mut() {
            let _ = p.wait();
        }
        eprintln!(
            "pipeline: {packets} Opus packets in {secs:.3} s = {:.1}/s; sizes {min}..={max} bytes",
            packets as f64 / secs
        );
        assert!((90..=110).contains(&packets), "got {packets} packets");
        if player.is_some() {
            // silence encodes to a few bytes; real sound at 96 kbps is ~240
            assert!(max > 50, "no non-silent packets (max {max} bytes)");
        }
    }
}
