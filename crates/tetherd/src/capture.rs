use bytes::Bytes;
use tether_protocol::{Codec, DisplayInfo, Resolution};

/// One captured frame, 32-bit BGRA. Rows may be padded: use `bytes_per_row`
/// (not `width * 4`) to address rows; encoders receive it as the pitch.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: usize,
    pub bgra: Vec<u8>,
    pub timestamp_micros: u64,
}

#[cfg(target_os = "macos")]
pub mod macos;

/// One encoded frame ready for the wire. Cheap to clone (payload is refcounted).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub codec: Codec,
    pub seq: u32,
    pub timestamp_micros: u64,
    pub payload: Bytes,
}

/// Platform screen capture. Implementations are pull-based: `next_frame`
/// blocks until the next frame is available. macOS implementation arrives in
/// Module 3; a second OS later is an additive impl, not surgery.
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
