//! macOS screen capture via ScreenCaptureKit.
//!
//! ScreenCaptureKit is push-based (frames arrive on a dispatch-queue
//! callback); this adapter bridges to the pull-based [`ScreenCapturer`] trait
//! through a one-frame, latest-wins [`Mailbox`]: if the consumer falls
//! behind, a newer frame replaces the unconsumed one, so the image the screen
//! finally settles on is never the one that gets dropped.
//!
//! On a static screen SCK delivers one complete frame and then only `.idle`
//! status samples that carry no image. The pipeline needs a steady trickle,
//! though — a joining controller needs a frame, an H.264 decoder needs a
//! keyframe, display switches are applied between frames — so while idle the
//! last image is re-emitted every [`IDLE_REPEAT`], sharing its pixel buffer
//! rather than copying it (the Windows capturer does the same).
//!
//! The stream is kept alive, too. A stream error (reported to the stream's
//! delegate) tears it down and rebuilds it in place with backoff, falling
//! back to the main display if the captured one is gone; meanwhile the last
//! image keeps repeating, so the remote screen freezes rather than the
//! session dying. About once a second the display mode is re-read and the
//! stream reconfigured to the new native size, so resolution changes reach
//! the pipeline as frames of the new size. The one stop that is *not*
//! undone is the local user's: "Stop Sharing" in macOS's menu-bar recording
//! indicator pauses capture until a viewer connects or picks a display
//! ([`Driver::user_stopped`]).
//!
//! The output size is the display's native pixel size, optionally scaled
//! down (aspect preserved) to fit a maximum — the hardware H.264 encoder's
//! ([`fit_within`]). Input is unaffected: it is mapped through normalized
//! coordinates onto the display's bounds, whatever size the video is.
//!
//! Frames are not copied out of ScreenCaptureKit: each carries the
//! IOSurface-backed `CVPixelBuffer` SCK delivered, retained
//! ([`PixelBuffer`]). For H.264 the stream asks for NV12, which VideoToolbox
//! encodes straight from the IOSurface; for JPEG it asks for BGRA, which the
//! encoder reads in place. Holding buffers takes them out of SCK's pool, so
//! the pool is deepened ([`QUEUE_DEPTH`]) and at most a couple are ever held:
//! the mailbox's pending frame and the last image (which the idle repeat
//! re-emits — the same buffer, with a fresh timestamp).
//!
//! The repeat/recovery logic is [`Driver`], generic over a [`Backend`] so it
//! is unit-tested against a fake; [`Sck`] is the ScreenCaptureKit backend.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use core_graphics::display::CGDisplay;
use screencapturekit::error::SCStreamErrorCode;
use screencapturekit::prelude::*;
use tether_protocol::{DisplayInfo, Resolution};
use tracing::{debug, info, warn};

use super::pixel_buffer::PixelBuffer;
use super::{Backoff, RawFrame, ScreenCapturer};

/// Re-emit the last image this often while the screen is static.
const IDLE_REPEAT: Duration = Duration::from_millis(250);
/// How often to re-read the captured display's mode.
const MODE_CHECK: Duration = Duration::from_secs(1);
/// A live display switch or reconfiguration should be answered by a fresh
/// frame; a stream still silent after this long is rebuilt.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// Backoff between attempts to rebuild a lost stream. The display sleeping
/// is the common cause (SCK fails the stream with "No capture source
/// provided" and lists no displays until it wakes), so the cap bounds how
/// long video stays frozen after a controller's input wakes the display.
const REBUILD_MIN: Duration = Duration::from_millis(250);
const REBUILD_MAX: Duration = Duration::from_secs(2);
/// SCK's surface pool size (its default is 3, its maximum 8). Delivered
/// buffers are retained rather than copied, so the pool must cover what the
/// daemon holds — the mailbox's pending frame and the last image (also the
/// one being encoded), plus a transient extra while the mailbox swaps — and
/// still leave SCK surfaces to render into; a starved pool drops frames.
const QUEUE_DEPTH: u32 = 6;

/// The pixel format to capture in, chosen by the encoder that consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    /// 32-bit BGRA, for byte consumers (JPEG).
    Bgra,
    /// NV12 (`420v`: 4:2:0, BT.709 video range): VideoToolbox's native
    /// H.264 input — no colour conversion, and under half BGRA's bytes.
    Nv12,
}

impl CaptureFormat {
    fn sck(self) -> PixelFormat {
        match self {
            CaptureFormat::Bgra => PixelFormat::BGRA,
            CaptureFormat::Nv12 => PixelFormat::YCbCr_420v,
        }
    }
}

/// The display list is empty — display asleep, screen locked, or clamshell.
/// Transient by nature: callers should retry rather than die, so that the
/// daemon is reachable while the machine's display sleeps (connecting and
/// injecting input is how a remote controller *wakes* it).
#[derive(Debug)]
pub struct NoDisplays;

impl std::fmt::Display for NoDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no displays available to capture (display asleep or screen locked)"
        )
    }
}

impl std::error::Error for NoDisplays {}

pub struct SckCapturer {
    driver: Driver<Sck>,
}

impl SckCapturer {
    /// Begin capturing the main display at its native pixel resolution.
    pub fn main_display(fps: u32, format: CaptureFormat) -> anyhow::Result<Self> {
        Ok(SckCapturer {
            driver: Driver::new(Sck::start(fps, format, None)?),
        })
    }

    /// [`main_display`](Self::main_display), but scaled down (aspect
    /// preserved, even dimensions) wherever a display's native size exceeds
    /// `max_size` — e.g. the hardware H.264 encoder's limit
    /// (`encode::h264::MAX_SIZE`) on a 5K/6K display or a scaled "More
    /// Space" mode. ScreenCaptureKit does the scaling on the GPU.
    pub fn main_display_within(
        fps: u32,
        format: CaptureFormat,
        max_size: Resolution,
    ) -> anyhow::Result<Self> {
        Ok(SckCapturer {
            driver: Driver::new(Sck::start(fps, format, Some(max_size))?),
        })
    }
}

impl ScreenCapturer for SckCapturer {
    fn resolution(&self) -> Resolution {
        self.driver.backend.resolution
    }

    /// Never fails: a lost stream is rebuilt in place while the last image
    /// repeats.
    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        Ok(self.driver.next_frame())
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        let current_id = self.driver.backend.display_id;
        let Ok(content) = SCShareableContent::get() else {
            // transient (asleep/locked) — report the one we're on
            let r = self.resolution();
            return vec![DisplayInfo {
                id: current_id,
                width: r.width,
                height: r.height,
                active: true,
                name: "Display".into(),
            }];
        };
        let mut ids: Vec<u32> = content.displays().iter().map(|d| d.display_id()).collect();
        ids.sort_unstable(); // stable ordinal naming
        ids.iter()
            .enumerate()
            .map(|(i, &id)| {
                let (w, h) = native_pixels(id);
                DisplayInfo {
                    id,
                    width: w,
                    height: h,
                    active: id == current_id,
                    name: format!("Display {}", i + 1),
                }
            })
            .collect()
    }

    fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
        if id == self.driver.backend.display_id && self.driver.backend.running() {
            return Ok(());
        }
        let result = self.driver.backend.switch_display(id);
        self.driver.after_switch(result.is_ok());
        result
    }

    fn active_display(&self) -> u32 {
        self.driver.backend.display_id
    }

    fn viewer_joined(&mut self) {
        self.driver.resume();
    }
}

// ---------------------------------------------------------------------------
// Driver: idle repeat, stream recovery, mode tracking (platform-independent
// logic over a Backend, so it can be tested with a fake).
// ---------------------------------------------------------------------------

/// Outcome of waiting on the live stream.
#[derive(Debug)]
enum Poll {
    Frame(RawFrame),
    Timeout,
    /// The stream died (delegate error, or its output went away).
    Lost(String),
    /// The local user stopped the stream ("Stop Sharing" in macOS's
    /// menu-bar recording indicator).
    UserStopped,
}

/// What [`Driver`] needs from a capture stream.
trait Backend {
    /// Whether a stream is currently running.
    fn running(&self) -> bool;
    /// Wait up to `timeout` for the running stream's next new image.
    fn poll(&mut self, timeout: Duration) -> Poll;
    /// Drop a frame that was delivered but not yet polled (it predates a
    /// display switch).
    fn discard_pending(&mut self);
    /// Tear down the running stream (it failed, or the user stopped it).
    fn stop(&mut self);
    /// Start a fresh stream on the current display — or, if that display is
    /// gone, on the main one.
    fn restart(&mut self) -> anyhow::Result<()>;
    /// Re-read the display mode and reconfigure the stream if its native size
    /// changed: `Ok(true)` = reconfigured. `Err` = the stream can't continue.
    fn sync_mode(&mut self) -> anyhow::Result<bool>;
    /// The configured output size.
    fn resolution(&self) -> Resolution;
    fn display_id(&self) -> u32;
}

struct Driver<B> {
    backend: B,
    idle_repeat: Duration,
    mode_check: Duration,
    first_frame_timeout: Duration,
    /// The last image handed out; re-emitted while the stream is quiet.
    last: Option<RawFrame>,
    last_emit: Option<Instant>,
    next_mode_check: Instant,
    /// The first-frame watchdog: armed when a stream starts (or is rebuilt)
    /// and by a live switch/reconfiguration — a stream always delivers one
    /// complete frame then, even of a static screen — and cleared by a frame
    /// of the configured size.
    awaiting_frame: Option<Instant>,
    backoff: Backoff,
    /// When to next try rebuilding a lost stream.
    retry_at: Option<Instant>,
    /// The local user stopped capture: no rebuilds until [`Self::resume`].
    paused: bool,
}

impl<B: Backend> Driver<B> {
    /// Drive `backend`, whose stream has just been started.
    fn new(backend: B) -> Self {
        let now = Instant::now();
        Driver {
            backend,
            idle_repeat: IDLE_REPEAT,
            mode_check: MODE_CHECK,
            first_frame_timeout: FIRST_FRAME_TIMEOUT,
            last: None,
            last_emit: None,
            next_mode_check: now + MODE_CHECK,
            awaiting_frame: Some(now),
            backoff: Backoff::new(REBUILD_MIN, REBUILD_MAX),
            retry_at: None,
            paused: false,
        }
    }

    /// The next frame: a new image if the stream delivers one, otherwise a
    /// repeat of the last image once [`IDLE_REPEAT`] has passed — so this
    /// returns at least that often, whatever state the stream is in.
    fn next_frame(&mut self) -> RawFrame {
        loop {
            let now = Instant::now();
            if self.backend.running() {
                if now >= self.next_mode_check {
                    self.next_mode_check = now + self.mode_check;
                    match self.backend.sync_mode() {
                        Ok(true) => self.awaiting_frame = Some(now),
                        Ok(false) => {}
                        Err(e) => self.lost(&format!("{e:#}")),
                    }
                }
            } else if !self.paused && self.retry_at.is_none_or(|at| now >= at) {
                self.rebuild();
            }

            let wait = self.repeat_wait();
            if self.backend.running() {
                match self.backend.poll(wait) {
                    Poll::Frame(frame) => return self.fresh(frame),
                    Poll::Timeout => {
                        let silent = self
                            .awaiting_frame
                            .is_some_and(|at| at.elapsed() >= self.first_frame_timeout);
                        if silent {
                            self.lost(
                                "no frame after starting, switching or reconfiguring the stream",
                            );
                        }
                    }
                    Poll::Lost(reason) => self.lost(&reason),
                    Poll::UserStopped => self.user_stopped(),
                }
            } else {
                // Down: sleep until the next repeat — or the next rebuild
                // attempt, if one is scheduled (none while paused).
                let until_retry = match self.retry_at {
                    Some(at) if !self.paused => at.saturating_duration_since(Instant::now()),
                    _ => wait,
                };
                std::thread::sleep(wait.min(until_retry));
            }

            if let Some(frame) = self.repeat_if_due() {
                return frame;
            }
        }
    }

    /// Bookkeeping after `Sck::switch_display` (successful or not).
    fn after_switch(&mut self, switched: bool) {
        // Picking a display is an explicit request for video, so it also
        // ends a pause the local user started ([`Self::user_stopped`]).
        self.paused = false;
        if switched {
            // Never show the old display's image as the new one — not even
            // a frame already delivered but not yet handed out.
            self.last = None;
            self.backend.discard_pending();
        }
        if !self.backend.running() {
            // Lost, paused, or torn down by a half-applied switch: rebuild
            // now, on whichever display is current.
            self.retry_at = Some(Instant::now());
        } else if switched {
            self.awaiting_frame = Some(Instant::now());
        }
    }

    fn fresh(&mut self, frame: RawFrame) -> RawFrame {
        // Only a frame of the configured size answers the watchdog: one of
        // the old size was rendered before the switch/reconfiguration took
        // effect, and says nothing about whether the new stream works.
        let r = self.backend.resolution();
        if (frame.width, frame.height) == (r.width, r.height) {
            self.awaiting_frame = None;
        }
        if self.backoff.failures() > 0 {
            info!("capture stream delivering again");
            self.backoff.reset();
        }
        self.last = Some(frame.clone()); // shares the pixel buffer
        self.last_emit = Some(Instant::now());
        frame
    }

    fn repeat_wait(&self) -> Duration {
        self.last_emit.map_or(self.idle_repeat, |at| {
            self.idle_repeat.saturating_sub(at.elapsed())
        })
    }

    fn repeat_if_due(&mut self) -> Option<RawFrame> {
        if self
            .last_emit
            .is_some_and(|at| at.elapsed() < self.idle_repeat)
        {
            return None;
        }
        let frame = match &self.last {
            Some(last) => RawFrame {
                timestamp_micros: unix_micros(),
                ..last.clone()
            },
            None => {
                // Nothing captured yet (stream starting, just switched
                // displays, or paused): black at the configured size keeps
                // frames — and therefore display-switch handling — moving.
                let Some(black) = placeholder(self.backend.resolution()) else {
                    // No size to make one at (not expected: `Sck` refuses
                    // 0×0). Count it as an emit anyway, so the caller waits
                    // a full interval rather than spinning.
                    self.last_emit = Some(Instant::now());
                    return None;
                };
                self.last = Some(black.clone());
                black
            }
        };
        self.last_emit = Some(Instant::now());
        Some(frame)
    }

    /// The local user clicked "Stop Sharing" in macOS's menu-bar recording
    /// indicator. Rebuilding the stream (as for a loss) would silently undo
    /// that within a quarter second, so capture pauses instead, sending
    /// black — the remote side sees capture is off rather than a frozen
    /// screen that looks live. The rule: the pause holds for the current
    /// viewers and ends when a viewer connects or one picks a display
    /// ([`Self::resume`], [`Self::after_switch`]) — an explicit remote
    /// request for video, which the local user can stop again.
    fn user_stopped(&mut self) {
        self.backend.stop();
        self.paused = true;
        self.awaiting_frame = None;
        self.retry_at = None;
        self.last = None;
        warn!(
            "screen capture was stopped from the macOS menu bar (Stop Sharing); \
             sending black until a viewer connects or selects a display"
        );
    }

    /// End a pause the local user started; no-op otherwise.
    fn resume(&mut self) {
        if self.paused {
            info!("resuming screen capture (paused from the macOS menu bar)");
            self.paused = false;
            self.retry_at = Some(Instant::now());
        }
    }

    fn lost(&mut self, reason: &str) {
        self.backend.stop();
        self.awaiting_frame = None;
        let delay = self.backoff.fail();
        self.retry_at = Some(Instant::now() + delay);
        if self.backoff.should_log() {
            warn!(
                reason,
                failures = self.backoff.failures(),
                retry_in_ms = delay.as_millis() as u64,
                "capture stream lost; repeating the last image while rebuilding"
            );
        }
    }

    fn rebuild(&mut self) {
        let before = self.backend.display_id();
        match self.backend.restart() {
            Ok(()) => {
                self.retry_at = None;
                // A new stream sends a first frame even of a static screen.
                self.awaiting_frame = Some(Instant::now());
                let id = self.backend.display_id();
                if id != before {
                    self.last = None; // fell back to another display
                }
                // The backoff resets only when a frame arrives: a stream that
                // starts and immediately dies again keeps backing off.
                if self.backoff.failures() == 0 || self.backoff.should_log() {
                    let r = self.backend.resolution();
                    info!(
                        display_id = id,
                        width = r.width,
                        height = r.height,
                        "capture stream rebuilt"
                    );
                }
            }
            Err(e) => {
                let delay = self.backoff.fail();
                self.retry_at = Some(Instant::now() + delay);
                if self.backoff.should_log() {
                    warn!(
                        error = %format!("{e:#}"),
                        failures = self.backoff.failures(),
                        retry_in_ms = delay.as_millis() as u64,
                        "capture stream rebuild failed"
                    );
                }
            }
        }
    }
}

/// A black frame at `size` (`None` for an empty size).
fn placeholder(size: Resolution) -> Option<RawFrame> {
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let bytes_per_row = size.width as usize * 4;
    Some(RawFrame::from_bgra(
        size.width,
        size.height,
        bytes_per_row,
        vec![0; bytes_per_row * size.height as usize],
        unix_micros(),
    ))
}

/// The capture output size for a display of `native` pixels: `native`
/// itself if it fits `max` (or there is no `max`), otherwise scaled down to
/// fit with its aspect ratio preserved and both sides even (4:2:0 chroma).
/// The limit is orientation-agnostic — `max`'s long side bounds the
/// display's long side — so a portrait 4K display fits a 4096×2304 limit
/// unscaled.
pub fn fit_within(native: (u32, u32), max: Option<Resolution>) -> (u32, u32) {
    let (w, h) = native;
    let Some(max) = max else {
        return native;
    };
    let (long, short) = (
        max.width.max(max.height) as u64,
        max.width.min(max.height) as u64,
    );
    let (max_w, max_h) = if w >= h { (long, short) } else { (short, long) };
    let (w64, h64) = (w as u64, h as u64);
    if w == 0 || h == 0 || (w64 <= max_w && h64 <= max_h) {
        return native;
    }
    // Scale by the tighter of the two ratios (cross-multiplied: exact).
    let (sw, sh) = if w64 * max_h >= h64 * max_w {
        (max_w, h64 * max_w / w64)
    } else {
        (w64 * max_h / h64, max_h)
    };
    let even = |v: u64| ((v as u32) & !1).max(2);
    (even(sw), even(sh))
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit backend
// ---------------------------------------------------------------------------

/// Latest-wins hand-off from SCK's output queue (and its delegate) to the
/// capture thread.
#[derive(Default)]
struct Mailbox {
    state: Mutex<MailboxState>,
    wake: Condvar,
}

#[derive(Default)]
struct MailboxState {
    frame: Option<RawFrame>,
    /// Why the stream ended, once it has (the first reason wins).
    ended: Option<Ended>,
}

#[derive(Debug, PartialEq)]
enum Ended {
    Error(String),
    /// "Stop Sharing" from macOS's menu-bar recording indicator.
    UserStopped,
}

impl From<SCError> for Ended {
    /// What the stream's delegate reports. The error code is kept for the
    /// one end that is not a failure to recover from.
    fn from(e: SCError) -> Self {
        if e.stream_error_code() == Some(SCStreamErrorCode::UserStopped) {
            Ended::UserStopped
        } else {
            Ended::Error(e.to_string())
        }
    }
}

impl Mailbox {
    fn lock(&self) -> MutexGuard<'_, MailboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn put_frame(&self, frame: RawFrame) {
        let replaced = self.lock().frame.replace(frame);
        self.wake.notify_one();
        if replaced.is_some() {
            // (freed here, outside the lock)
            debug!("consumer behind; replaced an unconsumed frame");
        }
    }

    /// The stream ended (reported by its delegate).
    fn end(&self, why: Ended) {
        self.lock().ended.get_or_insert(why);
        self.wake.notify_one();
    }

    /// Drop the pending frame, if any.
    fn discard(&self) {
        let stale = self.lock().frame.take();
        drop(stale); // outside the lock
    }

    /// A pending frame wins over a pending error (it is still a valid image);
    /// the error is reported on the next call.
    fn wait(&self, timeout: Duration) -> Poll {
        let (mut state, _) = self
            .wake
            .wait_timeout_while(self.lock(), timeout, |s| {
                s.frame.is_none() && s.ended.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(frame) = state.frame.take() {
            Poll::Frame(frame)
        } else {
            match state.ended.take() {
                Some(Ended::Error(error)) => Poll::Lost(error),
                Some(Ended::UserStopped) => Poll::UserStopped,
                None => Poll::Timeout,
            }
        }
    }
}

struct MailboxOutput {
    mailbox: Arc<Mailbox>,
    /// Whether this stream's first frame has had its colour tags checked.
    checked: AtomicBool,
}

impl SCStreamOutputTrait for MailboxOutput {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        // Only complete frames carry a new image: on a static screen SCK
        // sends `.idle` samples with no pixels, and `.blank`/`.suspended`/
        // `.stopped` carry nothing worth showing. An unreadable status falls
        // through to the image check — and in screencapturekit 7.0.1 it is
        // always unreadable (its bridge casts the NSNumber attachment `as?
        // SCFrameStatus`, which fails, so `frame_status()` is `None`), making
        // "has an image buffer" the test that actually applies today.
        if sample.frame_status().is_some_and(|s| !s.has_content()) {
            return;
        }
        let Some(buffer) = sample.image_buffer() else {
            return; // idle (static screen) or metadata-only sample
        };
        // Keep SCK's IOSurface-backed buffer itself — no copy. The crate's
        // wrapper is an owned CVPixelBufferRef; retain it as the objc2 type
        // the encoders use (the wrapper's own reference drops right after).
        // SAFETY: `as_ptr` is a valid CVPixelBufferRef while `buffer` lives,
        // and a delivered frame is complete: SCK never writes it again (it
        // recycles the IOSurface only once nothing retains it).
        let pixels = unsafe {
            PixelBuffer::retain(&*buffer.as_ptr().cast::<objc2_core_video::CVPixelBuffer>())
        };
        drop(buffer);
        if !self.checked.swap(true, Ordering::Relaxed) {
            let matrix = pixels.ycbcr_matrix();
            debug!(?pixels, ?matrix, "first capture frame");
            if pixels.is_nv12() && !pixels.is_bt709() {
                // VideoToolbox converts to the stream's BT.709 (correct
                // colours, at the cost of a conversion pass).
                warn!(?matrix, "capture NV12 is not tagged BT.709");
            }
        }
        self.mailbox
            .put_frame(RawFrame::from_pixel_buffer(pixels, unix_micros()));
    }
}

/// One running SCStream and the mailbox its output handler and delegate
/// feed. Dropping it stops the capture.
struct LiveStream {
    stream: SCStream,
    mailbox: Arc<Mailbox>,
}

impl LiveStream {
    fn start(
        display: &SCDisplay,
        width: u32,
        height: u32,
        fps: u32,
        format: CaptureFormat,
    ) -> anyhow::Result<Self> {
        let mailbox = Arc::new(Mailbox::default());
        let errors = Arc::clone(&mailbox);
        // The delegate is what makes a dead stream visible: without one, SCK
        // errors only reach stderr and the capture silently stops.
        let mut stream = SCStream::new_with_delegate(
            &display_filter(display),
            &capture_config(width, height, fps, format),
            ErrorHandler::new(move |e: SCError| errors.end(Ended::from(e))),
        );
        let output = MailboxOutput {
            mailbox: Arc::clone(&mailbox),
            checked: AtomicBool::new(false),
        };
        if stream
            .add_output_handler(output, SCStreamOutputType::Screen)
            .is_none()
        {
            bail!("failed to attach the capture output handler");
        }
        stream
            .start_capture()
            .map_err(|e| anyhow!("failed to start capture: {e}"))?;
        Ok(LiveStream { stream, mailbox })
    }
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        // A stream that died on its own is already stopped: expected to fail.
        if let Err(e) = self.stream.stop_capture() {
            debug!("stop_capture: {e}");
        }
    }
}

/// The ScreenCaptureKit [`Backend`].
struct Sck {
    live: Option<LiveStream>,
    fps: u32,
    format: CaptureFormat,
    /// Cap on the output size ([`fit_within`]).
    max_size: Option<Resolution>,
    display_id: u32,
    /// The configured output size.
    resolution: Resolution,
}

impl Sck {
    /// Start capturing the main display (or the first one listed).
    fn start(
        fps: u32,
        format: CaptureFormat,
        max_size: Option<Resolution>,
    ) -> anyhow::Result<Self> {
        let content = shareable_content()?;
        let displays = content.displays();
        let main_id = CGDisplay::main().id;
        let display = displays
            .iter()
            .find(|d| d.display_id() == main_id)
            .or_else(|| displays.first())
            .ok_or(anyhow::Error::new(NoDisplays))?;
        let mut sck = Sck {
            live: None,
            fps,
            format,
            max_size,
            display_id: display.display_id(),
            resolution: Resolution {
                width: 0,
                height: 0,
            },
        };
        sck.start_on(display)?;
        Ok(sck)
    }

    /// The output size for display `id`: its native pixels, capped. 0×0
    /// when the display has no mode (asleep or going offline).
    fn output_size(&self, id: u32) -> (u32, u32) {
        fit_within(native_pixels(id), self.max_size)
    }

    fn start_on(&mut self, display: &SCDisplay) -> anyhow::Result<()> {
        let id = display.display_id();
        let (width, height) = self.output_size(id);
        if width == 0 || height == 0 {
            // Transient, like an empty display list: retried with backoff.
            return Err(anyhow::Error::new(NoDisplays))
                .with_context(|| format!("display {id} reports no size"));
        }
        self.live = Some(LiveStream::start(
            display,
            width,
            height,
            self.fps,
            self.format,
        )?);
        self.display_id = id;
        self.resolution = Resolution { width, height };
        Ok(())
    }

    fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
        let content = shareable_content().context("enumerate displays for switch")?;
        let displays = content.displays();
        let display = displays
            .iter()
            .find(|d| d.display_id() == id)
            .ok_or_else(|| anyhow!("display {id} not available"))?;
        let (width, height) = self.output_size(id);
        if width == 0 || height == 0 {
            bail!("display {id} reports no size (asleep or going offline?)");
        }
        if let Some(live) = &self.live {
            // Live swap — no stream recreation (screencapturekit 7.0.1).
            live.stream
                .update_content_filter(&display_filter(display))
                .map_err(|e| anyhow!("update_content_filter: {e}"))?;
            if let Err(e) = live.stream.update_configuration(&capture_config(
                width,
                height,
                self.fps,
                self.format,
            )) {
                // Half-applied (new display, old size): drop the stream so
                // it is rebuilt cleanly on the display we still report.
                self.live = None;
                bail!("update_configuration: {e}");
            }
        }
        // Not running (lost): the driver rebuilds it on this display.
        self.display_id = id;
        self.resolution = Resolution { width, height };
        Ok(())
    }
}

impl Backend for Sck {
    fn running(&self) -> bool {
        self.live.is_some()
    }

    fn poll(&mut self, timeout: Duration) -> Poll {
        match &self.live {
            Some(live) => live.mailbox.wait(timeout),
            None => {
                std::thread::sleep(timeout);
                Poll::Timeout
            }
        }
    }

    fn discard_pending(&mut self) {
        if let Some(live) = &self.live {
            live.mailbox.discard();
        }
    }

    fn stop(&mut self) {
        self.live = None;
    }

    fn restart(&mut self) -> anyhow::Result<()> {
        self.live = None;
        let content = shareable_content()?;
        let displays = content.displays();
        // The captured display may have been unplugged: fall back to the
        // main one (as the Windows capturer does).
        let main_id = CGDisplay::main().id;
        let display = displays
            .iter()
            .find(|d| d.display_id() == self.display_id)
            .or_else(|| displays.iter().find(|d| d.display_id() == main_id))
            .or_else(|| displays.first())
            .ok_or(anyhow::Error::new(NoDisplays))?;
        self.start_on(display)
    }

    fn sync_mode(&mut self) -> anyhow::Result<bool> {
        let Some(live) = &self.live else {
            return Ok(false);
        };
        if !CGDisplay::new(self.display_id).is_online() {
            bail!("display {} went offline", self.display_id);
        }
        let (width, height) = self.output_size(self.display_id);
        let current = (self.resolution.width, self.resolution.height);
        if width == 0 || height == 0 || (width, height) == current {
            return Ok(false);
        }
        info!(
            display_id = self.display_id,
            from = ?current,
            to = ?(width, height),
            "display mode changed; reconfiguring capture"
        );
        live.stream
            .update_configuration(&capture_config(width, height, self.fps, self.format))
            .map_err(|e| anyhow!("update_configuration: {e}"))?;
        self.resolution = Resolution { width, height };
        Ok(true)
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn display_id(&self) -> u32 {
        self.display_id
    }
}

fn shareable_content() -> anyhow::Result<SCShareableContent> {
    SCShareableContent::get().map_err(|e| {
        anyhow!(
            "ScreenCaptureKit refused ({e}). Likely missing Screen Recording \
             permission: System Settings → Privacy & Security → Screen Recording \
             (macOS applies a new grant when tetherd restarts)"
        )
    })
}

/// Native backing-pixel dimensions for a display id (the SCDisplay/
/// CGDisplayPixelsWide values are the *scaled* logical size; the display mode
/// carries the true pixels — the gate's native-resolution criterion).
fn native_pixels(display_id: u32) -> (u32, u32) {
    let cg = CGDisplay::new(display_id);
    match cg.display_mode() {
        Some(mode) => (mode.pixel_width() as u32, mode.pixel_height() as u32),
        None => (cg.pixels_wide() as u32, cg.pixels_high() as u32),
    }
}

fn display_filter(display: &SCDisplay) -> SCContentFilter {
    SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build()
}

/// The colour matrix is left at SCK's default, which for YCbCr output is
/// BT.709 — what the H.264 stream signals (checked on each stream's first
/// frame, [`MailboxOutput`]). It is deliberately not pinned with
/// `with_color_matrix`: SCK holds that `CFStringRef` unretained (an `assign`
/// property), and screencapturekit 7.0.1 keeps the string alive only as long
/// as the configuration object, so a stream outliving its configuration
/// (as here) crashes on start.
fn capture_config(px_w: u32, px_h: u32, fps: u32, format: CaptureFormat) -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(px_w)
        .with_height(px_h)
        .with_pixel_format(format.sck())
        .with_queue_depth(QUEUE_DEPTH)
        .with_fps(fps)
        .with_shows_cursor(true)
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, Sender, channel};

    const MS: Duration = Duration::from_millis(1);

    fn frame(tag: u8, width: u32) -> RawFrame {
        RawFrame::from_bgra(
            width,
            2,
            width as usize * 4,
            vec![tag; width as usize * 4 * 2],
            unix_micros(),
        )
    }

    fn is_black(f: &RawFrame) -> bool {
        f.with_bgra(|px, _| px.iter().all(|&b| b == 0)).unwrap()
    }

    /// Scripted backend: frames come from a channel the test feeds; losses,
    /// rebuild outcomes and mode changes are queued up front.
    struct Fake {
        frames: Receiver<RawFrame>,
        running: bool,
        lose: VecDeque<String>,
        /// The next poll reports "Stop Sharing" from the menu bar.
        user_stop: bool,
        restart_ok: VecDeque<bool>,
        restarts: Vec<Instant>,
        /// A successful restart lands here (the captured display vanished).
        restart_onto: Option<(u32, Resolution)>,
        mode_changes: VecDeque<Resolution>,
        display_id: u32,
        resolution: Resolution,
    }

    impl Backend for Fake {
        fn running(&self) -> bool {
            self.running
        }
        fn poll(&mut self, timeout: Duration) -> Poll {
            if std::mem::take(&mut self.user_stop) {
                return Poll::UserStopped;
            }
            if let Some(reason) = self.lose.pop_front() {
                return Poll::Lost(reason);
            }
            match self.frames.recv_timeout(timeout) {
                Ok(f) => Poll::Frame(f),
                Err(_) => Poll::Timeout,
            }
        }
        fn discard_pending(&mut self) {
            while self.frames.try_recv().is_ok() {}
        }
        fn stop(&mut self) {
            self.running = false;
        }
        fn restart(&mut self) -> anyhow::Result<()> {
            self.restarts.push(Instant::now());
            if self.restart_ok.pop_front().unwrap_or(true) {
                self.running = true;
                if let Some((id, resolution)) = self.restart_onto.take() {
                    self.display_id = id;
                    self.resolution = resolution;
                }
                Ok(())
            } else {
                bail!("fake: display still gone")
            }
        }
        fn sync_mode(&mut self) -> anyhow::Result<bool> {
            Ok(match self.mode_changes.pop_front() {
                Some(r) => {
                    self.resolution = r;
                    true
                }
                None => false,
            })
        }
        fn resolution(&self) -> Resolution {
            self.resolution
        }
        fn display_id(&self) -> u32 {
            self.display_id
        }
    }

    /// A driver over a running fake with test-speed timings.
    fn driver() -> (Driver<Fake>, Sender<RawFrame>) {
        let (tx, rx) = channel();
        let fake = Fake {
            frames: rx,
            running: true,
            lose: VecDeque::new(),
            user_stop: false,
            restart_ok: VecDeque::new(),
            restarts: Vec::new(),
            restart_onto: None,
            mode_changes: VecDeque::new(),
            display_id: 1,
            resolution: Resolution {
                width: 8,
                height: 2,
            },
        };
        let mut d = Driver::new(fake);
        d.idle_repeat = 40 * MS;
        d.mode_check = Duration::from_secs(3600);
        // Off unless a test is about the watchdog ([`WATCHDOG`]): a short one
        // fires on any scheduler stall of a loaded CI machine and tears down
        // streams the test expects to stay up.
        d.first_frame_timeout = Duration::from_secs(3600);
        d.backoff = Backoff::new(20 * MS, 80 * MS);
        (d, tx)
    }

    /// The first-frame watchdog, short enough for a test to see it fire.
    const WATCHDOG: Duration = Duration::from_millis(150);

    #[test]
    fn repeats_the_last_frame_while_idle_without_copying() {
        let (mut d, tx) = driver();
        let first = frame(7, 8);
        tx.send(first.clone()).unwrap();
        let got = d.next_frame();
        assert!(got.same_image(&first));

        for _ in 0..3 {
            let t = Instant::now();
            let repeat = d.next_frame();
            let waited = t.elapsed();
            assert!(repeat.same_image(&first), "repeat must share the buffer");
            assert!(waited >= 35 * MS, "repeated too early: {waited:?}");
            assert!(waited < 2000 * MS, "repeat too late: {waited:?}");
            assert!(repeat.timestamp_micros >= first.timestamp_micros);
        }
    }

    #[test]
    fn a_new_frame_preempts_the_repeat() {
        let (mut d, tx) = driver();
        // A long repeat interval, so "held back until the repeat" is
        // distinguishable from a slow machine.
        d.idle_repeat = Duration::from_secs(2);
        tx.send(frame(1, 8)).unwrap();
        d.next_frame();
        let newer = frame(2, 8);
        tx.send(newer.clone()).unwrap();
        let t = Instant::now();
        assert!(d.next_frame().same_image(&newer));
        assert!(t.elapsed() < 1000 * MS, "new frame was held back");
    }

    #[test]
    fn black_placeholder_before_the_first_frame() {
        let (mut d, _tx) = driver();
        let f = d.next_frame();
        assert_eq!((f.width, f.height), (8, 2));
        assert!(is_black(&f));
        // and it repeats like any other image
        assert!(d.next_frame().same_image(&f));
    }

    #[test]
    fn lost_stream_is_rebuilt_with_backoff_while_the_image_repeats() {
        let (mut d, tx) = driver();
        let first = frame(3, 8);
        tx.send(first.clone()).unwrap();
        d.next_frame();

        d.backend.lose.push_back("fake: stream stopped".into());
        d.backend.restart_ok.extend([false, false, true]);
        // While down, next_frame keeps returning the frozen image on time.
        let deadline = Instant::now() + Duration::from_secs(2);
        while d.backend.restarts.len() < 3 {
            assert!(Instant::now() < deadline, "rebuild never retried");
            let t = Instant::now();
            assert!(d.next_frame().same_image(&first));
            assert!(t.elapsed() < 1000 * MS, "blocked instead of repeating");
        }
        assert!(d.backend.running);
        // Backoff: 20 ms before the first attempt, then 40, then 80.
        let r = &d.backend.restarts;
        assert!(r[1] - r[0] >= 35 * MS, "{:?}", r[1] - r[0]);
        assert!(r[2] - r[1] >= 70 * MS, "{:?}", r[2] - r[1]);

        // Delivering again resets the backoff.
        let after = frame(4, 8);
        tx.send(after.clone()).unwrap();
        assert!(d.next_frame().same_image(&after));
        assert_eq!(d.backoff.failures(), 0);
    }

    #[test]
    fn fallback_to_another_display_drops_the_old_image() {
        let (mut d, tx) = driver();
        tx.send(frame(5, 8)).unwrap();
        d.next_frame();
        d.backend.lose.push_back("fake: display unplugged".into());
        d.backend.restart_onto = Some((
            2,
            Resolution {
                width: 4,
                height: 2,
            },
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let f = d.next_frame();
            if f.width == 4 {
                assert!(is_black(&f), "old display's image shown");
                break;
            }
            assert!(Instant::now() < deadline);
        }
    }

    #[test]
    fn mode_change_with_a_silent_stream_triggers_a_rebuild() {
        let (mut d, tx) = driver();
        d.first_frame_timeout = WATCHDOG;
        tx.send(frame(6, 8)).unwrap();
        d.next_frame();
        d.backend.mode_changes.push_back(Resolution {
            width: 16,
            height: 2,
        });
        d.mode_check = 10 * MS;
        d.next_mode_check = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        while d.backend.restarts.is_empty() {
            assert!(Instant::now() < deadline, "silent stream never rebuilt");
            d.next_frame();
        }
        // ...whereas a reconfiguration answered by a frame does not.
        let (mut d, tx) = driver();
        d.first_frame_timeout = WATCHDOG;
        tx.send(frame(6, 8)).unwrap();
        d.next_frame();
        d.backend.mode_changes.push_back(Resolution {
            width: 16,
            height: 2,
        });
        d.next_mode_check = Instant::now();
        tx.send(frame(7, 16)).unwrap();
        assert_eq!(d.next_frame().width, 16);
        let until = Instant::now() + 300 * MS;
        while Instant::now() < until {
            d.next_frame();
        }
        assert!(d.backend.restarts.is_empty());
    }

    #[test]
    fn switch_drops_the_old_image_and_rebuilds_if_the_stream_is_down() {
        let (mut d, tx) = driver();
        tx.send(frame(8, 8)).unwrap();
        d.next_frame();
        d.backend.display_id = 2;
        d.backend.resolution = Resolution {
            width: 4,
            height: 2,
        };
        d.after_switch(true);
        let f = d.next_frame();
        assert_eq!(f.width, 4, "old display's image repeated after a switch");

        d.backend.running = false;
        d.retry_at = Some(Instant::now() + Duration::from_secs(60)); // deep in backoff
        d.after_switch(true);
        d.next_frame();
        assert_eq!(d.backend.restarts.len(), 1, "switch must rebuild now");
    }

    #[test]
    fn stale_frame_from_before_a_switch_is_discarded_and_does_not_satisfy_the_watchdog() {
        let (mut d, tx) = driver();
        d.first_frame_timeout = WATCHDOG;
        tx.send(frame(1, 8)).unwrap();
        d.next_frame();
        // An old-display frame is pending when the switch lands...
        tx.send(frame(2, 8)).unwrap();
        d.backend.display_id = 2;
        d.backend.resolution = Resolution {
            width: 4,
            height: 2,
        };
        d.after_switch(true);
        let f = d.next_frame();
        assert_eq!(
            f.width, 4,
            "pending old-display frame shown after the switch"
        );
        assert!(is_black(&f));
        // ...and another, of the old size, was still in flight. It must not
        // count as the new stream's first frame: that stream stays silent,
        // so the watchdog still rebuilds it.
        tx.send(frame(3, 8)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while d.backend.restarts.is_empty() {
            assert!(
                Instant::now() < deadline,
                "stale frame disarmed the watchdog"
            );
            d.next_frame();
        }
    }

    #[test]
    fn watchdog_is_armed_at_start_and_after_a_rebuild() {
        let (mut d, _tx) = driver();
        d.first_frame_timeout = WATCHDOG;
        let deadline = Instant::now() + Duration::from_secs(2);
        // A stream that never sends its first frame is rebuilt...
        while d.backend.restarts.is_empty() {
            assert!(Instant::now() < deadline, "silent new stream never rebuilt");
            d.next_frame();
        }
        // ...and so is a rebuilt one that stays silent.
        while d.backend.restarts.len() < 2 {
            assert!(
                Instant::now() < deadline,
                "silent rebuilt stream left alone"
            );
            d.next_frame();
        }
        // A first frame disarms it.
        let (mut d, tx) = driver();
        d.first_frame_timeout = WATCHDOG;
        tx.send(frame(1, 8)).unwrap();
        d.next_frame();
        let until = Instant::now() + 300 * MS;
        while Instant::now() < until {
            d.next_frame();
        }
        assert!(d.backend.restarts.is_empty());
    }

    #[test]
    fn no_placeholder_size_waits_instead_of_spinning() {
        let (mut d, _tx) = driver();
        d.backend.resolution = Resolution {
            width: 0,
            height: 0,
        };
        assert!(d.repeat_if_due().is_none());
        let wait = d.repeat_wait();
        assert!(wait >= 30 * MS, "would busy-spin: next wait {wait:?}");
    }

    #[test]
    fn user_stop_pauses_capture_until_a_viewer_joins_or_picks_a_display() {
        let (mut d, tx) = driver();
        tx.send(frame(9, 8)).unwrap();
        d.next_frame();

        d.backend.user_stop = true;
        // Black from now on, still on time, and never restarted behind the
        // user's back (a loss would be retried within 20 ms here).
        let until = Instant::now() + 300 * MS;
        while Instant::now() < until {
            let t = Instant::now();
            let f = d.next_frame();
            assert!(is_black(&f), "kept showing the screen after Stop Sharing");
            assert!(t.elapsed() < 1000 * MS, "frames stopped while paused");
        }
        assert!(
            d.backend.restarts.is_empty(),
            "restarted a user-stopped stream"
        );
        assert!(!d.backend.running);

        // A viewer connecting resumes capture.
        d.resume();
        d.next_frame();
        assert_eq!(d.backend.restarts.len(), 1);
        assert!(d.backend.running);

        // So does picking a display.
        d.backend.user_stop = true;
        d.next_frame();
        assert!(!d.backend.running);
        d.after_switch(true);
        d.next_frame();
        assert_eq!(d.backend.restarts.len(), 2);
        assert!(d.backend.running);

        // resume() without a pause is a no-op.
        d.resume();
        d.next_frame();
        assert_eq!(d.backend.restarts.len(), 2);
    }

    #[test]
    fn stream_errors_keep_their_code_and_user_stop_is_told_apart() {
        let stopped = SCError::from_error_code(SCStreamErrorCode::UserStopped.as_raw());
        assert_eq!(SCStreamErrorCode::UserStopped.as_raw(), -3817);
        assert_eq!(Ended::from(stopped), Ended::UserStopped);
        let lost = SCError::from_error_code(SCStreamErrorCode::NoCaptureSource.as_raw());
        assert!(matches!(Ended::from(lost), Ended::Error(e) if e.contains("No capture source")));
        let other = SCError::StreamError("boom".into());
        assert!(matches!(Ended::from(other), Ended::Error(_)));

        let mb = Mailbox::default();
        mb.end(Ended::UserStopped);
        assert!(matches!(mb.wait(5 * MS), Poll::UserStopped));
    }

    #[test]
    fn fit_within_keeps_native_sizes_that_fit_and_scales_the_rest() {
        let h264 = Some(Resolution {
            width: 4096,
            height: 2304,
        });
        // fits (or no cap): untouched
        assert_eq!(fit_within((3420, 2214), h264), (3420, 2214));
        assert_eq!(fit_within((3840, 2160), h264), (3840, 2160));
        assert_eq!(fit_within((4096, 2304), h264), (4096, 2304));
        assert_eq!(fit_within((6016, 3384), None), (6016, 3384));
        assert_eq!(fit_within((0, 0), h264), (0, 0));
        // portrait 4K: the long-side limit applies to its long side
        assert_eq!(fit_within((2160, 3840), h264), (2160, 3840));
        // 5K and 6K (16:9): width-bound
        assert_eq!(fit_within((5120, 2880), h264), (4096, 2304));
        assert_eq!(fit_within((6016, 3384), h264), (4096, 2304));
        // "More Space" on a 16" MacBook Pro (2056×1329 pt at 2×): height-bound
        assert_eq!(fit_within((4112, 2658), h264), (3564, 2304));
        // ultrawide: width-bound
        assert_eq!(fit_within((5120, 1440), h264), (4096, 1152));
        // 4:3 taller than the limit: height-bound
        assert_eq!(fit_within((3200, 2400), h264), (3072, 2304));
        // portrait 5K
        assert_eq!(fit_within((2880, 5120), h264), (2304, 4096));
        for native in [(5119, 2879), (7001, 3001), (4097, 4097), (9999, 3)] {
            let (w, h) = fit_within(native, h264);
            assert!(w % 2 == 0 && h % 2 == 0, "{native:?} -> odd {w}x{h}");
            assert!(
                w <= 4096 && h <= 4096 && w.min(h) <= 2304,
                "{native:?} -> {w}x{h}"
            );
            let (a, b) = (native.0 as f64 / native.1 as f64, w as f64 / h as f64);
            assert!(
                (a - b).abs() / a < 0.01 || h <= 4,
                "{native:?} -> {w}x{h} aspect"
            );
        }
    }

    #[test]
    fn mailbox_is_latest_wins_and_reports_errors_after_frames() {
        let mb = Mailbox::default();
        assert!(matches!(mb.wait(5 * MS), Poll::Timeout));
        let (a, b) = (frame(1, 8), frame(2, 8));
        mb.put_frame(a);
        mb.put_frame(b.clone());
        mb.end(Ended::Error("boom".into()));
        match mb.wait(5 * MS) {
            Poll::Frame(f) => assert!(f.same_image(&b), "older frame kept"),
            other => panic!("expected a frame, got {other:?}"),
        }
        assert!(matches!(mb.wait(5 * MS), Poll::Lost(e) if e == "boom"));
        assert!(matches!(mb.wait(5 * MS), Poll::Timeout));
        // discard drops a pending frame
        mb.put_frame(frame(3, 8));
        mb.discard();
        assert!(matches!(mb.wait(5 * MS), Poll::Timeout));
    }

    #[test]
    fn mailbox_wakes_the_waiter_promptly() {
        let mb = Arc::new(Mailbox::default());
        let producer = Arc::clone(&mb);
        let t = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(20 * MS);
            producer.end(Ended::Error("stream stopped".into()));
        });
        assert!(matches!(mb.wait(Duration::from_secs(5)), Poll::Lost(_)));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
