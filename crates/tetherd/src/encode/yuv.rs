//! BGRA → I420 (planar YUV 4:2:0) for software H.264 encoders.
//!
//! BT.601 limited range, integer math: what a decoder assumes when the
//! stream carries no colour description, so colours survive the round trip.
//! Odd trailing columns/rows are cropped (4:2:0 needs even dimensions), and a
//! 2:1 box downscale is available for sources larger than the encoder's
//! limit.

/// One I420 picture with tightly packed planes.
#[derive(Debug, Clone, Default)]
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[inline]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline]
fn chroma(b: i32, g: i32, r: i32) -> (u8, u8) {
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

impl I420 {
    /// Convert into `self`, reusing its buffers. `scale` is 1 (native) or 2
    /// (half size, box-filtered).
    pub fn convert_from(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
        pitch: usize,
        scale: usize,
    ) {
        let scale = scale.clamp(1, 2);
        let w = (src_w / scale) & !1;
        let h = (src_h / scale) & !1;
        self.width = w;
        self.height = h;
        self.y.resize(w * h, 0);
        self.u.resize((w / 2) * (h / 2), 0);
        self.v.resize((w / 2) * (h / 2), 0);

        let cw = w / 2;
        if scale == 1 {
            // Hot path (every frame at native size): walk row pairs with
            // plain slices so the optimiser can drop the bounds checks.
            let (ys, us, vs) = (&mut self.y, &mut self.u, &mut self.v);
            for cy in 0..h / 2 {
                let r0 = &bgra[2 * cy * pitch..2 * cy * pitch + w * 4];
                let r1 = &bgra[(2 * cy + 1) * pitch..(2 * cy + 1) * pitch + w * 4];
                let (y0, y1) = ys[2 * cy * w..(2 * cy + 2) * w].split_at_mut(w);
                let urow = &mut us[cy * cw..(cy + 1) * cw];
                let vrow = &mut vs[cy * cw..(cy + 1) * cw];
                for cx in 0..cw {
                    let a = &r0[cx * 8..cx * 8 + 8];
                    let b = &r1[cx * 8..cx * 8 + 8];
                    let p = |s: &[u8], o: usize| (s[o] as i32, s[o + 1] as i32, s[o + 2] as i32);
                    let (p00, p10, p01, p11) = (p(a, 0), p(a, 4), p(b, 0), p(b, 4));
                    y0[cx * 2] = luma(p00.0, p00.1, p00.2);
                    y0[cx * 2 + 1] = luma(p10.0, p10.1, p10.2);
                    y1[cx * 2] = luma(p01.0, p01.1, p01.2);
                    y1[cx * 2 + 1] = luma(p11.0, p11.1, p11.2);
                    let (u, v) = chroma(
                        (p00.0 + p10.0 + p01.0 + p11.0 + 2) >> 2,
                        (p00.1 + p10.1 + p01.1 + p11.1 + 2) >> 2,
                        (p00.2 + p10.2 + p01.2 + p11.2 + 2) >> 2,
                    );
                    urow[cx] = u;
                    vrow[cx] = v;
                }
            }
            return;
        }

        // 2:1 downscale: each output pixel box-filters a 2x2 source block.
        let px = |x: usize, y: usize| -> (i32, i32, i32) {
            let (sx, sy) = (x * 2, y * 2);
            let (mut b, mut g, mut r) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let o = (sy + dy) * pitch + (sx + dx) * 4;
                b += bgra[o] as i32;
                g += bgra[o + 1] as i32;
                r += bgra[o + 2] as i32;
            }
            (b >> 2, g >> 2, r >> 2)
        };
        for cy in 0..h / 2 {
            for cx in 0..cw {
                let (x, y) = (cx * 2, cy * 2);
                let p00 = px(x, y);
                let p10 = px(x + 1, y);
                let p01 = px(x, y + 1);
                let p11 = px(x + 1, y + 1);
                self.y[y * w + x] = luma(p00.0, p00.1, p00.2);
                self.y[y * w + x + 1] = luma(p10.0, p10.1, p10.2);
                self.y[(y + 1) * w + x] = luma(p01.0, p01.1, p01.2);
                self.y[(y + 1) * w + x + 1] = luma(p11.0, p11.1, p11.2);
                let b = (p00.0 + p10.0 + p01.0 + p11.0 + 2) >> 2;
                let g = (p00.1 + p10.1 + p01.1 + p11.1 + 2) >> 2;
                let r = (p00.2 + p10.2 + p01.2 + p11.2 + 2) >> 2;
                let (u, v) = chroma(b, g, r);
                self.u[cy * cw + cx] = u;
                self.v[cy * cw + cx] = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, pitch: usize, bgr: (u8, u8, u8)) -> Vec<u8> {
        let mut buf = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * pitch + x * 4;
                buf[o] = bgr.0;
                buf[o + 1] = bgr.1;
                buf[o + 2] = bgr.2;
                buf[o + 3] = 255;
            }
        }
        buf
    }

    #[test]
    fn reference_colours_hit_bt601_limited_values() {
        let mut out = I420::default();
        for (bgr, (y, u, v)) in [
            ((0, 0, 0), (16, 128, 128)),
            ((255, 255, 255), (235, 128, 128)),
            ((0, 0, 255), (82, 90, 240)),  // red
            ((0, 255, 0), (145, 54, 34)),  // green
            ((255, 0, 0), (41, 240, 110)), // blue
        ] {
            out.convert_from(&solid(4, 4, 16, bgr), 4, 4, 16, 1);
            let near = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 1;
            assert!(near(out.y[0], y), "{bgr:?}: Y {} vs {y}", out.y[0]);
            assert!(near(out.u[0], u), "{bgr:?}: U {} vs {u}", out.u[0]);
            assert!(near(out.v[0], v), "{bgr:?}: V {} vs {v}", out.v[0]);
        }
    }

    #[test]
    fn odd_dimensions_crop_and_padded_rows_are_respected() {
        let mut out = I420::default();
        // 5x3 with 8 bytes of row padding
        out.convert_from(&solid(5, 3, 28, (255, 255, 255)), 5, 3, 28, 1);
        assert_eq!((out.width, out.height), (4, 2));
        assert_eq!(out.y.len(), 8);
        assert_eq!(out.u.len(), 2);
        assert!(out.y.iter().all(|&y| y == 235));
    }

    /// Release-mode timing: `cargo test --release -p tetherd yuv_1080p -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn yuv_1080p_timing() {
        let buf = solid(1920, 1080, 1920 * 4, (10, 200, 30));
        let mut out = I420::default();
        let t = std::time::Instant::now();
        for _ in 0..50 {
            out.convert_from(&buf, 1920, 1080, 1920 * 4, 1);
        }
        println!(
            "bgra→i420 1080p: {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / 50.0
        );
    }

    #[test]
    fn half_scale_box_filters() {
        // 4x4 checkerboard of black/white averages to mid grey at 2x2.
        let mut buf = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                if (x + y) % 2 == 0 {
                    buf[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3].fill(255);
                }
            }
        }
        let mut out = I420::default();
        out.convert_from(&buf, 4, 4, 16, 2);
        assert_eq!((out.width, out.height), (2, 2));
        assert!(
            out.y.iter().all(|&y| (124..=127).contains(&y)),
            "{:?}",
            out.y
        );
    }
}
