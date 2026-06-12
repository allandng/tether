//! macOS screen capture via ScreenCaptureKit.
//!
//! ScreenCaptureKit is push-based (frames arrive on a dispatch-queue
//! callback); this adapter bridges to the pull-based [`ScreenCapturer`] trait
//! with a capacity-1 channel. If the consumer falls behind, frames are
//! dropped at the channel — latest-wins is the desired backpressure for
//! remote desktop.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use core_graphics::display::CGDisplay;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use tether_protocol::Resolution;
use tracing::{debug, warn};

use super::{RawFrame, ScreenCapturer};

struct ChannelOutput {
    tx: SyncSender<RawFrame>,
}

impl SCStreamOutputTrait for ChannelOutput {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        let Some(buffer) = sample.image_buffer() else {
            return; // SCK occasionally delivers metadata-only samples
        };
        let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            warn!("failed to lock pixel buffer, dropping frame");
            return;
        };
        let frame = RawFrame {
            width: guard.width() as u32,
            height: guard.height() as u32,
            bytes_per_row: guard.bytes_per_row(),
            bgra: guard.as_slice().to_vec(),
            timestamp_micros: unix_micros(),
        };
        match self.tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => debug!("consumer behind, dropped frame"),
            Err(TrySendError::Disconnected(_)) => {} // capturer dropped; stream stopping
        }
    }
}

pub struct SckCapturer {
    stream: SCStream,
    rx: Receiver<RawFrame>,
    resolution: Resolution,
}

impl SckCapturer {
    /// Capture the main display at its native pixel resolution.
    pub fn main_display(fps: u32) -> anyhow::Result<Self> {
        let content = SCShareableContent::get().map_err(|e| {
            anyhow!(
                "ScreenCaptureKit refused ({e}). Likely missing Screen Recording \
                 permission: System Settings → Privacy & Security → Screen Recording"
            )
        })?;
        let displays = content.displays();
        let main_id = CGDisplay::main().id;
        let display = displays
            .iter()
            .find(|d| d.display_id() == main_id)
            .or_else(|| displays.first())
            .context("no displays available to capture")?;

        // SCDisplay (and CGDisplayPixelsWide, despite the name) report the
        // scaled mode's logical size; the display *mode* carries the true
        // backing pixel dimensions (gate criterion: native resolution).
        let main = CGDisplay::main();
        let (px_w, px_h) = match main.display_mode() {
            Some(mode) => (mode.pixel_width() as u32, mode.pixel_height() as u32),
            None => (main.pixels_wide() as u32, main.pixels_high() as u32),
        };

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(px_w)
            .with_height(px_h)
            .with_pixel_format(PixelFormat::BGRA)
            .with_fps(fps)
            .with_shows_cursor(true);

        let (tx, rx) = sync_channel(1);
        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(ChannelOutput { tx }, SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow!("failed to start capture: {e}"))?;

        Ok(SckCapturer {
            stream,
            rx,
            resolution: Resolution { width: px_w, height: px_h },
        })
    }
}

impl ScreenCapturer for SckCapturer {
    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        self.rx.recv().context("capture stream ended")
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            warn!("stop_capture failed: {e}");
        }
    }
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
