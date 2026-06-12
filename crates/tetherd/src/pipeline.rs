use tokio::sync::watch;
use tether_protocol::Resolution;
use tracing::{error, info, warn};

use crate::capture::{EncodedFrame, FrameEncoder, ScreenCapturer};

/// Handles the session layer reads frames/resolution from.
pub struct Pipeline {
    pub resolution: watch::Receiver<Resolution>,
    pub frames: watch::Receiver<Option<EncodedFrame>>,
}

/// Start the capture→encode loop on a dedicated thread.
///
/// Capturer and encoder are *built inside* the thread (platform handles are
/// not generally `Send`); construction errors are reported synchronously so a
/// missing Screen Recording permission fails daemon startup instead of
/// producing a silent black stream.
pub fn start<C, E>(
    make_capturer: impl FnOnce() -> anyhow::Result<C> + Send + 'static,
    make_encoder: impl FnOnce() -> anyhow::Result<E> + Send + 'static,
) -> anyhow::Result<Pipeline>
where
    C: ScreenCapturer,
    E: FrameEncoder,
{
    let (resolution_tx, resolution_rx) = watch::channel(Resolution { width: 0, height: 0 });
    let (frames_tx, frames_rx) = watch::channel(None);
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);

    std::thread::Builder::new()
        .name("tether-capture".into())
        .spawn(move || {
            let mut capturer = match make_capturer() {
                Ok(c) => c,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            let mut encoder = match make_encoder() {
                Ok(e) => e,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            let _ = init_tx.send(Ok(()));

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
    Ok(Pipeline { resolution: resolution_rx, frames: frames_rx })
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
}
