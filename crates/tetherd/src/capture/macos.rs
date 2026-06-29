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
    fps: u32,
    current_id: u32,
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

fn capture_config(px_w: u32, px_h: u32, fps: u32) -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(px_w)
        .with_height(px_h)
        .with_pixel_format(PixelFormat::BGRA)
        .with_fps(fps)
        .with_shows_cursor(true)
}

impl SckCapturer {
    /// Begin capturing the main display at its native pixel resolution.
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
            .ok_or(anyhow::Error::new(NoDisplays))?;
        let chosen_id = display.display_id();
        let (px_w, px_h) = native_pixels(chosen_id);

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        let (tx, rx) = sync_channel(1);
        let mut stream = SCStream::new(&filter, &capture_config(px_w, px_h, fps));
        stream.add_output_handler(ChannelOutput { tx }, SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow!("failed to start capture: {e}"))?;

        Ok(SckCapturer {
            stream,
            rx,
            resolution: Resolution {
                width: px_w,
                height: px_h,
            },
            fps,
            current_id: chosen_id,
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

    fn displays(&self) -> Vec<tether_protocol::DisplayInfo> {
        let Ok(content) = SCShareableContent::get() else {
            // transient (asleep/locked) — report the one we're on
            return vec![tether_protocol::DisplayInfo {
                id: self.current_id,
                width: self.resolution.width,
                height: self.resolution.height,
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
                tether_protocol::DisplayInfo {
                    id,
                    width: w,
                    height: h,
                    active: id == self.current_id,
                    name: format!("Display {}", i + 1),
                }
            })
            .collect()
    }

    fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
        if id == self.current_id {
            return Ok(());
        }
        let content = SCShareableContent::get().context("enumerate displays for switch")?;
        let displays = content.displays();
        let display = displays
            .iter()
            .find(|d| d.display_id() == id)
            .ok_or_else(|| anyhow!("display {id} not available"))?;
        let (px_w, px_h) = native_pixels(id);

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        // Live swap — no stream recreation (screencapturekit 7.0.1).
        self.stream
            .update_content_filter(&filter)
            .map_err(|e| anyhow!("update_content_filter: {e}"))?;
        self.stream
            .update_configuration(&capture_config(px_w, px_h, self.fps))
            .map_err(|e| anyhow!("update_configuration: {e}"))?;
        self.current_id = id;
        self.resolution = Resolution {
            width: px_w,
            height: px_h,
        };
        Ok(())
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
