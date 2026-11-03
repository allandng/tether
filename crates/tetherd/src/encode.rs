use anyhow::Context;
use bytes::Bytes;
use tether_protocol::Codec;

use crate::capture::{FrameEncoder, RawFrame};

#[cfg(target_os = "macos")]
pub mod h264;
#[cfg(windows)]
pub mod h264_soft;
#[cfg(any(target_os = "macos", test))]
mod sps;
#[cfg(any(windows, test))]
pub mod yuv;

/// JPEG via libjpeg-turbo (SIMD). ~15–25 ms per Retina-sized frame, which
/// holds the ≥15 fps gate; H.264/VideoToolbox is a Module 6 drop-in here.
pub struct JpegEncoder {
    compressor: turbojpeg::Compressor,
}

impl JpegEncoder {
    pub fn new(quality: i32) -> anyhow::Result<Self> {
        let mut compressor = turbojpeg::Compressor::new().context("init turbojpeg")?;
        compressor.set_quality(quality)?;
        // 4:2:0 chroma subsampling: halves the bytes, invisible at screen-share quality.
        compressor.set_subsamp(turbojpeg::Subsamp::Sub2x2)?;
        Ok(JpegEncoder { compressor })
    }
}

impl FrameEncoder for JpegEncoder {
    fn codec(&self) -> Codec {
        Codec::Jpeg
    }

    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        let compressor = &mut self.compressor;
        // BGRA read in place (a capture pixel buffer is locked meanwhile).
        let buf = frame
            .with_bgra(|pixels, pitch| {
                compressor.compress_to_owned(turbojpeg::Image {
                    pixels,
                    width,
                    pitch,
                    height,
                    format: turbojpeg::PixelFormat::BGRA,
                })
            })?
            .context("jpeg encode")?;
        Ok(Bytes::copy_from_slice(&buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic BGRA gradient with deliberately padded rows.
    fn test_frame(width: u32, height: u32) -> RawFrame {
        let bytes_per_row = (width as usize * 4) + 16; // padding past the pixels
        let mut bgra = vec![0u8; bytes_per_row * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let o = y * bytes_per_row + x * 4;
                bgra[o] = (x * 255 / width as usize) as u8; // B ramps left→right
                bgra[o + 1] = (y * 255 / height as usize) as u8; // G ramps top→bottom
                bgra[o + 2] = 128; // R constant
                bgra[o + 3] = 255;
            }
        }
        RawFrame::from_bgra(width, height, bytes_per_row, bgra, 0)
    }

    #[test]
    fn encodes_padded_bgra_to_decodable_jpeg() {
        let frame = test_frame(64, 48);
        let mut enc = JpegEncoder::new(75).unwrap();
        assert_eq!(enc.codec(), Codec::Jpeg);
        let jpeg = enc.encode(&frame).unwrap();

        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let header = turbojpeg::read_header(&jpeg).unwrap();
        assert_eq!((header.width, header.height), (64, 48));

        // Decode and spot-check the gradient survived (lossy, so wide margins).
        let decoded = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::BGRA).unwrap();
        let right = decoded.pixels[(decoded.pitch * 24) + 60 * 4] as i32; // B near right edge
        let left = decoded.pixels[(decoded.pitch * 24) + 2 * 4] as i32; // B near left edge
        assert!(
            right - left > 100,
            "blue gradient lost: left={left} right={right}"
        );
    }

    /// JPEG mode on macOS: the capturer hands over BGRA pixel buffers,
    /// read in place with their own (padded) pitch.
    #[cfg(target_os = "macos")]
    #[test]
    fn encodes_a_bgra_pixel_buffer_like_the_same_bytes() {
        use crate::capture::pixel_buffer::PixelBuffer;
        let bytes = test_frame(61, 48); // odd width: the IOSurface pads its rows
        let crate::capture::Pixels::Bgra {
            data,
            bytes_per_row,
        } = &bytes.pixels
        else {
            unreachable!()
        };
        let surface = RawFrame::from_pixel_buffer(
            PixelBuffer::from_bgra(61, 48, data, *bytes_per_row).unwrap(),
            0,
        );
        let mut enc = JpegEncoder::new(75).unwrap();
        assert_eq!(enc.encode(&surface).unwrap(), enc.encode(&bytes).unwrap());
    }

    #[test]
    fn second_encode_reuses_compressor() {
        let mut enc = JpegEncoder::new(75).unwrap();
        let a = enc.encode(&test_frame(32, 32)).unwrap();
        let b = enc.encode(&test_frame(32, 32)).unwrap();
        assert_eq!(a, b, "same input must give same output across calls");
    }
}
