use bytes::Bytes;
use tether_protocol::{Codec, Resolution};

/// One captured frame, tightly packed 32-bit BGRA.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub timestamp_micros: u64,
}

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
}

/// Frame encoder. JPEG in Phase 1; H.264/VideoToolbox is a drop-in later.
pub trait FrameEncoder: Send {
    fn codec(&self) -> Codec;
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes>;
}
