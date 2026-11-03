//! Hardware H.264 encoding via VideoToolbox (VTCompressionSession).
//!
//! Output is Annex B: every access unit uses 4-byte start codes, and
//! keyframes carry SPS/PPS in-band, so each `FrameData` payload is decodable
//! by a WebCodecs `VideoDecoder` configured without a description (Annex B
//! mode). Low-latency settings: real-time, no frame reordering (no
//! B-frames), and an IDR at least every [`KEYFRAME_INTERVAL`] of wall-clock
//! time so any dropped-frame corruption self-heals and a controller joining
//! mid-stream decodes within ~2 s. The interval is wall-clock, not a frame
//! count: on a static screen the capturer only repeats frames every 250 ms,
//! and a 60-frame interval would then mean a keyframe every 15 s. Timestamps
//! are therefore real (elapsed time), VT's `MaxKeyFrameIntervalDuration` is
//! set, and an IDR is forced if VT hasn't produced one in time.
//!
//! Frame dropping discipline: raw frames may be dropped *before* the encoder
//! (the capture channel does), never after — every encoded P-frame references
//! the previously *encoded* frame.
//!
//! Input: a capture frame's `CVPixelBuffer` is handed to VT as-is — on macOS
//! the capturer delivers ScreenCaptureKit's IOSurface-backed NV12 buffers,
//! which the hardware encoder reads directly: no CPU copy, no colour
//! conversion. In-memory BGRA (tests, the capturer's black placeholder) is
//! wrapped without copying and converted by VT. The session is (re)created
//! when the source size or pixel format changes, with source attributes
//! matching it. Colour: the stream signals BT.709 (primaries, transfer,
//! matrix; video range) in the SPS — SCK's NV12 is 709 video range, and so
//! is VT's own conversion of BGRA — so decoders don't have to guess. The
//! SPS also gets a VUI bitstream restriction saying frames are never
//! reordered (`encode/sps.rs`); without it decoders buffer several frames
//! before output (~200 ms in Chrome at Retina sizes).
//!
//! Size: the hardware encoder takes frames up to [`MAX_SIZE`]; the macOS
//! capturer scales larger displays down to fit before frames get here.
//!
//! Teardown: a session being replaced (or dropped) is invalidated first —
//! VT's orderly shutdown — and every frame is tagged, so an output callback
//! that still lands late is recognised as stale instead of being taken for
//! the next frame's access unit (which would leave every later frame one
//! behind).

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail, ensure};
use bytes::Bytes;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreateWithBytes, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Main_AutoLevel,
};
use tether_protocol::{Codec, Resolution};
use tracing::{debug, info, warn};

use crate::capture::pixel_buffer::fourcc;
use crate::capture::{FrameEncoder, Pixels, RawFrame};

/// The largest frame to hand the encoder, as (long side, short side) — the
/// capturer applies it in either orientation
/// (`capture::macos::fit_within`). VideoToolbox's hardware H.264 encoder
/// takes frames up to 4096 px on a side (measured on Apple silicon: 4096×2560
/// and 2304×4096 encode in hardware; 4112 px or wider silently falls back to
/// a software encoder far too slow for 30 fps at 5K/6K), and 4096×2304 is
/// also the largest frame of H.264 level 5.1/5.2 (36 864 macroblocks) that
/// decoders commonly support.
pub const MAX_SIZE: Resolution = Resolution {
    width: 4096,
    height: 2304,
};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// Maximum wall-clock time between keyframes.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Frame-count cap as well (2 s at 30 fps); the duration cap is what matters
/// at low frame rates.
const KEYFRAME_INTERVAL_FRAMES: i32 = 60;
const FPS: i32 = 30;

/// One encoded access unit handed back from the VT output callback.
struct EncodedAu {
    annex_b: Vec<u8>,
    keyframe: bool,
}

/// What the output callback sends back: the tag of the frame it answers
/// (passed to VT as the frame's `sourceFrameRefcon`) and its result.
type Output = (u64, Result<EncodedAu, String>);

pub struct VtH264Encoder {
    session: Option<CFRetained<VTCompressionSession>>,
    width: u32,
    height: u32,
    /// Source pixel format the session was set up for.
    format: u32,
    bitrate_bps: i32,
    keyframe_interval: Duration,
    /// Time zero of the current session's presentation timestamps.
    epoch: Instant,
    /// Last presentation timestamp handed to VT (µs), to keep them strictly
    /// increasing.
    last_pts_us: i64,
    last_keyframe: Option<Instant>,
    /// Tag of the last frame submitted; outputs carrying another are stale.
    frame_tag: u64,
    rx: Receiver<Output>,
    /// Leaked into the session as the output-callback refcon; reclaimed in Drop.
    tx_raw: *mut SyncSender<Output>,
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
            format: 0,
            bitrate_bps: (bitrate_kbps as i32).saturating_mul(1000),
            keyframe_interval: KEYFRAME_INTERVAL,
            epoch: Instant::now(),
            last_pts_us: -1,
            last_keyframe: None,
            frame_tag: 0,
            rx,
            tx_raw: Box::into_raw(Box::new(tx)),
        })
    }

    /// Invalidate and release the session, if any. A session that is only
    /// released lives on until VT lets go of it and may still call back;
    /// invalidation tears it down now, in order.
    fn drop_session(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: a live session we own; it is not used again.
            unsafe { session.invalidate() };
        }
    }

    /// A session for `width`×`height` frames of pixel `format`, recreated
    /// when any of them changes (display switch, mode change, or the
    /// capturer's BGRA placeholder giving way to NV12 frames).
    fn ensure_session(&mut self, width: u32, height: u32, format: u32) -> anyhow::Result<()> {
        if self.session.is_some()
            && (self.width, self.height, self.format) == (width, height, format)
        {
            return Ok(());
        }
        self.drop_session(); // invalidate and drop any previous session first
        info!(
            width,
            height,
            format = %fourcc(format),
            bitrate_bps = self.bitrate_bps,
            "creating VT compression session"
        );

        let source_attributes = source_attributes(width, height, format);
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
                Some(source_attributes.as_ref()),
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

        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_RealTime },
            CFBoolean::new(true).as_ref(),
        )?;
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
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
            CFNumber::new_f64(self.keyframe_interval.as_secs_f64()).as_ref(),
        )?;
        set_property(
            &session,
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            CFNumber::new_i32(FPS).as_ref(),
        )?;
        // BT.709 colour description in the SPS (VUI). It matches the pixels:
        // ScreenCaptureKit's NV12 is tagged 709 (so VT converts nothing) and
        // VT's own BGRA conversion uses 709 too. Unsignalled, decoders guess
        // (often BT.601), shifting colours.
        // SAFETY: reading immutable VT/CoreVideo CFString constants.
        unsafe {
            for (key, value) in [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
            ] {
                set_property(&session, key, value)?;
            }
        }

        self.session = Some(session);
        self.width = width;
        self.height = height;
        self.format = format;
        self.epoch = Instant::now();
        self.last_pts_us = -1;
        self.last_keyframe = None; // a new session opens with an IDR anyway
        Ok(())
    }

    /// Real elapsed time since the session started (µs), strictly increasing
    /// — what VT's duration-based keyframe interval and rate control measure.
    fn next_pts_us(&mut self) -> i64 {
        let elapsed = i64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(i64::MAX);
        self.last_pts_us = elapsed.max(self.last_pts_us.saturating_add(1));
        self.last_pts_us
    }

    /// Whether this frame must be an IDR because the last keyframe is too old.
    fn keyframe_due(&self) -> bool {
        self.last_keyframe
            .is_some_and(|at| at.elapsed() >= self.keyframe_interval)
    }
}

/// kCMTimeInvalid: CMTime is plain old data and the all-zero value (flags=0,
/// i.e. `kCMTimeFlags_Valid` unset) is exactly the invalid time constant.
fn cm_time_invalid() -> CMTime {
    // SAFETY: CMTime is a repr(C) POD struct; all-zero is a valid (and
    // meaningful: "invalid time") bit pattern.
    unsafe { std::mem::zeroed() }
}

/// Source pixel-buffer attributes matching the frames the session will get:
/// IOSurface-backed `format` at `width`×`height`.
fn source_attributes(width: u32, height: u32, format: u32) -> CFRetained<CFDictionary> {
    let no_properties = CFDictionary::<CFString, CFType>::empty();
    let (format, width, height) = (
        CFNumber::new_i64(format.into()),
        CFNumber::new_i64(width.into()),
        CFNumber::new_i64(height.into()),
    );
    // SAFETY: reading immutable CoreVideo CFString constants.
    let keys: [&CFString; 4] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
        ]
    };
    let values: [&CFType; 4] = [
        format.as_ref(),
        width.as_ref(),
        height.as_ref(),
        no_properties.as_ref(),
    ];
    let dict = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // SAFETY: erasing the key/value types of a CF dictionary is a no-op.
    unsafe { CFRetained::cast_unchecked(dict) }
}

/// Wrap in-memory BGRA as a `CVPixelBuffer` without copying. No release
/// callback: the bytes are only borrowed, which is sound because `encode`
/// forces VT to finish with the buffer (`complete_frames`) before it returns
/// and the frame — owner of the bytes — outlives that call.
fn wrap_bgra(
    width: u32,
    height: u32,
    data: &[u8],
    bytes_per_row: usize,
) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
    ensure!(
        data.len() >= bytes_per_row * height as usize && bytes_per_row >= width as usize * 4,
        "BGRA frame smaller than {width}x{height} at pitch {bytes_per_row}"
    );
    let mut pixel_buffer: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: the base address points at `data`, which spans
    // bytes_per_row * height bytes (checked above); the out-pointer is a
    // live stack slot.
    let cv = unsafe {
        CVPixelBufferCreateWithBytes(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            NonNull::new(data.as_ptr() as *mut c_void).context("null frame data")?,
            bytes_per_row,
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
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(pixel_buffer)) })
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
        let result: anyhow::Result<Bytes> = (|| {
            // The pixels VT reads. A capture pixel buffer goes in as-is (an
            // idle repeat re-submits the same one with a fresh timestamp);
            // in-memory BGRA is wrapped, not copied.
            let wrapped;
            let pixel_buffer: &CVPixelBuffer = match &frame.pixels {
                Pixels::PixelBuffer(buffer) => buffer.as_cv(),
                Pixels::Bgra {
                    data,
                    bytes_per_row,
                } => {
                    wrapped = wrap_bgra(frame.width, frame.height, data, *bytes_per_row)?;
                    &wrapped
                }
            };
            self.ensure_session(
                CVPixelBufferGetWidth(pixel_buffer) as u32,
                CVPixelBufferGetHeight(pixel_buffer) as u32,
                CVPixelBufferGetPixelFormatType(pixel_buffer),
            )?;

            let force_keyframe = self.keyframe_due();
            let frame_options = force_keyframe.then(|| {
                // SAFETY: reading an immutable VT-provided CFString constant.
                let key: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
                CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)])
            });
            let frame_options: Option<&CFDictionary> = frame_options.as_deref().map(|d| d.as_ref());

            // SAFETY: microsecond-timescale pts; plain value construction.
            let pts = unsafe { CMTime::new(self.next_pts_us(), 1_000_000) };
            self.frame_tag = self.frame_tag.wrapping_add(1);
            let tag = self.frame_tag;
            let session = self.session.as_ref().expect("ensured");
            // SAFETY: the pixel buffer, options dictionary and session are
            // live; the source-frame refcon is an integer tag, never
            // dereferenced.
            let status = unsafe {
                session.encode_frame(
                    pixel_buffer,
                    pts,
                    cm_time_invalid(), // duration unknown
                    frame_options,
                    tag as usize as *mut c_void,
                    ptr::null_mut(),
                )
            };
            if status != 0 {
                bail!("VTCompressionSessionEncodeFrame failed: {status}");
            }
            // Force the callback before returning — both for latency and for the
            // soundness of wrapped (borrowed) BGRA bytes.
            // kCMTimeInvalid means "complete everything pending".
            let status = unsafe { session.complete_frames(cm_time_invalid()) };
            if status != 0 {
                bail!("VTCompressionSessionCompleteFrames failed: {status}");
            }

            let au = self
                .receive(tag, Duration::from_secs(2))?
                .map_err(|e| anyhow!("VT encode failed: {e}"))?;
            if au.keyframe {
                self.last_keyframe = Some(Instant::now());
            } else if force_keyframe {
                warn!("VT ignored a forced keyframe request");
            }
            Ok(Bytes::from(au.annex_b))
        })();
        if let Err(e) = &result {
            // A stalled/failed VT cycle can wedge the session: rebuild it on
            // the next frame. Output it still delivers late is tagged with an
            // old frame and skipped by `receive`; clear what is already here.
            warn!(error = %e, "h264 encode failed; rebuilding VT session next frame");
            self.drop_session();
            while self.rx.try_recv().is_ok() {}
        }
        result
    }
}

impl VtH264Encoder {
    /// Wait up to `timeout` for the output of the frame tagged `tag`,
    /// discarding any stale output (an earlier frame's, from a session that
    /// timed out or was torn down) that arrives first.
    fn receive(&self, tag: u64, timeout: Duration) -> anyhow::Result<Result<EncodedAu, String>> {
        let deadline = Instant::now() + timeout;
        loop {
            let (got, result) = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("VT output callback never fired")?;
            if got == tag {
                return Ok(result);
            }
            debug!(got, want = tag, "discarding stale VT output");
        }
    }
}

impl Drop for VtH264Encoder {
    fn drop(&mut self) {
        self.drop_session(); // tear the session down before freeing its refcon
        // SAFETY: tx_raw came from Box::into_raw in new(); the session that
        // could call back into it is gone.
        unsafe { drop(Box::from_raw(self.tx_raw)) };
    }
}

/// VT output callback: convert the sample buffer to one Annex B access unit
/// (SPS/PPS prepended on keyframes, AVCC length prefixes rewritten to start
/// codes) and hand it back to `encode()`, tagged with the frame it answers.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: i32,
    _flags: objc2_video_toolbox::VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the leaked SyncSender owned by VtH264Encoder, alive
    // for as long as any session that knows this pointer.
    let tx = unsafe { &*(refcon as *const SyncSender<Output>) };
    // The frame's tag, passed to VT as an integer (see `encode`).
    let tag = source_refcon as usize as u64;
    let result = if status != 0 {
        Err(format!("callback status {status}"))
    } else {
        match unsafe { sample_buffer.as_ref() } {
            // SAFETY: a complete H.264 sample buffer from this session.
            Some(sample) => unsafe { annex_b_from_sample(sample) },
            None => Err("null sample buffer".into()),
        }
    };
    let _ = tx.try_send((tag, result));
}

/// SAFETY: `sample` must be a valid, complete CMSampleBuffer from the VT
/// encoder (H.264, AVCC framing).
unsafe fn annex_b_from_sample(sample: &CMSampleBuffer) -> Result<EncodedAu, String> {
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
                let not_sync =
                    dict.value(kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void);
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
            let set = unsafe { std::slice::from_raw_parts(ptr_out, size_out) };
            // The SPS says "no reordering" (VT omits it), or decoders hold
            // ~6 frames before showing one; anything else goes out as-is.
            match super::sps::without_reordering(set) {
                Some(sps) => out.extend_from_slice(&sps),
                None => out.extend_from_slice(set),
            }
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
        block.copy_data_bytes(
            0,
            len,
            NonNull::new(avcc.as_mut_ptr() as *mut c_void).unwrap(),
        )
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
    Ok(EncodedAu {
        annex_b: out,
        keyframe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pixel_buffer::PixelBuffer;

    fn gradient_bgra(width: u32, height: u32, shift: usize) -> Vec<u8> {
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
        bgra
    }

    /// In-memory BGRA (the pre-zero-copy input, still used for the
    /// capturer's placeholder).
    fn gradient_frame(width: u32, height: u32, shift: usize) -> RawFrame {
        let bgra = gradient_bgra(width, height, shift);
        RawFrame::from_bgra(width, height, width as usize * 4, bgra, 0)
    }

    /// An IOSurface-backed NV12 pixel buffer, as ScreenCaptureKit delivers
    /// for H.264.
    fn nv12_frame(width: u32, height: u32, shift: usize) -> RawFrame {
        let bgra = gradient_bgra(width, height, shift);
        let buffer = PixelBuffer::nv12_from_bgra(width, height, &bgra, width as usize * 4).unwrap();
        RawFrame::from_pixel_buffer(buffer, 0)
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
        assert!(
            delta_total / 9 < key.len(),
            "deltas not smaller than keyframe"
        );
    }

    #[test]
    fn resolution_change_recreates_the_session() {
        let mut enc = VtH264Encoder::new(2000).unwrap();
        let a = enc.encode(&gradient_frame(640, 400, 0)).unwrap();
        let b = enc.encode(&gradient_frame(320, 200, 0)).unwrap();
        // both must be keyframes with parameter sets (new session each)
        for (label, au) in [("first", &a), ("second", &b)] {
            let types = nal_types(au);
            assert!(
                types.contains(&5),
                "{label} AU lacks IDR after session (re)create"
            );
        }
    }

    /// Output a session delivers too late (after its encode timed out, or
    /// after it was torn down) must never be taken for a later frame's.
    #[test]
    fn stale_output_is_discarded_not_returned_for_the_next_frame() {
        let mut enc = VtH264Encoder::new(2000).unwrap();
        enc.encode(&gradient_frame(640, 400, 0)).unwrap();
        // As if the last frame's callback, or an old session's, landed late.
        // SAFETY: tx_raw is the live sender `new` leaked.
        let tx = unsafe { &*enc.tx_raw };
        for stale_tag in [enc.frame_tag, enc.frame_tag - 1] {
            let bogus = EncodedAu {
                annex_b: vec![0xde, 0xad],
                keyframe: false,
            };
            tx.try_send((stale_tag, Ok(bogus))).unwrap();
        }
        let au = enc.encode(&gradient_frame(640, 400, 1)).unwrap();
        assert_eq!(
            &au[..4],
            &START_CODE,
            "stale output returned: {:?}",
            &au[..]
        );
        assert!(!nal_types(&au).is_empty());
        // ...and the encoder is in step afterwards: nothing left queued.
        assert!(enc.rx.try_recv().is_err());
    }

    /// The capture cap (`MAX_SIZE`, in either orientation) must stay within
    /// what VideoToolbox encodes in hardware.
    #[test]
    fn max_size_is_hardware_encoded_in_both_orientations() {
        let (long, short) = (MAX_SIZE.width, MAX_SIZE.height);
        for (w, h) in [(long, short), (short, long)] {
            let mut enc = VtH264Encoder::new(8000).unwrap();
            let bgra = vec![128u8; w as usize * h as usize * 4];
            let buffer = PixelBuffer::nv12_from_bgra(w, h, &bgra, w as usize * 4).unwrap();
            enc.encode(&RawFrame::from_pixel_buffer(buffer, 0)).unwrap();
            let session = enc.session.as_ref().expect("a session");
            let mut value: *const CFType = ptr::null();
            // SAFETY: live session, valid key, out-pointer to a live local
            // (filled with a +1 reference or left null).
            let status = unsafe {
                objc2_video_toolbox::VTSessionCopyProperty(
                    session.as_ref(),
                    objc2_video_toolbox::kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                    None,
                    (&mut value as *mut *const CFType).cast(),
                )
            };
            let hardware = NonNull::new(value as *mut CFType)
                // SAFETY: copy rule — we own the returned reference.
                .map(|v| unsafe { CFRetained::from_raw(v) })
                .and_then(|v| v.downcast_ref::<CFBoolean>().map(|b| b.as_bool()));
            assert_eq!(
                hardware,
                Some(true),
                "{w}x{h} not hardware-encoded (status {status})"
            );
        }
    }

    #[test]
    fn nv12_pixel_buffers_encode_directly_and_repeats_are_near_free() {
        let mut enc = VtH264Encoder::new(4000).unwrap();
        let first = nv12_frame(640, 400, 0);
        let key = enc.encode(&first).unwrap();
        let types = nal_types(&key);
        for (nal, name) in [(7, "SPS"), (8, "PPS"), (5, "IDR")] {
            assert!(types.contains(&nal), "{name} missing: {types:?}");
        }
        assert_eq!(enc.format, crate::capture::pixel_buffer::NV12_VIDEO_RANGE);
        for i in 1..6 {
            let au = enc.encode(&nv12_frame(640, 400, i)).unwrap();
            assert!(!nal_types(&au).contains(&5), "frame {i} unexpectedly IDR");
        }
        // The idle repeat re-submits the very same retained buffer.
        let still = nv12_frame(640, 400, 9);
        enc.encode(&still).unwrap();
        for _ in 0..3 {
            let repeat = RawFrame {
                timestamp_micros: 1,
                ..still.clone()
            };
            let au = enc.encode(&repeat).unwrap();
            assert!(au.len() < 2048, "repeat P-frame is {} bytes", au.len());
        }
    }

    #[test]
    fn pixel_format_change_recreates_the_session() {
        let mut enc = VtH264Encoder::new(2000).unwrap();
        // placeholder BGRA, then the stream's NV12, then a BGRA surface
        let frames = [
            gradient_frame(640, 400, 0),
            nv12_frame(640, 400, 1),
            RawFrame::from_pixel_buffer(
                PixelBuffer::from_bgra(640, 400, &gradient_bgra(640, 400, 2), 640 * 4).unwrap(),
                0,
            ),
        ];
        for (i, frame) in frames.iter().enumerate() {
            let au = enc.encode(frame).unwrap();
            assert!(
                nal_types(&au).contains(&5),
                "frame {i} after a format change lacks an IDR"
            );
        }
        // same format again: no new session, so a delta
        let au = enc.encode(&frames[2]).unwrap();
        assert!(!nal_types(&au).contains(&5));
    }

    /// The SPS must carry a BT.709 video-range colour description for both
    /// inputs, so decoders don't fall back to guessing (BT.601).
    #[test]
    fn sps_signals_bt709_video_range() {
        for frame in [gradient_frame(640, 400, 0), nv12_frame(640, 400, 0)] {
            let mut enc = VtH264Encoder::new(2000).unwrap();
            let au = enc.encode(&frame).unwrap();
            let colour = sps_colour(&au).expect("SPS lacks a VUI colour description");
            // (full range, primaries, transfer, matrix): 1 = BT.709
            assert_eq!(colour, (false, 1, 1, 1), "{:?}", frame.pixels);
        }
    }

    /// The SPS tells decoders no frame is ever reordered, so they output each
    /// picture as soon as it is decoded instead of filling a reorder buffer.
    #[test]
    fn sps_signals_no_frame_reordering() {
        for frame in [gradient_frame(640, 400, 0), nv12_frame(3420, 2214, 0)] {
            let mut enc = VtH264Encoder::new(4000).unwrap();
            let au = enc.encode(&frame).unwrap();
            let start = au
                .windows(5)
                .position(|w| w[..4] == START_CODE && w[4] & 0x1F == 7)
                .expect("SPS")
                + 4;
            let end = au[start..]
                .windows(4)
                .position(|w| w == START_CODE)
                .map_or(au.len(), |e| start + e);
            assert_eq!(
                super::super::sps::reorder_frames(&au[start..end]),
                Some(0),
                "{}x{}",
                frame.width,
                frame.height
            );
        }
    }

    /// The SPS's VUI colour description: (full_range, colour_primaries,
    /// transfer_characteristics, matrix_coefficients).
    fn sps_colour(annex_b: &[u8]) -> Option<(bool, u32, u32, u32)> {
        let start = annex_b
            .windows(5)
            .position(|w| w[..4] == START_CODE && w[4] & 0x1F == 7)?
            + 5;
        let end = annex_b[start..]
            .windows(4)
            .position(|w| w == START_CODE)
            .map_or(annex_b.len(), |e| start + e);
        // strip emulation-prevention bytes
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for &b in &annex_b[start..end] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        let mut r = Bits {
            data: &rbsp,
            pos: 0,
        };
        let profile = r.u(8);
        r.u(16); // constraint flags, level
        r.ue(); // seq_parameter_set_id
        if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
            if r.ue() == 3 {
                r.u(1);
            }
            r.ue();
            r.ue();
            r.u(1);
            if r.u(1) == 1 {
                return None; // scaling matrices: not produced by VT here
            }
        }
        r.ue(); // log2_max_frame_num_minus4
        match r.ue() {
            0 => {
                r.ue();
            }
            1 => {
                r.u(1);
                r.se();
                r.se();
                for _ in 0..r.ue() {
                    r.se();
                }
            }
            _ => {}
        }
        r.ue(); // max_num_ref_frames
        r.u(1); // gaps_in_frame_num_value_allowed_flag
        r.ue(); // pic_width_in_mbs_minus1
        r.ue(); // pic_height_in_map_units_minus1
        if r.u(1) == 0 {
            r.u(1); // mb_adaptive_frame_field_flag
        }
        r.u(1); // direct_8x8_inference_flag
        if r.u(1) == 1 {
            for _ in 0..4 {
                r.ue(); // frame cropping offsets
            }
        }
        if r.u(1) == 0 {
            return None; // no VUI
        }
        if r.u(1) == 1 && r.u(8) == 255 {
            r.u(32); // extended sample aspect ratio
        }
        if r.u(1) == 1 {
            r.u(1); // overscan_appropriate_flag
        }
        if r.u(1) == 0 {
            return None; // no video_signal_type
        }
        r.u(3); // video_format
        let full_range = r.u(1) == 1;
        if r.u(1) == 0 {
            return None; // no colour_description
        }
        Some((full_range, r.u(8), r.u(8), r.u(8)))
    }

    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Bits<'_> {
        fn u(&mut self, n: usize) -> u32 {
            let mut v = 0;
            for _ in 0..n {
                let bit = self
                    .data
                    .get(self.pos / 8)
                    .map_or(0, |b| (b >> (7 - self.pos % 8)) & 1);
                v = (v << 1) | u32::from(bit);
                self.pos += 1;
            }
            v
        }
        fn ue(&mut self) -> u32 {
            let mut zeros = 0;
            while self.u(1) == 0 && zeros < 32 {
                zeros += 1;
            }
            (1 << zeros) - 1 + self.u(zeros)
        }
        fn se(&mut self) -> i64 {
            let k = i64::from(self.ue());
            if k % 2 == 1 { (k + 1) / 2 } else { -k / 2 }
        }
    }

    /// Native-resolution encode cost: the old input (in-memory BGRA, which
    /// VT must copy into an IOSurface and convert) against IOSurface pixel
    /// buffers (BGRA, and the NV12 the H.264 capture now delivers), plus the
    /// per-frame copy the old capture callback made. Frames are paced at
    /// 30 fps like the live pipeline, and every frame differs.
    ///
    /// cargo test --release -p tetherd --lib bench_ -- --ignored --nocapture
    /// (`TETHER_BENCH_SIZE=WxH` overrides the main display's native size.)
    #[test]
    #[ignore = "benchmark; run explicitly with --ignored --nocapture"]
    fn bench_zero_copy_vs_bytes_at_native_resolution() {
        const DISTINCT: usize = 4;
        const FRAMES: usize = 90;
        let (w, h) = std::env::var("TETHER_BENCH_SIZE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or_else(|| {
                let mode = core_graphics::display::CGDisplay::main().display_mode();
                mode.map_or((3420, 2214), |m| {
                    (m.pixel_width() as u32, m.pixel_height() as u32)
                })
            });
        println!("{w}x{h}, {FRAMES} frames per input, paced at 30 fps");

        // screen-like content: a static texture, a moving box, a scrolling band
        let image = |i: usize| {
            let mut bgra = vec![0u8; w as usize * h as usize * 4];
            for (p, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (p % w as usize, p / w as usize);
                let texture = ((x * 7 + y * 13) ^ (x * y)) as u8;
                let in_box = (x / 256) == i * 2 && (y / 256) == 2;
                let band = y / 64 == 5 && ((x + i * 48) / 6).is_multiple_of(3);
                let v = if in_box || band { 255 } else { texture };
                *px = [v, v / 2, 128, 255];
            }
            bgra
        };
        let images: Vec<Vec<u8>> = (0..DISTINCT).map(image).collect();
        let pitch = w as usize * 4;
        let surfaces = |nv12: bool| -> Vec<RawFrame> {
            images
                .iter()
                .map(|img| {
                    let buffer = if nv12 {
                        PixelBuffer::nv12_from_bgra(w, h, img, pitch)
                    } else {
                        PixelBuffer::from_bgra(w, h, img, pitch)
                    };
                    RawFrame::from_pixel_buffer(buffer.unwrap(), 0)
                })
                .collect()
        };
        let bgra_surfaces = surfaces(false);
        let nv12_surfaces = surfaces(true);
        let bytes: Vec<RawFrame> = images
            .into_iter()
            .map(|img| RawFrame::from_bgra(w, h, pitch, img, 0))
            .collect();

        let cpu = || {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
            // SAFETY: getrusage fills the struct it is given.
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
            // SAFETY: zero-initialised, then filled by getrusage.
            let usage = unsafe { usage.assume_init() };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(usage.ru_utime) + tv(usage.ru_stime)
        };
        let report = |label: &str, mut ms: Vec<f64>, cpu_s: f64| {
            ms.sort_by(f64::total_cmp);
            let avg = ms.iter().sum::<f64>() / ms.len() as f64;
            let p95 = ms[(ms.len() - 1) * 95 / 100];
            println!(
                "{label:<34} avg {avg:6.2} ms | p50 {:6.2} | p95 {p95:6.2} | CPU {:5.2} ms/frame",
                ms[ms.len() / 2],
                cpu_s * 1000.0 / ms.len() as f64,
            );
        };

        // What the old capture callback did per frame on SCK's queue:
        // lock the BGRA surface and copy it out.
        let (mut ms, cpu0) = (Vec::new(), cpu());
        for i in 0..FRAMES {
            let t = Instant::now();
            let copy = bgra_surfaces[i % DISTINCT]
                .with_bgra(|px, _| px.to_vec())
                .unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(copy);
        }
        report("old capture copy (IOSurface→Vec)", ms, cpu() - cpu0);

        // Animated: every frame differs. Static: the capturer's idle repeat —
        // one image re-submitted (every 250 ms live; paced at 30 fps here).
        for animated in [true, false] {
            println!(
                "-- {}",
                if animated {
                    "animated"
                } else {
                    "static (idle repeats)"
                }
            );
            for (label, frames) in [
                ("encode old: BGRA bytes", &bytes),
                ("encode BGRA IOSurface", &bgra_surfaces),
                ("encode new: NV12 IOSurface", &nv12_surfaces),
            ] {
                let mut enc = VtH264Encoder::new(4000).unwrap();
                for f in frames {
                    enc.encode(f).unwrap(); // session + first IDR, warm
                }
                let (mut ms, cpu0) = (Vec::new(), cpu());
                let start = Instant::now();
                for i in 0..FRAMES {
                    let due = start + Duration::from_secs_f64(i as f64 / 30.0);
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    let frame = &frames[if animated { i % DISTINCT } else { 0 }];
                    let t = Instant::now();
                    enc.encode(frame).unwrap();
                    ms.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                report(label, ms, cpu() - cpu0);
            }
        }
    }

    /// A static screen: the capturer repeats one image a few times a second.
    /// Keyframes must still recur on the wall-clock interval (a frame-count
    /// interval would stretch to tens of seconds), and the repeats between
    /// them must be near-free P-frames.
    #[test]
    fn keyframes_recur_on_wall_clock_while_idle() {
        let mut enc = VtH264Encoder::new(2000).unwrap();
        enc.keyframe_interval = Duration::from_millis(300);
        let still = gradient_frame(640, 400, 0);
        let start = Instant::now();
        let mut idr_at = Vec::new();
        let mut largest_delta = 0;
        while start.elapsed() < Duration::from_millis(1250) {
            let au = enc.encode(&still).unwrap();
            if nal_types(&au).contains(&5) {
                idr_at.push(start.elapsed());
            } else {
                largest_delta = largest_delta.max(au.len());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            idr_at.len() >= 3,
            "expected an IDR every ~300 ms, got them at {idr_at:?}"
        );
        for pair in idr_at.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap <= Duration::from_millis(450),
                "keyframe gap {gap:?}: {idr_at:?}"
            );
        }
        assert!(
            largest_delta < 2048,
            "idle repeat P-frame is {largest_delta} bytes"
        );
    }
}
