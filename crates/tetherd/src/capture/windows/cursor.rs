//! Software cursor compositing for Desktop Duplication.
//!
//! DXGI hands back the desktop image *without* the hardware cursor and the
//! pointer shape separately, so the capturer draws it onto each outgoing
//! frame itself. Pure functions over BGRA buffers, so the three DXGI shape
//! encodings are unit-tested without a GPU.

/// DXGI_OUTDUPL_POINTER_SHAPE_TYPE values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// 1 bpp: an AND mask over an XOR mask, stacked (`height` covers both).
    Monochrome,
    /// 32 bpp BGRA, alpha-blended.
    Color,
    /// 32 bpp BGRA where the alpha byte is a mask: 0xFF = XOR the RGB onto the
    /// screen, 0x00 = replace the screen pixel with the RGB.
    MaskedColor,
}

impl ShapeKind {
    pub fn from_dxgi(t: u32) -> Option<Self> {
        match t {
            1 => Some(ShapeKind::Monochrome),
            2 => Some(ShapeKind::Color),
            4 => Some(ShapeKind::MaskedColor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PointerShape {
    pub kind: ShapeKind,
    pub width: u32,
    /// As DXGI reports it — for `Monochrome` this is twice the visible height.
    pub height: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

/// Draw `shape` with its top-left corner at (`x`, `y`) — DXGI's pointer
/// position is the shape's top-left, not the hotspot. Clipped to the frame.
pub fn draw(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    fpitch: usize,
    shape: &PointerShape,
    x: i32,
    y: i32,
) {
    let visible_h = match shape.kind {
        ShapeKind::Monochrome => shape.height / 2,
        _ => shape.height,
    };
    for sy in 0..visible_h {
        let dy = y + sy as i32;
        if dy < 0 || dy >= fh as i32 {
            continue;
        }
        for sx in 0..shape.width {
            let dx = x + sx as i32;
            if dx < 0 || dx >= fw as i32 {
                continue;
            }
            let d = dy as usize * fpitch + dx as usize * 4;
            let Some(dst) = frame.get_mut(d..d + 4) else {
                continue;
            };
            match shape.kind {
                ShapeKind::Monochrome => {
                    let byte = sx as usize / 8;
                    let bit = 0x80u8 >> (sx % 8);
                    let and_row = sy as usize * shape.pitch as usize;
                    let xor_row = (sy + visible_h) as usize * shape.pitch as usize;
                    let (Some(&and), Some(&xor)) = (
                        shape.data.get(and_row + byte),
                        shape.data.get(xor_row + byte),
                    ) else {
                        continue;
                    };
                    let and_mask = if and & bit != 0 { 0xFF } else { 0x00 };
                    let xor_mask = if xor & bit != 0 { 0xFF } else { 0x00 };
                    for c in dst.iter_mut().take(3) {
                        *c = (*c & and_mask) ^ xor_mask;
                    }
                    dst[3] = 0xFF;
                }
                ShapeKind::Color | ShapeKind::MaskedColor => {
                    let s = sy as usize * shape.pitch as usize + sx as usize * 4;
                    let Some(src) = shape.data.get(s..s + 4) else {
                        continue;
                    };
                    if shape.kind == ShapeKind::MaskedColor {
                        if src[3] == 0xFF {
                            for i in 0..3 {
                                dst[i] ^= src[i];
                            }
                        } else {
                            dst[..3].copy_from_slice(&src[..3]);
                        }
                    } else {
                        let a = u32::from(src[3]);
                        for i in 0..3 {
                            dst[i] = ((u32::from(src[i]) * a + u32::from(dst[i]) * (255 - a)) / 255)
                                as u8;
                        }
                    }
                    dst[3] = 0xFF;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> Vec<u8> {
        vec![fill; (w * h * 4) as usize]
    }

    fn px(f: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * w + x) * 4) as usize;
        [f[o], f[o + 1], f[o + 2], f[o + 3]]
    }

    #[test]
    fn color_shape_alpha_blends_and_clips() {
        let mut f = frame(4, 4, 100);
        // 2x2 shape: opaque red, transparent, half-alpha white, opaque blue
        let shape = PointerShape {
            kind: ShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            data: vec![
                0, 0, 255, 255, 9, 9, 9, 0, 255, 255, 255, 128, 255, 0, 0, 255,
            ],
        };
        draw(&mut f, 4, 4, 16, &shape, 3, 3); // only the top-left pixel lands
        assert_eq!(px(&f, 4, 3, 3), [0, 0, 255, 255]);
        assert_eq!(
            px(&f, 4, 2, 2),
            [100, 100, 100, 100],
            "outside shape untouched"
        );

        let mut f = frame(4, 4, 100);
        draw(&mut f, 4, 4, 16, &shape, 0, 0);
        assert_eq!(
            px(&f, 4, 1, 0),
            [100, 100, 100, 255],
            "transparent keeps screen"
        );
        let half = px(&f, 4, 0, 1)[0];
        assert!((176..=178).contains(&half), "half-alpha blend, got {half}");
        assert_eq!(px(&f, 4, 1, 1), [255, 0, 0, 255]);
    }

    #[test]
    fn negative_positions_clip_instead_of_wrapping() {
        let mut f = frame(2, 2, 7);
        let shape = PointerShape {
            kind: ShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            data: vec![255; 16],
        };
        draw(&mut f, 2, 2, 8, &shape, -1, -1);
        assert_eq!(px(&f, 2, 0, 0), [255, 255, 255, 255]);
        assert_eq!(px(&f, 2, 1, 1), [7, 7, 7, 7]);
    }

    #[test]
    fn monochrome_and_xor_masks() {
        // 2 wide, 2 visible rows → height 4; one byte per row.
        // row0: AND=0 XOR=0 → black; AND=1 XOR=0 → transparent
        // row1: AND=1 XOR=1 → invert; AND=0 XOR=1 → white
        let shape = PointerShape {
            kind: ShapeKind::Monochrome,
            width: 2,
            height: 4,
            pitch: 1,
            data: vec![0b0100_0000, 0b1000_0000, 0b0000_0000, 0b1100_0000],
        };
        let mut f = frame(2, 2, 0x30);
        draw(&mut f, 2, 2, 8, &shape, 0, 0);
        assert_eq!(px(&f, 2, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&f, 2, 1, 0), [0x30, 0x30, 0x30, 255]);
        assert_eq!(px(&f, 2, 0, 1), [0xCF, 0xCF, 0xCF, 255]);
        assert_eq!(px(&f, 2, 1, 1), [0xFF, 0xFF, 0xFF, 255]);
    }

    #[test]
    fn masked_color_xors_or_replaces() {
        let shape = PointerShape {
            kind: ShapeKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![0xFF, 0x00, 0x0F, 0xFF, 1, 2, 3, 0x00],
        };
        let mut f = frame(2, 1, 0xF0);
        draw(&mut f, 2, 1, 8, &shape, 0, 0);
        assert_eq!(px(&f, 2, 0, 0), [0x0F, 0xF0, 0xFF, 255]);
        assert_eq!(px(&f, 2, 1, 0), [1, 2, 3, 255]);
    }

    #[test]
    fn truncated_shape_data_is_ignored_not_panicking() {
        let shape = PointerShape {
            kind: ShapeKind::Color,
            width: 8,
            height: 8,
            pitch: 32,
            data: vec![255; 10],
        };
        let mut f = frame(8, 8, 0);
        draw(&mut f, 8, 8, 32, &shape, 0, 0);
    }
}
