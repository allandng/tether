//! Hardware H.264 encoding via VideoToolbox (VTCompressionSession).
//!
//! Output is Annex B: every access unit uses 4-byte start codes, and
//! keyframes carry SPS/PPS in-band, so each `FrameData` payload is decodable
//! by a WebCodecs `VideoDecoder` configured without a description (Annex B
//! mode). Low-latency settings: real-time, no frame reordering (no
//! B-frames), keyframe every 2s so any dropped-frame corruption self-heals.
//!
//! Frame dropping discipline: raw frames may be dropped *before* the encoder
//! (the capture channel does), never after — every encoded P-frame references
//! the previously *encoded* frame.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use objc2_core_foundation::{CFBoolean, CFNumber, CFRetained, CFString};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreateWithBytes, kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTProfileLevel_H264_Main_AutoLevel,
};
use tether_protocol::Codec;
use tracing::{info, warn};

use crate::capture::{FrameEncoder, RawFrame};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const KEYFRAME_INTERVAL_FRAMES: i32 = 60; // 2s at 30fps
const FPS: i32 = 30;

/// One encoded access unit handed back from the VT output callback.
struct EncodedAu {
    annex_b: Vec<u8>,
}

pub struct VtH264Encoder {
    session: Option<CFRetained<VTCompressionSession>>,
    width: u32,
    height: u32,
    bitrate_bps: i32,
    frame_index: i64,
    rx: Receiver<Result<EncodedAu, String>>,
    /// Leaked into the session as the output-callback refcon; reclaimed in Drop.
    tx_raw: *mut SyncSender<Result<EncodedAu, String>>,
}

// SAFETY: the encoder lives on the single capture/encode thread; the raw
// sender pointer is only dereferenced by the VT callback while the session
// is alive, and the session is torn down before the box is reclaimed.
unsafe impl Send for VtH264Encoder {}

impl VtH264Encoder {
    pub fn new(bitrate_kbps: u32) -> anyhow::Result<Self> {
        let (tx, rx) = sync_channel(4);
        Ok(VtH264Encoder {
            session: None,
            width: 0,
            height: 0,
            bitrate_bps: (bitrate_kbps as i32).saturating_mul(1000),
            frame_index: 0,
            rx,
            tx_raw: Box::into_raw(Box::new(tx)),
        })
    }

    fn ensure_session(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        if self.session.is_some() && self.width == width && self.height == height {
            return Ok(());
        }
        self.session = None; // drop (and invalidate) any previous session first
        info!(width, height, bitrate_bps = self.bitrate_bps, "creating VT compression session");

        let mut session_out: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: all pointer args are valid for the duration of the call;
        // the callback refcon outlives the session (reclaimed in Drop after
        // the session is released).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(output_callback),
                self.tx_raw as *mut c_void,
                NonNull::new(&mut session_out).expect("stack ptr"),
            )
        };
        if status != 0 || session_out.is_null() {
            bail!("VTCompressionSessionCreate failed: {status}");
        }
        // SAFETY: create-rule — we own one reference to the returned session.
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(session_out)) };

        set_property(&session, unsafe { kVTCompressionPropertyKey_RealTime }, CFBoolean::new(true).as_ref())?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
            CFBoolean::new(false).as_ref(),
        )?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_ProfileLevel },
            unsafe { kVTProfileLevel_H264_Main_AutoLevel },
        )?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            CFNumber::new_i32(self.bitrate_bps).as_ref(),
        )?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
            CFNumber::new_i32(KEYFRAME_INTERVAL_FRAMES).as_ref(),
        )?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            CFNumber::new_i32(FPS).as_ref(),
        )?;

        self.session = Some(session);
        self.width = width;
        self.height = height;
        Ok(())
    }
}

/// kCMTimeInvalid: CMTime is plain old data and the all-zero value (flags=0,
/// i.e. `kCMTimeFlags_Valid` unset) is exactly the invalid time constant.
fn cm_time_invalid() -> CMTime {
    // SAFETY: CMTime is a repr(C) POD struct; all-zero is a valid (and
    // meaningful: "invalid time") bit pattern.
    unsafe { std::mem::zeroed() }
}

fn set_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: &objc2_core_foundation::CFType,
) -> anyhow::Result<()> {
    // SAFETY: key/value are valid CF objects; session is a live VTSession.
    let status = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value)) };
    if status != 0 {
        // Non-fatal: some properties are encoder-dependent hints.
        warn!(key = %key, status, "VTSessionSetProperty failed");
    }
    Ok(())
}

impl FrameEncoder for VtH264Encoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    /// Retune the live session's average bitrate (adaptive bitrate). Applies on
    /// subsequent frames; safe between `encode` calls on the capture thread.
    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == 0 {
            return; // never clamp the live session to 0 bps
        }
        let bps = (kbps as i32).saturating_mul(1000);
        if bps == self.bitrate_bps {
            return;
        }
        self.bitrate_bps = bps;
        if let Some(session) = &self.session {
            let _ = set_property(
                session,
                unsafe { kVTCompressionPropertyKey_AverageBitRate },
                CFNumber::new_i32(bps).as_ref(),
            );
        }
    }

    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Bytes> {
        self.ensure_session(frame.width, frame.height)?;
        let session = self.session.as_ref().expect("ensured");

        // Wrap the BGRA bytes without copying. No release callback: the
        // bytes are only borrowed, which is sound because complete_frames()
        // below forces the encoder to finish with this buffer before we
        // return (and `frame` outlives this call).
        let mut pixel_buffer: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: base address points at frame.bgra which is valid and large
        // enough (bytes_per_row * height); out-pointer is a live stack slot.
        let cv = unsafe {
            CVPixelBufferCreateWithBytes(
                None,
                frame.width as usize,
                frame.height as usize,
                kCVPixelFormatType_32BGRA,
                NonNull::new(frame.bgra.as_ptr() as *mut c_void).context("null frame data")?,
                frame.bytes_per_row,
                None,
                ptr::null_mut(),
                None,
                NonNull::new(&mut pixel_buffer).expect("stack ptr"),
            )
        };
        if cv != 0 || pixel_buffer.is_null() {
            bail!("CVPixelBufferCreateWithBytes failed: {cv}");
        }
        // SAFETY: create-rule ownership of the new pixel buffer.
        let pixel_buffer = unsafe { CFRetained::from_raw(NonNull::new_unchecked(pixel_buffer)) };

        // SAFETY: 90kHz timescale pts; the pixel buffer and session are live.
        let pts = unsafe { CMTime::new(self.frame_index * (90_000 / FPS as i64), 90_000) };
        self.frame_index += 1;
        let status = unsafe {
            session.encode_frame(
                &pixel_buffer,
                pts,
                cm_time_invalid(), // duration unknown
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed: {status}");
        }
        // Force the callback before returning — both for latency and for the
        // soundness of the borrowed pixel bytes above.
        // kCMTimeInvalid means "complete everything pending".
        let status = unsafe { session.complete_frames(cm_time_invalid()) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: {status}");
        }

        let au = self
            .rx
            .recv_timeout(Duration::from_secs(2))
            .context("VT output callback never fired")?
            .map_err(|e| anyhow!("VT encode failed: {e}"))?;
        Ok(Bytes::from(au.annex_b))
    }
}

impl Drop for VtH264Encoder {
    fn drop(&mut self) {
        self.session = None; // release the session before freeing its refcon
        // SAFETY: tx_raw came from Box::into_raw in new(); the session that
        // could call back into it is gone.
        unsafe { drop(Box::from_raw(self.tx_raw)) };
    }
}

/// VT output callback: convert the sample buffer to one Annex B access unit
/// (SPS/PPS prepended on keyframes, AVCC length prefixes rewritten to start
/// codes) and hand it back to `encode()`.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_refcon: *mut c_void,
    status: i32,
    _flags: objc2_video_toolbox::VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the leaked SyncSender owned by VtH264Encoder, alive
    // for as long as any session that knows this pointer.
    let tx = unsafe { &*(refcon as *const SyncSender<Result<EncodedAu, String>>) };
    if status != 0 {
        let _ = tx.try_send(Err(format!("callback status {status}")));
        return;
    }
    let Some(sample) = (unsafe { sample_buffer.as_ref() }) else {
        let _ = tx.try_send(Err("null sample buffer".into()));
        return;
    };
    match unsafe { annex_b_from_sample(sample) } {
        Ok(annex_b) => {
            let _ = tx.try_send(Ok(EncodedAu { annex_b }));
        }
        Err(e) => {
            let _ = tx.try_send(Err(e));
        }
    }
}

/// SAFETY: `sample` must be a valid, complete CMSampleBuffer from the VT
/// encoder (H.264, AVCC framing).
unsafe fn annex_b_from_sample(sample: &CMSampleBuffer) -> Result<Vec<u8>, String> {
    // Keyframe: NotSync attachment absent (or false) means sync frame.
    let keyframe = unsafe {
        sample
            .sample_attachments_array(false)
            .and_then(|arr| {
                if arr.count() == 0 {
                    return None;
                }
                let dict = arr.value_at_index(0) as *const objc2_core_foundation::CFDictionary;
                let dict = dict.as_ref()?;
                let not_sync = dict.value(
                    kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void,
                );
                Some(!not_sync.is_null())
            })
            .map(|not_sync| !not_sync)
            .unwrap_or(true) // no attachments at all = sync frame
    };

    let mut out = Vec::with_capacity(64 * 1024);
    let mut nal_len_size: i32 = 4;

    if keyframe {
        let format = unsafe { sample.format_description() }
            .ok_or_else(|| "no format description".to_string())?;
        // SPS (index 0), PPS (index 1), and however many more there are.
        let mut index = 0usize;
        loop {
            let mut ptr_out: *const u8 = ptr::null();
            let mut size_out: usize = 0;
            let mut count_out: usize = 0;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &format,
                    index,
                    &mut ptr_out,
                    &mut size_out,
                    &mut count_out,
                    &mut nal_len_size,
                )
            };
            if status != 0 {
                return Err(format!("GetH264ParameterSetAtIndex({index}) = {status}"));
            }
            out.extend_from_slice(&START_CODE);
            // SAFETY: VT guarantees ptr_out/size_out describe a valid buffer
            // owned by the format description, which outlives this scope.
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr_out, size_out) });
            index += 1;
            if index >= count_out {
                break;
            }
        }
    }

    let block = unsafe { sample.data_buffer() }.ok_or_else(|| "no data buffer".to_string())?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    let status = unsafe {
        block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr() as *mut c_void).unwrap())
    };
    if status != 0 {
        return Err(format!("CMBlockBufferCopyDataBytes = {status}"));
    }

    // AVCC -> Annex B: rewrite [len][nal] records to [start code][nal].
    let nal_len_size = nal_len_size as usize;
    let mut offset = 0usize;
    while offset + nal_len_size <= avcc.len() {
        let mut nal_len = 0usize;
        for &b in &avcc[offset..offset + nal_len_size] {
            nal_len = (nal_len << 8) | b as usize;
        }
        offset += nal_len_size;
        if nal_len == 0 || offset + nal_len > avcc.len() {
            return Err("corrupt AVCC framing".into());
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[offset..offset + nal_len]);
        offset += nal_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_frame(width: u32, height: u32, shift: usize) -> RawFrame {
        let bytes_per_row = width as usize * 4;
        let mut bgra = vec![255u8; bytes_per_row * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let o = y * bytes_per_row + x * 4;
                bgra[o] = ((x + shift) % 256) as u8;
                bgra[o + 1] = (y % 256) as u8;
                bgra[o + 2] = 100;
            }
        }
        RawFrame { width, height, bytes_per_row, bgra, timestamp_micros: 0 }
    }

    fn nal_types(annex_b: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= annex_b.len() {
            if annex_b[i..i + 4] == START_CODE {
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

    #[test]
    fn first_frame_is_annex_b_idr_with_parameter_sets() {
        let mut enc = VtH264Encoder::new(4000).unwrap();
        assert_eq!(enc.codec(), Codec::H264);
        let au = enc.encode(&gradient_frame(640, 400, 0)).unwrap();
        assert_eq!(&au[..4], &START_CODE);
        let types = nal_types(&au);
        assert!(types.contains(&7), "SPS missing: {types:?}");
        assert!(types.contains(&8), "PPS missing: {types:?}");
        assert!(types.contains(&5), "IDR missing: {types:?}");
    }

    #[test]
    fn set_bitrate_on_a_live_session_keeps_producing_valid_access_units() {
        use crate::capture::FrameEncoder;
        let mut enc = VtH264Encoder::new(4000).unwrap();
        enc.encode(&gradient_frame(640, 400, 0)).unwrap(); // create the session
        // retune the live session down, then up — must not error or corrupt output
        enc.set_bitrate(800);
        let a = enc.encode(&gradient_frame(640, 400, 1)).unwrap();
        enc.set_bitrate(3000);
        let b = enc.encode(&gradient_frame(640, 400, 2)).unwrap();
        for au in [&a, &b] {
            assert_eq!(&au[..4], &START_CODE);
            assert!(!nal_types(au).is_empty(), "no NAL units after set_bitrate");
        }
    }

    #[test]
    fn subsequent_frames_are_deltas_and_bitrate_is_sane() {
        let mut enc = VtH264Encoder::new(4000).unwrap();
        let key = enc.encode(&gradient_frame(640, 400, 0)).unwrap();
        let mut delta_total = 0usize;
        for i in 1..10 {
            let au = enc.encode(&gradient_frame(640, 400, i)).unwrap();
            let types = nal_types(&au);
            assert!(!types.contains(&5), "frame {i} unexpectedly IDR");
            assert!(!types.contains(&7), "delta frame {i} carries SPS");
            delta_total += au.len();
        }
        // 9 delta frames of a slowly-moving gradient at 4 Mbps must come in
        // well under the keyframe-per-frame regime JPEG would produce.
        assert!(delta_total / 9 < key.len(), "deltas not smaller than keyframe");
    }

    #[test]
    fn resolution_change_recreates_the_session() {
        let mut enc = VtH264Encoder::new(2000).unwrap();
        let a = enc.encode(&gradient_frame(640, 400, 0)).unwrap();
        let b = enc.encode(&gradient_frame(320, 200, 0)).unwrap();
        // both must be keyframes with parameter sets (new session each)
        for (label, au) in [("first", &a), ("second", &b)] {
            let types = nal_types(au);
            assert!(types.contains(&5), "{label} AU lacks IDR after session (re)create");
        }
    }
}
