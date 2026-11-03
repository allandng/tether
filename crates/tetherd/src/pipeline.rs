use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tether_protocol::{Codec, DisplayInfo, Resolution};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::capture::{Backoff, EncodedFrame, FrameEncoder, ImageId, ScreenCapturer};

/// Handles the session layer reads frames/resolution from.
pub struct Pipeline {
    pub resolution: watch::Receiver<Resolution>,
    pub frames: watch::Receiver<Option<EncodedFrame>>,
    /// Requested encoder bitrate (kbps); the adaptive-bitrate loop writes it,
    /// the encode loop applies it. 0 = leave at the codec default.
    pub bitrate: Arc<AtomicU32>,
    /// The host's displays (with the active one flagged); republished on switch.
    pub displays: watch::Receiver<Vec<DisplayInfo>>,
    /// Request a switch of the active capture to a display id.
    pub select_display: std::sync::mpsc::Sender<u32>,
}

/// Backoff between attempts to (re)build the capturer: from
/// [`CAPTURE_RETRY_MIN`] doubling up to [`CAPTURE_RETRY_MAX`]. The cap bounds
/// how long after a display wakes (or Screen Recording is granted) capture
/// resumes.
const CAPTURE_RETRY_MIN: Duration = Duration::from_secs(1);
const CAPTURE_RETRY_MAX: Duration = Duration::from_secs(3);

/// JPEG frames are self-contained, so an unchanged image (the capturer's
/// idle repeat) is only re-sent this often — enough for a joiner or a lost
/// chunk to recover, without streaming megabytes of identical JPEGs. H.264
/// encodes every repeat: they are nearly free and carry the keyframe cadence.
const JPEG_IDLE_REFRESH: Duration = Duration::from_secs(1);

/// Start the capture→encode loop on a dedicated thread.
///
/// Capturer and encoder are *built inside* the thread (platform handles are
/// not generally `Send`). Only an encoder construction failure fails daemon
/// startup. A capturer that can't be built — no display (asleep, locked,
/// clamshell), or no Screen Recording permission — is retried in the
/// background with backoff, as is one whose `next_frame` fails mid-stream:
/// the daemon (and its control socket) must stay reachable, and under
/// launchd `KeepAlive` a hard exit would just be a crash loop. Until capture
/// works the published resolution stays 0×0 and no frames flow.
pub fn start<C, E>(
    mut make_capturer: impl FnMut() -> anyhow::Result<C> + Send + 'static,
    make_encoder: impl FnOnce() -> anyhow::Result<E> + Send + 'static,
) -> anyhow::Result<Pipeline>
where
    C: ScreenCapturer,
    E: FrameEncoder,
{
    let (resolution_tx, resolution_rx) = watch::channel(Resolution {
        width: 0,
        height: 0,
    });
    let (frames_tx, frames_rx) = watch::channel(None);
    let (displays_tx, displays_rx) = watch::channel(Vec::<DisplayInfo>::new());
    let (select_tx, select_rx) = std::sync::mpsc::channel::<u32>();
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
    let bitrate = Arc::new(AtomicU32::new(0));
    let out = Outputs {
        resolution: resolution_tx,
        frames: frames_tx,
        displays: displays_tx,
        select: select_rx,
        bitrate: Arc::clone(&bitrate),
    };

    std::thread::Builder::new()
        .name("tether-capture".into())
        .spawn(move || {
            let mut encoder = match make_encoder() {
                Ok(e) => e,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            let _ = init_tx.send(Ok(()));

            let mut backoff = Backoff::new(CAPTURE_RETRY_MIN, CAPTURE_RETRY_MAX);
            let mut seq: u32 = 0;
            loop {
                let Some(mut capturer) = open_capturer(&mut make_capturer, &mut backoff, &out)
                else {
                    return; // shutting down
                };
                match stream(&mut capturer, &mut encoder, &out, &mut seq, &mut backoff) {
                    Ok(()) => return, // all receivers gone: daemon shutting down
                    Err(e) => {
                        // Platform capturers recover from lost streams
                        // internally, so this is rare: start over with a
                        // fresh capturer rather than ending capture for good.
                        let delay = backoff.fail();
                        if backoff.should_log() {
                            error!(
                                error = %format!("{e:#}"),
                                failures = backoff.failures(),
                                retry_in_ms = delay.as_millis() as u64,
                                "capture failed; rebuilding the capturer"
                            );
                        }
                        drop(capturer);
                        std::thread::sleep(delay);
                    }
                }
            }
        })?;

    init_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("capture thread died during startup"))??;
    Ok(Pipeline {
        resolution: resolution_rx,
        frames: frames_rx,
        bitrate,
        displays: displays_rx,
        select_display: select_tx,
    })
}

/// The capture thread's ends of the pipeline's channels.
struct Outputs {
    resolution: watch::Sender<Resolution>,
    frames: watch::Sender<Option<EncodedFrame>>,
    displays: watch::Sender<Vec<DisplayInfo>>,
    select: std::sync::mpsc::Receiver<u32>,
    bitrate: Arc<AtomicU32>,
}

/// Build a capturer, retrying with backoff until one works. `None` = every
/// frame receiver is gone (daemon shutting down).
fn open_capturer<C: ScreenCapturer>(
    make_capturer: &mut impl FnMut() -> anyhow::Result<C>,
    backoff: &mut Backoff,
    out: &Outputs,
) -> Option<C> {
    loop {
        if out.frames.is_closed() {
            return None;
        }
        let e = match make_capturer() {
            Ok(c) => return Some(c),
            Err(e) => e,
        };
        let delay = backoff.fail();
        if backoff.should_log() {
            let retry_in_ms = delay.as_millis() as u64;
            let failures = backoff.failures();
            if is_transient(&e) {
                info!(error = %e, failures, retry_in_ms, "no display to capture yet; retrying in the background");
            } else {
                error!(
                    error = %format!("{e:#}"),
                    failures,
                    retry_in_ms,
                    "screen capture unavailable; retrying in the background (the daemon stays up)"
                );
            }
        }
        std::thread::sleep(delay);
    }
}

/// Capture→encode→publish until the capturer fails (`Err`) or every frame
/// receiver is gone (`Ok`).
fn stream<C: ScreenCapturer, E: FrameEncoder>(
    capturer: &mut C,
    encoder: &mut E,
    out: &Outputs,
    seq: &mut u32,
    backoff: &mut Backoff,
) -> anyhow::Result<()> {
    let mut resolution = capturer.resolution();
    let _ = out.resolution.send(resolution);
    let mut active = capturer.active_display();
    let _ = out.displays.send(capturer.displays());
    info!(
        width = resolution.width,
        height = resolution.height,
        "capture started"
    );

    // Identity of the last frame's image (weak, so it doesn't keep ~20 MB —
    // or one of ScreenCaptureKit's few pool buffers — alive): the same image
    // again is the capturer's idle repeat.
    let mut prev_image: Option<ImageId> = None;
    let mut last_publish: Option<Instant> = None;
    // Sessions subscribe to `frames` once their viewer has authenticated, so
    // a rise in the receiver count is a viewer joining.
    let mut viewers = out.frames.receiver_count();
    loop {
        let now_viewers = out.frames.receiver_count();
        if now_viewers > viewers {
            capturer.viewer_joined();
        }
        viewers = now_viewers;

        // Apply any pending display-switch requests (cheap, between
        // frames — and frames keep coming on a static screen, so this is
        // prompt). Drain so only the last request matters.
        let mut switch_to = None;
        while let Ok(id) = out.select.try_recv() {
            switch_to = Some(id);
        }
        if let Some(id) = switch_to {
            match capturer.switch_display(id) {
                Ok(()) => info!(id, "switched active display"),
                // republish either way: on failure the controller's
                // picker still self-corrects to the real active display
                Err(e) => warn!(error = %e, id, "display switch failed"),
            }
            active = capturer.active_display();
            let _ = out.displays.send(capturer.displays());
        }

        let raw = capturer.next_frame()?;
        backoff.reset(); // a delivered frame proves this capturer works
        let current = Resolution {
            width: raw.width,
            height: raw.height,
        };
        let resized = current != resolution;
        if resized {
            info!(?current, "capture resolution changed");
            resolution = current;
            let _ = out.resolution.send(resolution);
        }
        // A capturer whose display was unplugged falls back to another one
        // on its own — possibly of the same size, so a resolution change
        // can't be relied on to reveal it. Republish the display list when
        // the active display or the size changes (a mode change or hot-plug
        // can change the set too), so the picker — and the injector, which
        // targets the active display — follow the video.
        let now_active = capturer.active_display();
        if resized || now_active != active {
            if now_active != active {
                info!(
                    from = active,
                    to = now_active,
                    "capture moved to another display"
                );
                active = now_active;
            }
            let _ = out.displays.send(capturer.displays());
        }

        let image = raw.image_id();
        let repeat = prev_image.as_ref().is_some_and(|p| p.same(&image));
        prev_image = Some(image);
        if repeat
            && encoder.codec() == Codec::Jpeg
            && last_publish.is_some_and(|at| at.elapsed() < JPEG_IDLE_REFRESH)
        {
            continue;
        }

        // apply any adaptive-bitrate request (0 = leave at default)
        let requested = out.bitrate.load(Ordering::Relaxed);
        if requested != 0 {
            encoder.set_bitrate(requested);
        }
        match encoder.encode(&raw) {
            // Rate control skipped this frame (software encoders do
            // under bitrate pressure): nothing to send.
            Ok(payload) if payload.is_empty() => {}
            Ok(payload) => {
                *seq = seq.wrapping_add(1);
                let frame = EncodedFrame {
                    codec: encoder.codec(),
                    seq: *seq,
                    timestamp_micros: raw.timestamp_micros,
                    payload,
                };
                if out.frames.send(Some(frame)).is_err() {
                    return Ok(()); // all receivers gone: daemon shutting down
                }
                last_publish = Some(Instant::now());
            }
            Err(e) => warn!(error = %e, "encode failed, skipping frame"),
        }
    }
}

#[cfg(target_os = "macos")]
fn is_transient(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::capture::macos::NoDisplays>()
        .is_some()
}

#[cfg(windows)]
fn is_transient(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::capture::windows::NoDisplays>()
        .is_some()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn is_transient(_: &anyhow::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::RawFrame;
    use bytes::Bytes;
    use std::sync::atomic::AtomicUsize;

    struct FakeCapturer {
        frames_left: u32,
    }

    impl ScreenCapturer for FakeCapturer {
        fn resolution(&self) -> Resolution {
            Resolution {
                width: 64,
                height: 48,
            }
        }
        fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
            if self.frames_left == 0 {
                anyhow::bail!("fake capture exhausted");
            }
            self.frames_left -= 1;
            Ok(RawFrame::from_bgra(
                64,
                48,
                64 * 4,
                vec![0u8; 64 * 4 * 48],
                1000,
            ))
        }
    }

    struct FakeEncoder;

    impl FrameEncoder for FakeEncoder {
        fn codec(&self) -> Codec {
            Codec::Jpeg
        }
        fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes> {
            Ok(Bytes::from(format!("{}x{}", frame.width, frame.height)))
        }
    }

    #[tokio::test]
    async fn pipeline_publishes_resolution_and_frames() {
        let pipeline = start(|| Ok(FakeCapturer { frames_left: 3 }), || Ok(FakeEncoder)).unwrap();

        let mut resolution = pipeline.resolution.clone();
        let mut frames = pipeline.frames.clone();

        // resolution: either already set or arriving momentarily
        if resolution.borrow_and_update().width == 0 {
            resolution.changed().await.unwrap();
        }
        assert_eq!(
            *resolution.borrow(),
            Resolution {
                width: 64,
                height: 48
            }
        );

        frames.changed().await.unwrap();
        let frame = frames.borrow_and_update().clone().expect("a frame");
        assert_eq!(frame.codec, Codec::Jpeg);
        assert!(frame.seq >= 1);
        assert_eq!(&frame.payload[..], b"64x48");
    }

    #[test]
    fn encoder_init_failure_fails_startup() {
        let result = start(
            || Ok(FakeCapturer { frames_left: 1 }),
            || Err::<FakeEncoder, _>(anyhow::anyhow!("no encoder")),
        );
        let err = result.err().expect("startup must fail");
        assert!(err.to_string().contains("no encoder"));
    }

    /// Missing Screen Recording permission at startup: the daemon must come
    /// up anyway (no crash loop under launchd) and start streaming once the
    /// capturer can be built.
    #[tokio::test]
    async fn capturer_init_failure_retries_in_background() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let pipeline = start(
            move || {
                if counter.fetch_add(1, Ordering::SeqCst) < 1 {
                    Err(anyhow::anyhow!("no permission"))
                } else {
                    Ok(FakeCapturer { frames_left: 3 })
                }
            },
            || Ok(FakeEncoder),
        )
        .expect("a capture failure must not fail startup");

        let mut frames = pipeline.frames.clone();
        // First retry after CAPTURE_RETRY_MIN (1 s); allow some slack.
        tokio::time::timeout(Duration::from_secs(10), frames.changed())
            .await
            .expect("no frame within retry window")
            .unwrap();
        assert!(frames.borrow_and_update().is_some());
        assert!(attempts.load(Ordering::SeqCst) >= 2);
    }

    /// Display asleep at startup: the daemon must come up anyway and start
    /// streaming once a display appears.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn no_displays_at_startup_retries_in_background() {
        use crate::capture::macos::NoDisplays;

        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let pipeline = start(
            move || {
                if counter.fetch_add(1, Ordering::SeqCst) < 1 {
                    Err(anyhow::Error::new(NoDisplays))
                } else {
                    Ok(FakeCapturer { frames_left: 3 })
                }
            },
            || Ok(FakeEncoder),
        )
        .expect("transient no-displays must not fail startup");

        let mut frames = pipeline.frames.clone();
        tokio::time::timeout(Duration::from_secs(10), frames.changed())
            .await
            .expect("no frame within retry window")
            .unwrap();
        assert!(frames.borrow_and_update().is_some());
        assert!(attempts.load(Ordering::SeqCst) >= 2);
    }

    /// A capturer whose stream ends must not end capture for good: the
    /// pipeline builds a new one and frames keep flowing.
    #[tokio::test]
    async fn mid_stream_capture_error_rebuilds_the_capturer() {
        let built = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&built);
        let pipeline = start(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(FakeCapturer { frames_left: 1 })
            },
            || Ok(FakeEncoder),
        )
        .unwrap();

        let mut frames = pipeline.frames.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::time::timeout_at(deadline, frames.changed())
                .await
                .expect("frames stopped after the capturer failed")
                .unwrap();
            let seq = frames.borrow_and_update().as_ref().map_or(0, |f| f.seq);
            if seq >= 2 {
                break;
            }
        }
        assert!(built.load(Ordering::SeqCst) >= 2);
    }

    /// A capturer on a static screen (idle repeats only) and two displays
    /// of the same size.
    struct IdleCapturer {
        image: Arc<Vec<u8>>,
        active: u32,
        frames: u32,
        /// After this many frames, silently fall back to display 1 (as a
        /// platform capturer does when its display is unplugged).
        fall_back_after: Option<u32>,
        viewers_joined: Arc<AtomicUsize>,
        /// Completed `next_frame` calls, for tests that must know the
        /// pipeline loop has gone round (and so sampled its viewers).
        polls: Arc<AtomicUsize>,
    }

    impl ScreenCapturer for IdleCapturer {
        fn resolution(&self) -> Resolution {
            Resolution {
                width: 4,
                height: 4,
            }
        }
        fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
            std::thread::sleep(Duration::from_millis(10));
            self.frames += 1;
            if self.fall_back_after == Some(self.frames) {
                self.active = 1;
            }
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(RawFrame::from_bgra(4, 4, 16, Arc::clone(&self.image), 1))
        }
        fn displays(&self) -> Vec<DisplayInfo> {
            [1, 2]
                .into_iter()
                .map(|id| DisplayInfo {
                    id,
                    width: 4,
                    height: 4,
                    active: id == self.active,
                    name: format!("Display {id}"),
                })
                .collect()
        }
        fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
            self.active = id;
            Ok(())
        }
        fn active_display(&self) -> u32 {
            self.active
        }
        fn viewer_joined(&mut self) {
            self.viewers_joined.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn idle_capturer() -> IdleCapturer {
        IdleCapturer {
            image: Arc::new(vec![0; 64]),
            active: 1,
            frames: 0,
            fall_back_after: None,
            viewers_joined: Arc::new(AtomicUsize::new(0)),
            polls: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn wait_for_active(displays: &mut watch::Receiver<Vec<DisplayInfo>>, id: u32) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if displays
                    .borrow_and_update()
                    .iter()
                    .any(|d| d.active && d.id == id)
                {
                    return;
                }
                displays.changed().await.unwrap();
            }
        })
        .await
        .is_ok()
    }

    /// The captured display vanishes and the capturer falls back to another
    /// one of the same size: no resolution change, yet the display list must
    /// be republished so input follows the video.
    #[tokio::test]
    async fn silent_fallback_to_another_display_is_republished() {
        let pipeline = start(
            || {
                Ok(IdleCapturer {
                    active: 2,
                    fall_back_after: Some(5),
                    ..idle_capturer()
                })
            },
            || Ok(FakeEncoder),
        )
        .unwrap();
        let mut displays = pipeline.displays.clone();
        assert!(wait_for_active(&mut displays, 2).await, "never published");
        assert!(
            wait_for_active(&mut displays, 1).await,
            "fallback to display 1 not republished"
        );
        assert_eq!(
            *pipeline.resolution.borrow(),
            Resolution {
                width: 4,
                height: 4
            }
        );
    }

    /// A viewer subscribing to frames (sessions do once authenticated)
    /// reaches the capturer — how a capturer paused by the local user
    /// learns to resume.
    #[tokio::test]
    async fn a_joining_viewer_is_reported_to_the_capturer() {
        let joined = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let (counter, polled) = (Arc::clone(&joined), Arc::clone(&polls));
        let pipeline = start(
            move || {
                Ok(IdleCapturer {
                    viewers_joined: Arc::clone(&counter),
                    polls: Arc::clone(&polled),
                    ..idle_capturer()
                })
            },
            || Ok(FakeEncoder),
        )
        .unwrap();
        // Two completed captures after this point mean the loop has started
        // an iteration — and sampled the receiver count — since: with only
        // the pipeline's own handle (not a viewer). Timing-free, unlike a
        // sleep, which a stalled CI machine can outlast.
        let mark = polls.load(Ordering::SeqCst);
        wait_until(|| polls.load(Ordering::SeqCst) >= mark + 2).await;
        assert_eq!(
            joined.load(Ordering::SeqCst),
            0,
            "the pipeline's own handle counted as a viewer"
        );

        let _viewer = pipeline.frames.clone();
        wait_until(|| joined.load(Ordering::SeqCst) == 1).await;
    }

    async fn wait_until(done: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition never held");
    }

    #[tokio::test]
    async fn display_switch_applies_on_a_static_screen() {
        let pipeline = start(|| Ok(idle_capturer()), || Ok(FakeEncoder)).unwrap();
        let mut displays = pipeline.displays.clone();
        pipeline.select_display.send(2).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if displays
                    .borrow_and_update()
                    .iter()
                    .any(|d| d.active && d.id == 2)
                {
                    return;
                }
                displays.changed().await.unwrap();
            }
        })
        .await
        .expect("switch not applied");
    }

    struct CountingEncoder {
        codec: Codec,
        encodes: Arc<AtomicUsize>,
    }

    impl FrameEncoder for CountingEncoder {
        fn codec(&self) -> Codec {
            self.codec
        }
        fn encode(&mut self, _frame: &RawFrame) -> anyhow::Result<Bytes> {
            self.encodes.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::from_static(b"x"))
        }
    }

    /// ~100 idle repeats per second: JPEG re-sends the unchanged image about
    /// once a second; H.264 encodes every repeat (keyframe cadence).
    #[tokio::test]
    async fn idle_repeats_are_throttled_for_jpeg_only() {
        let mut counts = Vec::new();
        for codec in [Codec::Jpeg, Codec::H264] {
            let encodes = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&encodes);
            let _pipeline = start(
                || Ok(idle_capturer()),
                move || {
                    Ok(CountingEncoder {
                        codec,
                        encodes: counter,
                    })
                },
            )
            .unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            counts.push(encodes.load(Ordering::SeqCst));
        }
        let (jpeg, h264) = (counts[0], counts[1]);
        assert!(jpeg <= 2, "JPEG re-encoded an unchanged image {jpeg} times");
        assert!(h264 >= 10, "H.264 skipped idle repeats ({h264} encodes)");
    }
}
