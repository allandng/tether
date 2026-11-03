use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use tether_protocol::{Codec, DisplayInfo, Resolution};

/// One captured frame: `width`×`height` pixels in one of the [`Pixels`]
/// representations.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    pub timestamp_micros: u64,
}

/// A frame's image.
///
/// Shared (`Arc`) so a capturer can re-emit its last image while the screen
/// is static without copying it — a native-resolution frame is 10–30 MB. A
/// repeat therefore shares its image with the frame it repeats
/// ([`RawFrame::same_image`]).
#[derive(Debug, Clone)]
pub enum Pixels {
    /// 32-bit BGRA in memory. Rows may be padded: address them with
    /// `bytes_per_row` (not `width * 4`); encoders receive it as the pitch.
    Bgra {
        data: Arc<Vec<u8>>,
        bytes_per_row: usize,
    },
    /// macOS: the IOSurface-backed `CVPixelBuffer` ScreenCaptureKit
    /// delivered (BGRA or NV12), retained as-is. VideoToolbox encodes it
    /// without a CPU copy; byte consumers read it through
    /// [`RawFrame::with_bgra`].
    #[cfg(target_os = "macos")]
    PixelBuffer(Arc<pixel_buffer::PixelBuffer>),
}

impl RawFrame {
    /// A frame of in-memory BGRA pixels, rows `bytes_per_row` apart.
    pub fn from_bgra(
        width: u32,
        height: u32,
        bytes_per_row: usize,
        data: impl Into<Arc<Vec<u8>>>,
        timestamp_micros: u64,
    ) -> Self {
        RawFrame {
            width,
            height,
            pixels: Pixels::Bgra {
                data: data.into(),
                bytes_per_row,
            },
            timestamp_micros,
        }
    }

    /// A frame carrying a retained CoreVideo pixel buffer (its own size).
    #[cfg(target_os = "macos")]
    pub fn from_pixel_buffer(buffer: pixel_buffer::PixelBuffer, timestamp_micros: u64) -> Self {
        RawFrame {
            width: buffer.width(),
            height: buffer.height(),
            pixels: Pixels::PixelBuffer(Arc::new(buffer)),
            timestamp_micros,
        }
    }

    /// True when both frames carry the very same image — i.e. one is an
    /// idle repeat of the other.
    pub fn same_image(&self, other: &RawFrame) -> bool {
        self.image_id().same(&other.image_id())
    }

    /// This frame's image identity, without keeping the image alive.
    pub fn image_id(&self) -> ImageId {
        ImageId(match &self.pixels {
            Pixels::Bgra { data, .. } => ImageRef::Bgra(Arc::downgrade(data)),
            #[cfg(target_os = "macos")]
            Pixels::PixelBuffer(buffer) => ImageRef::PixelBuffer(Arc::downgrade(buffer)),
        })
    }

    /// Run `f` over the image as packed BGRA (`pixels`, `bytes_per_row`).
    /// In-memory BGRA and BGRA pixel buffers are read in place (the latter
    /// locked for reading); an NV12 pixel buffer is converted on the CPU
    /// first — a fallback for byte consumers, not the H.264 path.
    pub fn with_bgra<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> anyhow::Result<R> {
        match &self.pixels {
            Pixels::Bgra {
                data,
                bytes_per_row,
            } => Ok(f(data, *bytes_per_row)),
            #[cfg(target_os = "macos")]
            Pixels::PixelBuffer(buffer) => buffer.with_bgra(f),
        }
    }
}

/// Identity of a frame's image that does not keep the image alive (a `Weak`:
/// a native-resolution image is tens of MB, and a capture pixel buffer
/// belongs to ScreenCaptureKit's small pool). Compare with
/// [`ImageId::same`]; the pipeline uses it to spot idle repeats.
#[derive(Debug, Clone)]
pub struct ImageId(ImageRef);

#[derive(Debug, Clone)]
enum ImageRef {
    Bgra(Weak<Vec<u8>>),
    #[cfg(target_os = "macos")]
    PixelBuffer(Weak<pixel_buffer::PixelBuffer>),
}

impl ImageId {
    pub fn same(&self, other: &ImageId) -> bool {
        match (&self.0, &other.0) {
            (ImageRef::Bgra(a), ImageRef::Bgra(b)) => a.ptr_eq(b),
            #[cfg(target_os = "macos")]
            (ImageRef::PixelBuffer(a), ImageRef::PixelBuffer(b)) => a.ptr_eq(b),
            #[cfg(target_os = "macos")]
            _ => false,
        }
    }
}

/// Exponential retry schedule: `min`, `2·min`, `4·min`, … capped at `max`.
/// Shared by the pipeline (capturer construction) and capturers that rebuild
/// a lost stream in place.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    failures: u32,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        Backoff {
            min,
            max: max.max(min),
            failures: 0,
        }
    }

    /// Record a failure; returns how long to wait before the next attempt.
    pub(crate) fn fail(&mut self) -> Duration {
        let shift = self.failures.min(16);
        self.failures = self.failures.saturating_add(1);
        self.min.saturating_mul(1 << shift).min(self.max)
    }

    /// Consecutive failures since the last [`reset`](Self::reset).
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    pub(crate) fn reset(&mut self) {
        self.failures = 0;
    }

    /// Log throttle for a retry loop: true on the 1st, 2nd, 4th, 8th, …
    /// consecutive failure, so a condition that persists for hours (no
    /// permission, display asleep) logs a handful of lines, not thousands.
    pub(crate) fn should_log(&self) -> bool {
        self.failures.is_power_of_two()
    }
}

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod pixel_buffer;
#[cfg(windows)]
pub mod windows;

/// One encoded frame ready for the wire. Cheap to clone (payload is refcounted).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub codec: Codec,
    pub seq: u32,
    pub timestamp_micros: u64,
    pub payload: Bytes,
}

/// Platform screen capture. Implementations are pull-based: `next_frame`
/// blocks until the next frame is available.
///
/// Contract the pipeline relies on: on a static screen `next_frame` still
/// returns periodically (both platform capturers re-emit the last image every
/// 250 ms), so a joining controller gets a frame, H.264 keyframes keep
/// coming, and display-switch requests are applied promptly. Platform
/// capturers recover from lost streams internally; an `Err` from
/// `next_frame` makes the pipeline drop the capturer and build a new one.
pub trait ScreenCapturer: Send {
    fn resolution(&self) -> Resolution;
    fn next_frame(&mut self) -> anyhow::Result<RawFrame>;

    /// The displays this capturer can target, with `active` marking the one it
    /// is currently capturing. Default: a single unnamed display from the
    /// current resolution (fine for fakes and single-display platforms).
    fn displays(&self) -> Vec<DisplayInfo> {
        let r = self.resolution();
        vec![DisplayInfo {
            id: 0,
            width: r.width,
            height: r.height,
            active: true,
            name: "Display".into(),
        }]
    }

    /// Switch the active capture to `id`. Default: only the implicit id 0 is
    /// valid (single-display platforms).
    fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
        if id == 0 {
            Ok(())
        } else {
            anyhow::bail!("display {id} not available")
        }
    }

    /// The id of the display being captured right now — the one
    /// [`displays`](Self::displays) marks `active`. Cheap: the pipeline
    /// polls it every frame to notice a capturer that silently fell back to
    /// another display (the captured one was unplugged), so the display list
    /// — and with it the input injector's target — follows the video.
    /// Default: the implicit single display 0.
    fn active_display(&self) -> u32 {
        0
    }

    /// An authenticated viewer just connected. A capturer that paused at
    /// the local user's request (macOS: "Stop Sharing" in the menu bar)
    /// resumes here. Default: nothing to do.
    fn viewer_joined(&mut self) {}
}

/// Frame encoder. JPEG (turbojpeg) or H.264 (VideoToolbox), selected at
/// startup via --codec.
pub trait FrameEncoder: Send {
    fn codec(&self) -> Codec;
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes>;
    /// Update the target bitrate (kbps) at runtime. No-op for codecs without a
    /// rate control knob (JPEG); overridden by H.264.
    fn set_bitrate(&mut self, _kbps: u32) {}
}

impl FrameEncoder for Box<dyn FrameEncoder> {
    fn codec(&self) -> Codec {
        (**self).codec()
    }
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes> {
        (**self).encode(frame)
    }
    fn set_bitrate(&mut self, kbps: u32) {
        (**self).set_bitrate(kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets() {
        let ms = Duration::from_millis;
        let mut b = Backoff::new(ms(250), ms(2000));
        let delays: Vec<_> = (0..6).map(|_| b.fail()).collect();
        assert_eq!(
            delays,
            [ms(250), ms(500), ms(1000), ms(2000), ms(2000), ms(2000)]
        );
        assert_eq!(b.failures(), 6);
        b.reset();
        assert_eq!(b.failures(), 0);
        assert_eq!(b.fail(), ms(250));
    }

    #[test]
    fn backoff_never_overflows() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        for _ in 0..10_000 {
            assert!(b.fail() <= Duration::from_secs(30));
        }
    }

    #[test]
    fn backoff_log_throttle_is_logarithmic() {
        let mut b = Backoff::new(Duration::from_millis(1), Duration::from_millis(1));
        let mut logged = Vec::new();
        for _ in 0..40 {
            b.fail();
            if b.should_log() {
                logged.push(b.failures());
            }
        }
        assert_eq!(logged, [1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn same_image_tracks_the_shared_buffer() {
        let a = RawFrame::from_bgra(1, 1, 4, vec![0; 4], 1);
        let repeat = RawFrame {
            timestamp_micros: 2,
            ..a.clone()
        };
        let fresh = RawFrame::from_bgra(1, 1, 4, vec![0; 4], 1);
        assert!(a.same_image(&repeat));
        assert!(!a.same_image(&fresh));
    }

    #[test]
    // `Pixels` has a single variant on Windows, where the `let … else` below
    // is irrefutable (a `_ => unreachable!()` arm instead would trip
    // `unreachable_patterns` there).
    #[allow(irrefutable_let_patterns)]
    fn image_id_does_not_keep_the_image_alive() {
        let a = RawFrame::from_bgra(1, 1, 4, vec![7; 4], 1);
        let id = a.image_id();
        assert!(id.same(&a.image_id()));
        let Pixels::Bgra { data, .. } = &a.pixels else {
            unreachable!()
        };
        let weak = Arc::downgrade(data);
        drop(a);
        assert!(weak.upgrade().is_none(), "ImageId kept the pixels alive");
        // ...yet a new image is never mistaken for the old one.
        let b = RawFrame::from_bgra(1, 1, 4, vec![7; 4], 1);
        assert!(!id.same(&b.image_id()));
    }

    #[test]
    fn with_bgra_reads_in_memory_pixels_in_place() {
        let frame = RawFrame::from_bgra(1, 2, 8, vec![1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8], 0);
        let (first, pitch) = frame
            .with_bgra(|px, pitch| (px[pitch..pitch + 4].to_vec(), pitch))
            .unwrap();
        assert_eq!((first.as_slice(), pitch), (&[5, 6, 7, 8][..], 8));
    }
}
