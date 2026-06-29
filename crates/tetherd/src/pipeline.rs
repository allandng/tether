use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::watch;
use tether_protocol::Resolution;
use tracing::{error, info, warn};

use crate::capture::{EncodedFrame, FrameEncoder, ScreenCapturer};

/// Handles the session layer reads frames/resolution from.
pub struct Pipeline {
    pub resolution: watch::Receiver<Resolution>,
    pub frames: watch::Receiver<Option<EncodedFrame>>,
    /// Requested encoder bitrate (kbps); the adaptive-bitrate loop writes it,
    /// the encode loop applies it. 0 = leave at the codec default.
    pub bitrate: Arc<AtomicU32>,
}

/// How long to wait between capturer construction attempts when the failure
/// is transient (display asleep / screen locked).
const DISPLAY_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

/// Start the capture→encode loop on a dedicated thread.
///
/// Capturer and encoder are *built inside* the thread (platform handles are
/// not generally `Send`). Hard construction errors (e.g. missing Screen
/// Recording permission) are reported synchronously and fail daemon startup;
/// a transient [`NoDisplays`](crate::capture::macos::NoDisplays) condition
/// instead reports success and retries in the background — the daemon must
/// stay reachable while the display sleeps, because an incoming controller's
/// input is what wakes it.
pub fn start<C, E>(
    mut make_capturer: impl FnMut() -> anyhow::Result<C> + Send + 'static,
    make_encoder: impl FnOnce() -> anyhow::Result<E> + Send + 'static,
) -> anyhow::Result<Pipeline>
where
    C: ScreenCapturer,
    E: FrameEncoder,
{
    let (resolution_tx, resolution_rx) = watch::channel(Resolution { width: 0, height: 0 });
    let (frames_tx, frames_rx) = watch::channel(None);
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
    let bitrate = Arc::new(AtomicU32::new(0));
    let bitrate_loop = Arc::clone(&bitrate);

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
            let mut capturer = match make_capturer() {
                Ok(c) => {
                    let _ = init_tx.send(Ok(()));
                    c
                }
                Err(e) if is_transient(&e) => {
                    info!("no display yet ({e}); will keep retrying in the background");
                    let _ = init_tx.send(Ok(()));
                    let mut attempts: u32 = 0;
                    loop {
                        std::thread::sleep(DISPLAY_RETRY);
                        match make_capturer() {
                            Ok(c) => break c,
                            Err(e) => {
                                attempts += 1;
                                if attempts % 20 == 0 {
                                    warn!(error = %e, "still waiting for a display to capture");
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut resolution = capturer.resolution();
            let _ = resolution_tx.send(resolution);
            info!(width = resolution.width, height = resolution.height, "capture started");

            let mut seq: u32 = 0;
            loop {
                let raw = match capturer.next_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        error!(error = %e, "capture ended");
                        return;
                    }
                };
                let current = Resolution { width: raw.width, height: raw.height };
                if current != resolution {
                    info!(?current, "capture resolution changed");
                    resolution = current;
                    let _ = resolution_tx.send(resolution);
                }
                // apply any adaptive-bitrate request (0 = leave at default)
                let requested = bitrate_loop.load(Ordering::Relaxed);
                if requested != 0 {
                    encoder.set_bitrate(requested);
                }
                match encoder.encode(&raw) {
                    Ok(payload) => {
                        seq = seq.wrapping_add(1);
                        let frame = EncodedFrame {
                            codec: encoder.codec(),
                            seq,
                            timestamp_micros: raw.timestamp_micros,
                            payload,
                        };
                        if frames_tx.send(Some(frame)).is_err() {
                            return; // all receivers gone: daemon shutting down
                        }
                    }
                    Err(e) => warn!(error = %e, "encode failed, skipping frame"),
                }
            }
        })?;

    init_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("capture thread died during startup"))??;
    Ok(Pipeline { resolution: resolution_rx, frames: frames_rx, bitrate })
}

#[cfg(target_os = "macos")]
fn is_transient(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::capture::macos::NoDisplays>().is_some()
}

#[cfg(not(target_os = "macos"))]
fn is_transient(_: &anyhow::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::RawFrame;
    use bytes::Bytes;
    use tether_protocol::Codec;

    struct FakeCapturer {
        frames_left: u32,
    }

    impl ScreenCapturer for FakeCapturer {
        fn resolution(&self) -> Resolution {
            Resolution { width: 64, height: 48 }
        }
        fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
            if self.frames_left == 0 {
                anyhow::bail!("fake capture exhausted");
            }
            self.frames_left -= 1;
            Ok(RawFrame {
                width: 64,
                height: 48,
                bytes_per_row: 64 * 4,
                bgra: vec![0u8; 64 * 4 * 48],
                timestamp_micros: 1000,
            })
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
        let pipeline = start(
            || Ok(FakeCapturer { frames_left: 3 }),
            || Ok(FakeEncoder),
        )
        .unwrap();

        let mut resolution = pipeline.resolution.clone();
        let mut frames = pipeline.frames.clone();

        // resolution: either already set or arriving momentarily
        if resolution.borrow_and_update().width == 0 {
            resolution.changed().await.unwrap();
        }
        assert_eq!(
            *resolution.borrow(),
            Resolution { width: 64, height: 48 }
        );

        frames.changed().await.unwrap();
        let frame = frames.borrow_and_update().clone().expect("a frame");
        assert_eq!(frame.codec, Codec::Jpeg);
        assert!(frame.seq >= 1);
        assert_eq!(&frame.payload[..], b"64x48");
    }

    #[test]
    fn capturer_init_failure_fails_startup() {
        let result = start(
            || Err::<FakeCapturer, _>(anyhow::anyhow!("no permission")),
            || Ok(FakeEncoder),
        );
        let err = result.err().expect("startup must fail");
        assert!(err.to_string().contains("no permission"));
    }

    /// Display asleep at startup: the daemon must come up anyway and start
    /// streaming once a display appears.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn no_displays_at_startup_retries_in_background() {
        use crate::capture::macos::NoDisplays;
        use std::sync::atomic::{AtomicU32, Ordering};

        static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
        ATTEMPTS.store(0, Ordering::SeqCst);

        let pipeline = start(
            || {
                if ATTEMPTS.fetch_add(1, Ordering::SeqCst) < 1 {
                    Err(anyhow::Error::new(NoDisplays))
                } else {
                    Ok(FakeCapturer { frames_left: 3 })
                }
            },
            || Ok(FakeEncoder),
        )
        .expect("transient no-displays must not fail startup");

        let mut frames = pipeline.frames.clone();
        // First retry happens after DISPLAY_RETRY (3s); allow some slack.
        tokio::time::timeout(std::time::Duration::from_secs(10), frames.changed())
            .await
            .expect("no frame within retry window")
            .unwrap();
        assert!(frames.borrow_and_update().is_some());
        assert!(ATTEMPTS.load(Ordering::SeqCst) >= 2);
    }
}
