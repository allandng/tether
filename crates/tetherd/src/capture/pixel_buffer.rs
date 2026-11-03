//! CoreVideo pixel buffers as frame payloads (macOS).
//!
//! ScreenCaptureKit hands each frame over as an IOSurface-backed
//! `CVPixelBuffer`. [`PixelBuffer`] retains it, so a frame travels from the
//! capture callback to the encoder without its pixels being copied:
//! VideoToolbox reads the IOSurface directly (zero-copy H.264), and consumers
//! that need bytes (JPEG, tests, smoke examples) lock the buffer and read it
//! in place through [`PixelBuffer::with_bgra`] — converting NV12 to BGRA on
//! the CPU only when the buffer is NV12.
//!
//! Colour: NV12 is BT.709 throughout (matrix, primaries, transfer) — what
//! ScreenCaptureKit produces for `420v`/`420f`, what VideoToolbox's own
//! BGRA → YCbCr conversion uses, and what the H.264 stream signals
//! (`encode/h264.rs`).

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use anyhow::{Context, bail, ensure};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType, Type};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimariesKey,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunctionKey,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrixKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

/// `BGRA`: packed 32-bit BGRA.
pub const BGRA: u32 = kCVPixelFormatType_32BGRA;
/// `420v`: NV12 (bi-planar 4:2:0), video range — VideoToolbox's native
/// H.264 input.
pub const NV12_VIDEO_RANGE: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
/// `420f`: NV12, full range.
pub const NV12_FULL_RANGE: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

/// A retained `CVPixelBuffer` that is only ever read.
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

// SAFETY: CoreFoundation retain/release are thread-safe, so the handle may
// move to and be dropped on any thread (Send), and concurrent read-only
// locks are allowed (Sync). Nothing writes the pixels while a `PixelBuffer`
// holds them: `retain`'s contract requires it of whoever hands a buffer
// over (ScreenCaptureKit hands one over complete and recycles its IOSurface
// only once no one uses it any more), this type only ever locks read-only,
// and the constructors below fill a buffer before wrapping it.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PixelBuffer({}x{} {})",
            self.width(),
            self.height(),
            fourcc(self.pixel_format())
        )
    }
}

impl PixelBuffer {
    /// Retain `buffer` as a read-only frame image.
    ///
    /// # Safety
    ///
    /// Nothing may write to `buffer`'s pixels for as long as the returned
    /// `PixelBuffer` (or any clone of the frame holding it) lives: this
    /// type hands out `&[u8]` views of them and is `Send + Sync` on that
    /// basis. True of ScreenCaptureKit's delivered frames, which are complete
    /// when delivered and whose IOSurfaces are recycled only once released.
    pub unsafe fn retain(buffer: &CVPixelBuffer) -> Self {
        PixelBuffer(buffer.retain())
    }

    pub fn as_cv(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.0) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.0) as u32
    }

    /// The CoreVideo pixel format (a four-char code such as [`NV12_VIDEO_RANGE`]).
    pub fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.0)
    }

    pub fn is_nv12(&self) -> bool {
        matches!(self.pixel_format(), NV12_VIDEO_RANGE | NV12_FULL_RANGE)
    }

    /// The buffer's YCbCr matrix tag (e.g. `ITU_R_709_2`), if it has one.
    pub fn ycbcr_matrix(&self) -> Option<String> {
        // SAFETY: valid key; a null attachment-mode out-pointer is allowed.
        let value = unsafe {
            self.0
                .attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut())
        }?;
        value.downcast_ref::<CFString>().map(|s| s.to_string())
    }

    /// Whether the buffer is tagged with the BT.709 YCbCr matrix.
    pub fn is_bt709(&self) -> bool {
        // SAFETY: reading an immutable CoreVideo CFString constant.
        let bt709 = unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 }.to_string();
        self.ycbcr_matrix().is_some_and(|m| m == bt709)
    }

    /// Run `f` over the image as packed BGRA (`pixels`, `bytes_per_row`).
    /// BGRA is read in place under a read-only lock; NV12 is converted to a
    /// temporary BGRA copy first (BT.709).
    pub fn with_bgra<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> anyhow::Result<R> {
        let cv = self.as_cv();
        let lock = Lock::new(cv, CVPixelBufferLockFlags::ReadOnly)?;
        let (width, height) = (CVPixelBufferGetWidth(cv), CVPixelBufferGetHeight(cv));
        match self.pixel_format() {
            BGRA => {
                let pitch = CVPixelBufferGetBytesPerRow(cv);
                let pixels =
                    lock.plane(CVPixelBufferGetBaseAddress(cv), pitch, height, width * 4)?;
                Ok(f(pixels, pitch))
            }
            format @ (NV12_VIDEO_RANGE | NV12_FULL_RANGE) => {
                let y_pitch = CVPixelBufferGetBytesPerRowOfPlane(cv, 0);
                let uv_pitch = CVPixelBufferGetBytesPerRowOfPlane(cv, 1);
                let y = lock.plane(
                    CVPixelBufferGetBaseAddressOfPlane(cv, 0),
                    y_pitch,
                    CVPixelBufferGetHeightOfPlane(cv, 0),
                    width,
                )?;
                let uv = lock.plane(
                    CVPixelBufferGetBaseAddressOfPlane(cv, 1),
                    uv_pitch,
                    CVPixelBufferGetHeightOfPlane(cv, 1),
                    width.div_ceil(2) * 2,
                )?;
                let mut bgra = vec![0; width * height * 4];
                let nv12 = Nv12 {
                    y,
                    y_pitch,
                    uv,
                    uv_pitch,
                };
                nv12_to_bgra(&nv12, width, height, format == NV12_FULL_RANGE, &mut bgra);
                Ok(f(&bgra, width * 4))
            }
            other => bail!("no CPU readback for pixel format {}", fourcc(other)),
        }
    }

    /// A new IOSurface-backed BGRA buffer holding a copy of `bgra` (rows
    /// `bytes_per_row` apart) — what ScreenCaptureKit delivers in BGRA mode.
    /// For tests and benchmarks.
    pub fn from_bgra(
        width: u32,
        height: u32,
        bgra: &[u8],
        bytes_per_row: usize,
    ) -> anyhow::Result<Self> {
        let (w, h) = (width as usize, height as usize);
        ensure!(
            bgra.len() >= bytes_per_row * h.saturating_sub(1) + w * 4,
            "BGRA too short"
        );
        let cv = create(width, height, BGRA)?;
        {
            let lock = Lock::new(&cv, CVPixelBufferLockFlags::empty())?;
            let pitch = CVPixelBufferGetBytesPerRow(&cv);
            let dst = lock.plane_mut(CVPixelBufferGetBaseAddress(&cv), pitch, h, w * 4)?;
            for row in 0..h {
                dst[row * pitch..][..w * 4].copy_from_slice(&bgra[row * bytes_per_row..][..w * 4]);
            }
        }
        Ok(PixelBuffer(cv))
    }

    /// A new IOSurface-backed NV12 buffer (`420v`, BT.709, tagged as such)
    /// converted from `bgra` — what ScreenCaptureKit delivers in NV12 mode.
    /// For tests and benchmarks.
    pub fn nv12_from_bgra(
        width: u32,
        height: u32,
        bgra: &[u8],
        bytes_per_row: usize,
    ) -> anyhow::Result<Self> {
        let (w, h) = (width as usize, height as usize);
        ensure!(
            bgra.len() >= bytes_per_row * h.saturating_sub(1) + w * 4,
            "BGRA too short"
        );
        let cv = create(width, height, NV12_VIDEO_RANGE)?;
        {
            let lock = Lock::new(&cv, CVPixelBufferLockFlags::empty())?;
            let y_pitch = CVPixelBufferGetBytesPerRowOfPlane(&cv, 0);
            let uv_pitch = CVPixelBufferGetBytesPerRowOfPlane(&cv, 1);
            let y = lock.plane_mut(CVPixelBufferGetBaseAddressOfPlane(&cv, 0), y_pitch, h, w)?;
            let uv = lock.plane_mut(
                CVPixelBufferGetBaseAddressOfPlane(&cv, 1),
                uv_pitch,
                h.div_ceil(2),
                w.div_ceil(2) * 2,
            )?;
            bgra_to_nv12(bgra, bytes_per_row, w, h, y, y_pitch, uv, uv_pitch);
        }
        // Tag it the way ScreenCaptureKit tags its NV12 frames.
        // SAFETY: valid buffer, keys and values are immutable CF constants.
        unsafe {
            for (key, value) in [
                (
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
                (
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (
                    kCVImageBufferTransferFunctionKey,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ),
            ] {
                let value: &CFType = value.as_ref();
                cv.set_attachment(key, value, CVAttachmentMode::ShouldPropagate);
            }
        }
        Ok(PixelBuffer(cv))
    }
}

/// A new, uninitialised IOSurface-backed pixel buffer.
fn create(width: u32, height: u32, format: u32) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
    let no_properties = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: reading an immutable CoreVideo CFString constant.
    let key: &CFString = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attributes =
        CFDictionary::<CFString, CFType>::from_slices(&[key], &[no_properties.as_ref()]);
    let attributes: &CFDictionary = attributes.as_ref();
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: valid attribute dictionary; `out` is a live stack slot.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            format,
            Some(attributes),
            NonNull::from(&mut out),
        )
    };
    let out = NonNull::new(out).filter(|_| status == 0).with_context(|| {
        format!(
            "CVPixelBufferCreate({width}x{height} {}) = {status}",
            fourcc(format)
        )
    })?;
    // SAFETY: create rule — we own the one reference.
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// A base-address lock, released on drop. Plane slices borrow from it, so
/// they cannot outlive the lock.
struct Lock<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

impl<'a> Lock<'a> {
    fn new(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> anyhow::Result<Self> {
        // SAFETY: valid buffer; balanced by the unlock in Drop.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        ensure!(status == 0, "CVPixelBufferLockBaseAddress = {status}");
        Ok(Lock { buffer, flags })
    }

    /// The `rows` rows of a locked plane starting at `base`, `pitch` bytes
    /// apart and `row_len` bytes long (the last row is not padded out).
    fn plane(
        &self,
        base: *mut c_void,
        pitch: usize,
        rows: usize,
        row_len: usize,
    ) -> anyhow::Result<&[u8]> {
        let len = plane_len(base, pitch, rows, row_len)?;
        // SAFETY: CoreVideo guarantees a locked plane spans `pitch` bytes for
        // every row (`len` stops at the last row's pixels) for as long as the
        // lock — which the returned borrow is tied to — is held.
        Ok(unsafe { std::slice::from_raw_parts(base as *const u8, len) })
    }

    /// [`Self::plane`], writable — only for a buffer this module just
    /// created and locked read-write, so nothing else can observe it.
    #[allow(clippy::mut_from_ref)]
    fn plane_mut(
        &self,
        base: *mut c_void,
        pitch: usize,
        rows: usize,
        row_len: usize,
    ) -> anyhow::Result<&mut [u8]> {
        debug_assert!(!self.flags.contains(CVPixelBufferLockFlags::ReadOnly));
        let len = plane_len(base, pitch, rows, row_len)?;
        // SAFETY: as in `plane`; the buffer is freshly created and unshared,
        // and each plane is borrowed mutably once.
        Ok(unsafe { std::slice::from_raw_parts_mut(base as *mut u8, len) })
    }
}

fn plane_len(
    base: *mut c_void,
    pitch: usize,
    rows: usize,
    row_len: usize,
) -> anyhow::Result<usize> {
    ensure!(!base.is_null(), "pixel buffer plane has no base address");
    ensure!(
        row_len <= pitch,
        "row of {row_len} bytes exceeds the {pitch}-byte pitch"
    );
    Ok(match rows {
        0 => 0,
        rows => pitch * (rows - 1) + row_len,
    })
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the successful lock in `new`.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

/// A four-char code (pixel format) as text, e.g. `420v`.
pub(crate) fn fourcc(code: u32) -> String {
    code.to_be_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

/// A borrowed NV12 image: luma plane, interleaved Cb/Cr plane at half
/// resolution in both directions.
struct Nv12<'a> {
    y: &'a [u8],
    y_pitch: usize,
    uv: &'a [u8],
    uv_pitch: usize,
}

/// 16.16 fixed point.
const fn fx(v: f64) -> i32 {
    (v * 65536.0 + 0.5) as i32
}

/// BT.709 NV12 → opaque BGRA (tightly packed, `width * 4` per row).
fn nv12_to_bgra(src: &Nv12<'_>, width: usize, height: usize, full_range: bool, out: &mut [u8]) {
    // R = Y' + 1.5748 Cr', G = Y' − 0.1873 Cb' − 0.4681 Cr', B = Y' + 1.8556 Cb',
    // with Y' and C' expanded from video range (16–235 / 16–240) if needed.
    let (y_off, y_mul, c_mul) = if full_range {
        (0, 1.0, 1.0)
    } else {
        (16, 255.0 / 219.0, 255.0 / 224.0)
    };
    let ky = fx(y_mul);
    let (kr_cr, kg_cb, kg_cr, kb_cb) = (
        fx(1.5748 * c_mul),
        fx(0.187_324 * c_mul),
        fx(0.468_124 * c_mul),
        fx(1.8556 * c_mul),
    );
    let clamp = |v: i32| ((v + (1 << 15)) >> 16).clamp(0, 255) as u8;
    for row in 0..height {
        let y_row = &src.y[row * src.y_pitch..];
        let uv_row = &src.uv[(row / 2) * src.uv_pitch..];
        let out_row = &mut out[row * width * 4..][..width * 4];
        for (col, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let y = (i32::from(y_row[col]) - y_off) * ky;
            let cb = i32::from(uv_row[col / 2 * 2]) - 128;
            let cr = i32::from(uv_row[col / 2 * 2 + 1]) - 128;
            px[0] = clamp(y + kb_cb * cb);
            px[1] = clamp(y - kg_cb * cb - kg_cr * cr);
            px[2] = clamp(y + kr_cr * cr);
            px[3] = 255;
        }
    }
}

/// BGRA → BT.709 video-range NV12. Chroma is the average of each 2×2 block
/// (clamped at odd edges).
#[allow(clippy::too_many_arguments)]
fn bgra_to_nv12(
    bgra: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    y: &mut [u8],
    y_pitch: usize,
    uv: &mut [u8],
    uv_pitch: usize,
) {
    let px = |x: usize, row: usize| {
        let o = row * pitch + x * 4;
        (
            i32::from(bgra[o + 2]),
            i32::from(bgra[o + 1]),
            i32::from(bgra[o]),
        )
    };
    // Y = 16 + (0.1826 R + 0.6142 G + 0.0620 B), video range.
    let (yr, yg, yb) = (fx(0.182_586), fx(0.614_231), fx(0.062_007));
    for row in 0..height {
        for x in 0..width {
            let (r, g, b) = px(x, row);
            y[row * y_pitch + x] = (16 + ((yr * r + yg * g + yb * b + (1 << 15)) >> 16)) as u8;
        }
    }
    let (ur, ug, ub) = (fx(-0.100_644), fx(-0.338_572), fx(0.439_216));
    let (vr, vg, vb) = (fx(0.439_216), fx(-0.398_942), fx(-0.040_274));
    for cy in 0..height.div_ceil(2) {
        for cx in 0..width.div_ceil(2) {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px((2 * cx + dx).min(width - 1), (2 * cy + dy).min(height - 1));
                (r, g, b) = (r + pr, g + pg, b + pb);
            }
            let (r, g, b) = (r / 4, g / 4, b / 4);
            let u = 128 + ((ur * r + ug * g + ub * b + (1 << 15)) >> 16);
            let v = 128 + ((vr * r + vg * g + vb * b + (1 << 15)) >> 16);
            uv[cy * uv_pitch + 2 * cx] = u.clamp(16, 240) as u8;
            uv[cy * uv_pitch + 2 * cx + 1] = v.clamp(16, 240) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Triple = (u8, u8, u8);

    /// Reference colours, (R, G, B) → (Y, Cb, Cr) in BT.709 video range —
    /// the same values VideoToolbox produces for BGRA input.
    const REFERENCE: [(Triple, Triple); 5] = [
        ((255, 0, 0), (63, 102, 240)),
        ((0, 255, 0), (173, 42, 26)),
        ((0, 0, 255), (32, 240, 118)),
        ((255, 255, 255), (235, 128, 128)),
        ((0, 0, 0), (16, 128, 128)),
    ];

    fn solid(width: u32, height: u32, (r, g, b): (u8, u8, u8)) -> Vec<u8> {
        [b, g, r, 255].repeat((width * height) as usize)
    }

    #[test]
    fn nv12_conversion_hits_bt709_reference_values_both_ways() {
        for (rgb, (ey, eu, ev)) in REFERENCE {
            let buffer = PixelBuffer::nv12_from_bgra(4, 2, &solid(4, 2, rgb), 16).unwrap();
            assert_eq!(buffer.pixel_format(), NV12_VIDEO_RANGE);
            assert!(buffer.is_nv12() && buffer.is_bt709(), "{buffer:?}");
            let cv = buffer.as_cv();
            let lock = Lock::new(cv, CVPixelBufferLockFlags::ReadOnly).unwrap();
            let y = lock
                .plane(CVPixelBufferGetBaseAddressOfPlane(cv, 0), 1, 1, 1)
                .unwrap()[0];
            let uv = lock
                .plane(CVPixelBufferGetBaseAddressOfPlane(cv, 1), 2, 1, 2)
                .unwrap();
            let got = (y, uv[0], uv[1]);
            assert!(
                got.0.abs_diff(ey) <= 1 && got.1.abs_diff(eu) <= 1 && got.2.abs_diff(ev) <= 1,
                "{rgb:?} -> {got:?}, expected {:?}",
                (ey, eu, ev)
            );
            drop(lock);

            // ...and back: the CPU readback recovers the colour.
            let (r, g, b) = buffer.with_bgra(|px, _| (px[2], px[1], px[0])).unwrap();
            for (got, want) in [(r, rgb.0), (g, rgb.1), (b, rgb.2)] {
                assert!(
                    got.abs_diff(want) <= 3,
                    "{rgb:?} read back as {:?}",
                    (r, g, b)
                );
            }
        }
    }

    #[test]
    fn bgra_pixel_buffer_reads_back_in_place_with_its_own_pitch() {
        let (w, h) = (37, 5); // odd width: the IOSurface pads its rows
        let mut bgra = vec![0u8; w * h * 4];
        for (i, b) in bgra.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let buffer = PixelBuffer::from_bgra(w as u32, h as u32, &bgra, w * 4).unwrap();
        assert_eq!((buffer.width(), buffer.height()), (w as u32, h as u32));
        assert_eq!(buffer.pixel_format(), BGRA);
        let rows = buffer
            .with_bgra(|px, pitch| {
                assert!(pitch >= w * 4);
                (0..h)
                    .map(|r| px[r * pitch..][..w * 4].to_vec())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .unwrap();
        assert_eq!(rows, bgra);
    }

    #[test]
    fn nv12_readback_handles_odd_sizes_and_full_range() {
        // odd size: chroma covers the trailing column/row
        let buffer = PixelBuffer::nv12_from_bgra(5, 3, &solid(5, 3, (255, 255, 255)), 20).unwrap();
        let all_white = buffer
            .with_bgra(|px, pitch| (0..3).all(|r| px[r * pitch..][..20].iter().all(|&b| b >= 252)))
            .unwrap();
        assert!(all_white);

        // full range: Y=255 is white, Y=0 black, no expansion
        let (y, uv) = ([0u8, 255], [128u8, 128]);
        let nv12 = Nv12 {
            y: &y,
            y_pitch: 2,
            uv: &uv,
            uv_pitch: 2,
        };
        let mut out = [0u8; 8];
        nv12_to_bgra(&nv12, 2, 1, true, &mut out);
        assert_eq!(out, [0, 0, 0, 255, 255, 255, 255, 255]);
    }
}
