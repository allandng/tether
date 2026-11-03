//! Software H.264 via Cisco OpenH264 — the Windows `--codec h264` path.
//!
//! The Phase 7 plan proposed the Media Foundation H.264 MFT; this uses
//! OpenH264 instead for v1. Both are vendor-neutral, but the MF hardware
//! encoders are asynchronous MFTs whose event model differs per GPU vendor,
//! while OpenH264 is one synchronous call that behaves identically on every
//! machine (including VMs with no GPU encoder at all). At 1080p30 it costs a
//! few milliseconds of one core with the screen-content preset; the gate doc
//! records the measured number.
//!
//! Output matches the VideoToolbox path exactly: Annex B with 4-byte start
//! codes, SPS/PPS in-band on every IDR, no B-frames, an IDR at least every
//! [`KEYFRAME_INTERVAL`]. The interval is wall-clock rather than a frame
//! count because the capturer slows to a trickle while the screen is static,
//! and a controller joining mid-stream must not wait long for a keyframe.

use std::time::{Duration, Instant};

use anyhow::anyhow;
use bytes::Bytes;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSlices;
use tether_protocol::Codec;
use tracing::info;

use super::yuv::I420;
use crate::capture::{FrameEncoder, RawFrame};

const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
const FPS: f32 = 30.0;
/// OpenH264 refuses anything above level 5.2 (3840x2160).
const MAX_W: usize = 3840;
const MAX_H: usize = 2160;

/// Encoder worker threads: OpenH264 slices the picture across them. Capped
/// so a many-core host doesn't spend its whole budget on one stream.
fn threads() -> u16 {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 4) as u16)
        .unwrap_or(1)
}

pub struct OpenH264Encoder {
    encoder: Encoder,
    /// The rate the live encoder is actually running at (0 = unknown, e.g.
    /// right after a (re)initialisation reset it to the configured value).
    bitrate_bps: u32,
    yuv: I420,
    last_keyframe: Option<Instant>,
    dims: (usize, usize),
}

impl OpenH264Encoder {
    pub fn new(bitrate_kbps: u32) -> anyhow::Result<Self> {
        let bitrate_bps = bitrate_kbps.max(100).saturating_mul(1000);
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(FPS))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(Complexity::Low)
            // neither is supported for screen content; say so up front
            // instead of having OpenH264 warn and turn them off itself
            .adaptive_quantization(false)
            .background_detection(false)
            // Rate control only holds the target with frame skipping on; a
            // skipped frame comes back empty and the pipeline drops it.
            .skip_frames(true)
            .num_threads(threads())
            .intra_frame_period(IntraFramePeriod::auto());
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| anyhow!("init OpenH264: {e}"))?;
        info!(bitrate_kbps, "OpenH264 encoder ready");
        Ok(OpenH264Encoder {
            encoder,
            bitrate_bps,
            yuv: I420::default(),
            last_keyframe: None,
            dims: (0, 0),
        })
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == 0 {
            return;
        }
        let bps = kbps.saturating_mul(1000);
        if bps == self.bitrate_bps {
            return;
        }
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: bps as i32,
        };
        // SAFETY: ENCODER_OPTION_BITRATE takes a pointer to SBitrateInfo.
        // Before the first frame the encoder isn't initialised and this fails
        // harmlessly; the rate is only recorded once it took, so the
        // pipeline's next per-frame request applies it.
        let rc = unsafe {
            self.encoder.raw_api().set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&raw mut info).cast(),
            )
        };
        if rc == 0 {
            self.bitrate_bps = bps;
        }
    }

    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let scale = if w > MAX_W || h > MAX_H { 2 } else { 1 };
        let yuv = &mut self.yuv;
        frame.with_bgra(|bgra, pitch| yuv.convert_from(bgra, w, h, pitch, scale))?;
        let dims = (self.yuv.width, self.yuv.height);
        if dims != self.dims {
            // OpenH264 reinitialises (and emits an IDR) on a size change —
            // back at the configured bitrate, so forget the adapted one and
            // let the pipeline's next request re-apply it.
            self.dims = dims;
            self.bitrate_bps = 0;
            self.last_keyframe = Some(Instant::now());
        }
        if self
            .last_keyframe
            .is_none_or(|at| at.elapsed() >= KEYFRAME_INTERVAL)
        {
            if self.last_keyframe.is_some() {
                self.encoder.force_intra_frame();
            }
            self.last_keyframe = Some(Instant::now());
        }
        let source = YUVSlices::new(
            (&self.yuv.y, &self.yuv.u, &self.yuv.v),
            dims,
            (dims.0, dims.0 / 2, dims.0 / 2),
        );
        let bitstream = self
            .encoder
            .encode(&source)
            .map_err(|e| anyhow!("OpenH264 encode: {e}"))?;
        // Empty = rate control skipped this frame; the pipeline drops it and
        // the next frame references the last one actually encoded.
        Ok(Bytes::from(bitstream.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal_types(annex_b: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= annex_b.len() {
            if annex_b[i..i + 4] == [0, 0, 0, 1] {
                if let Some(&b) = annex_b.get(i + 4) {
                    types.push(b & 0x1F);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }

    /// A static textured screen with a small box that moves with `step` —
    /// screen-content-like: most of the picture is unchanged between frames.
    fn frame(w: u32, h: u32, step: u8) -> RawFrame {
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in bgra.chunks_mut(4).enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let texture = ((x * 7 + y * 13) ^ (x * y)) as u8;
            let in_box = (x / 16) == u32::from(step) && (y / 16) == 1;
            let v = if in_box { 255 } else { texture };
            px.copy_from_slice(&[v, v / 2, 128, 255]);
        }
        RawFrame::from_bgra(w, h, (w * 4) as usize, bgra, 0)
    }

    #[test]
    fn first_frame_is_annex_b_idr_with_parameter_sets() {
        let mut enc = OpenH264Encoder::new(2000).unwrap();
        assert_eq!(enc.codec(), Codec::H264);
        let au = enc.encode(&frame(320, 240, 0)).unwrap();
        assert_eq!(&au[..4], &[0, 0, 0, 1], "4-byte start code");
        let types = nal_types(&au);
        assert!(types.contains(&7), "SPS missing: {types:?}");
        assert!(types.contains(&8), "PPS missing: {types:?}");
        assert!(types.contains(&5), "IDR missing: {types:?}");
    }

    #[test]
    fn subsequent_frames_are_deltas_until_resize() {
        let mut enc = OpenH264Encoder::new(2000).unwrap();
        let key = enc.encode(&frame(320, 240, 0)).unwrap();
        for i in 1..5 {
            let au = enc.encode(&frame(320, 240, i)).unwrap();
            assert!(!nal_types(&au).contains(&5), "frame {i} unexpectedly IDR");
            assert!(au.len() < key.len(), "delta not smaller than keyframe");
        }
        enc.set_bitrate(800); // live rate change must not disturb the stream
        let au = enc.encode(&frame(320, 240, 9)).unwrap();
        assert!(!nal_types(&au).contains(&5));
        // a resolution change restarts with a decodable IDR
        let au = enc.encode(&frame(640, 360, 0)).unwrap();
        let types = nal_types(&au);
        assert!(types.contains(&7) && types.contains(&5), "{types:?}");
    }

    #[test]
    fn oversized_sources_are_downscaled_not_refused() {
        let mut enc = OpenH264Encoder::new(4000).unwrap();
        let au = enc.encode(&frame(5120, 2880, 0)).unwrap();
        assert!(nal_types(&au).contains(&5));
        assert_eq!(enc.dims, (2560, 1440));
    }
}
