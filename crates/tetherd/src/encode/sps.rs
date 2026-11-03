//! H.264 SPS rewrite: tell decoders the stream never reorders frames.
//!
//! VideoToolbox (with `AllowFrameReordering` off) emits no B-frames, but its
//! SPS carries no VUI `bitstream_restriction`. Without it a conforming decoder
//! must assume output may be reordered and hold up to `max_dec_frame_buffering`
//! decoded pictures — inferred from the level — before showing one. For a
//! 3420×2214 Retina capture at level 5.1 that is 6 frames: measured in Chrome's
//! WebCodecs decoder as a steady ~200 ms between `decode()` and output, on top
//! of everything else in the glass-to-glass path.
//!
//! [`without_reordering`] rewrites the SPS with `bitstream_restriction_flag = 1`,
//! `max_num_reorder_frames = 0` and `max_dec_frame_buffering =
//! max_num_ref_frames`, which is what the stream actually does, so each picture
//! is output as soon as it is decoded. Everything before the VUI's restriction
//! fields is copied bit for bit; an SPS this parser doesn't fully understand is
//! left alone (`None`), so the worst case is the old latency, never a broken
//! stream.

/// Rewrite an SPS NAL unit (1-byte header + emulation-prevented RBSP, no start
/// code) to signal no frame reordering. `None` when the input isn't an SPS,
/// can't be parsed completely, or already signals `max_num_reorder_frames = 0`
/// — the caller keeps the original then.
pub fn without_reordering(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, payload) = nal.split_first()?;
    if header & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape(payload);
    let sps = parse(&rbsp)?;
    if sps.max_num_reorder_frames == Some(0) {
        return None;
    }

    let mut w = BitWriter::default();
    let mut r = BitReader::new(&rbsp);
    // copy everything before the restriction flag (or the VUI-present flag)
    for _ in 0..sps.rewrite_at {
        w.bit(r.bit()?);
    }
    if !sps.vui_present {
        w.bit(true); // vui_parameters_present_flag
        // aspect_ratio_info, overscan_info, video_signal_type, chroma_loc_info,
        // timing_info, nal_hrd, vcl_hrd, pic_struct: all absent
        for _ in 0..8 {
            w.bit(false);
        }
    }
    let kept = sps.restriction.unwrap_or_default();
    w.bit(true); // bitstream_restriction_flag
    w.bit(kept.mv_over_pic_boundaries);
    w.ue(kept.max_bytes_per_pic_denom);
    w.ue(kept.max_bits_per_mb_denom);
    w.ue(kept.log2_max_mv_length_horizontal);
    w.ue(kept.log2_max_mv_length_vertical);
    w.ue(0); // max_num_reorder_frames
    // the DPB must still hold every reference frame
    w.ue(kept
        .max_dec_frame_buffering
        .max(sps.max_num_ref_frames)
        .max(1));
    w.bit(true); // rbsp_stop_one_bit, then byte-align with zeros
    let mut out = vec![header];
    out.extend(escape(&w.finish()));
    Some(out)
}

/// The restriction fields worth preserving from an SPS that already has them.
/// `max_num_reorder_frames` an SPS NAL unit signals (`None`: not signalled,
/// or not parseable).
#[cfg(all(test, target_os = "macos"))] // only the VideoToolbox tests read it
pub(crate) fn reorder_frames(nal: &[u8]) -> Option<u32> {
    parse(&unescape(nal.get(1..)?))?.max_num_reorder_frames
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Restriction {
    mv_over_pic_boundaries: bool,
    max_bytes_per_pic_denom: u32,
    max_bits_per_mb_denom: u32,
    log2_max_mv_length_horizontal: u32,
    log2_max_mv_length_vertical: u32,
    max_dec_frame_buffering: u32,
}

impl Default for Restriction {
    /// For an SPS that had none: no size limits claimed (0 = unrestricted),
    /// motion vectors up to the largest length the syntax allows.
    fn default() -> Self {
        Restriction {
            mv_over_pic_boundaries: true,
            max_bytes_per_pic_denom: 0,
            max_bits_per_mb_denom: 0,
            log2_max_mv_length_horizontal: 15,
            log2_max_mv_length_vertical: 15,
            max_dec_frame_buffering: 0,
        }
    }
}

#[derive(Debug)]
struct Parsed {
    max_num_ref_frames: u32,
    vui_present: bool,
    /// Bit offset (in the RBSP) where the rewrite starts: the
    /// `bitstream_restriction_flag` when there is a VUI, else the
    /// `vui_parameters_present_flag`.
    rewrite_at: usize,
    restriction: Option<Restriction>,
    max_num_reorder_frames: Option<u32>,
}

/// Parse an SPS RBSP (H.264 7.3.2.1.1 + E.1.1) far enough to know where its
/// VUI restriction fields sit, checking that only the stop bit follows.
fn parse(rbsp: &[u8]) -> Option<Parsed> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(8)?; // constraint_set flags + reserved
    r.bits(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if r.ue()? == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            return None; // scaling matrices: not emitted by our encoders
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                r.se()?; // offset_for_ref_frame[i]
            }
        }
        2 => {}
        _ => return None,
    }
    let max_num_ref_frames = r.ue()?;
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.bit()? {
        r.bit()?; // mb_adaptive_frame_field_flag (frame_mbs_only_flag == 0)
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?; // frame_crop_{left,right,top,bottom}_offset
        }
    }
    let vui_flag_at = r.pos;
    let vui_present = r.bit()?;
    let mut parsed = Parsed {
        max_num_ref_frames,
        vui_present,
        rewrite_at: vui_flag_at,
        restriction: None,
        max_num_reorder_frames: None,
    };
    if vui_present {
        if r.bit()? && r.bits(8)? == 255 {
            r.bits(16)?; // sar_width
            r.bits(16)?; // sar_height
        }
        if r.bit()? {
            r.bit()?; // overscan_appropriate_flag
        }
        if r.bit()? {
            r.bits(3)?; // video_format
            r.bit()?; // video_full_range_flag
            if r.bit()? {
                r.bits(24)?; // colour_primaries, transfer, matrix
            }
        }
        if r.bit()? {
            r.ue()?; // chroma_sample_loc_type_top_field
            r.ue()?; // chroma_sample_loc_type_bottom_field
        }
        if r.bit()? {
            r.bits(32)?; // num_units_in_tick
            r.bits(32)?; // time_scale
            r.bit()?; // fixed_frame_rate_flag
        }
        let nal_hrd = r.bit()?;
        if nal_hrd {
            hrd_parameters(&mut r)?;
        }
        let vcl_hrd = r.bit()?;
        if vcl_hrd {
            hrd_parameters(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.bit()?; // low_delay_hrd_flag
        }
        r.bit()?; // pic_struct_present_flag
        parsed.rewrite_at = r.pos;
        if r.bit()? {
            let mv_over_pic_boundaries = r.bit()?;
            let max_bytes_per_pic_denom = r.ue()?;
            let max_bits_per_mb_denom = r.ue()?;
            let log2_max_mv_length_horizontal = r.ue()?;
            let log2_max_mv_length_vertical = r.ue()?;
            parsed.max_num_reorder_frames = Some(r.ue()?);
            let max_dec_frame_buffering = r.ue()?;
            parsed.restriction = Some(Restriction {
                mv_over_pic_boundaries,
                max_bytes_per_pic_denom,
                max_bits_per_mb_denom,
                log2_max_mv_length_horizontal,
                log2_max_mv_length_vertical,
                max_dec_frame_buffering,
            });
        }
    }
    // Only rbsp_trailing_bits may follow: a stop bit, then zero padding. Any
    // other trailing data is syntax we don't know; leave such an SPS alone.
    if !r.bit()? {
        return None;
    }
    while let Some(b) = r.bit() {
        if b {
            return None;
        }
    }
    Some(parsed)
}

/// E.1.2 hrd_parameters(), skipped.
fn hrd_parameters(r: &mut BitReader) -> Option<()> {
    let cpb_cnt_minus1 = r.ue()?;
    if cpb_cnt_minus1 > 31 {
        return None;
    }
    r.bits(4)?; // bit_rate_scale
    r.bits(4)?; // cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.bit()?; // cbr_flag
    }
    r.bits(20)?; // four 5-bit length fields
    Some(())
}

/// Strip emulation-prevention bytes (`00 00 03` → `00 00`).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Insert emulation-prevention bytes so no start code can appear in the NAL.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1 == 1;
        self.pos += 1;
        Some(bit)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |v, _| Some(v << 1 | u32::from(self.bit()?)))
    }

    /// Exp-Golomb ue(v); `None` past the end or beyond 32 bits.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    len: usize,
}

impl BitWriter {
    fn bit(&mut self, b: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().expect("pushed") |= 1 << (7 - self.len % 8);
        }
        self.len += 1;
    }

    fn ue(&mut self, v: u32) {
        let v = u64::from(v) + 1;
        let width = 64 - v.leading_zeros();
        for _ in 1..width {
            self.bit(false);
        }
        for i in (0..width).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// The bytes written; a partial last byte is zero-padded.
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The SPS VideoToolbox produced for a 3420×2214 capture on an M3 (Main
    /// profile, level 5.1, one reference frame, BT.709 VUI, no restriction).
    const VT_SPS: &str = "274d0033ab401ac022fdcd3501010102";

    fn parsed(nal: &[u8]) -> Parsed {
        parse(&unescape(&nal[1..])).expect("parses")
    }

    #[test]
    fn video_toolbox_sps_gains_a_no_reorder_restriction() {
        let original = hex(VT_SPS);
        let before = parsed(&original);
        assert!(before.vui_present);
        assert_eq!(before.max_num_ref_frames, 1);
        assert_eq!(before.max_num_reorder_frames, None);

        let rewritten = without_reordering(&original).expect("rewritten");
        assert_eq!(rewritten[0], original[0], "NAL header kept");
        let after = parsed(&rewritten);
        assert_eq!(after.max_num_reorder_frames, Some(0));
        let r = after.restriction.expect("restriction present");
        assert_eq!(
            r.max_dec_frame_buffering, 1,
            "holds the one reference frame"
        );
        assert_eq!(
            r,
            Restriction {
                max_dec_frame_buffering: 1,
                ..Restriction::default()
            }
        );

        // every bit up to the restriction flag is untouched: profile, level,
        // size, cropping and the BT.709 colour description all survive
        let (a, b) = (unescape(&original[1..]), unescape(&rewritten[1..]));
        let (mut ra, mut rb) = (BitReader::new(&a), BitReader::new(&b));
        for i in 0..before.rewrite_at {
            assert_eq!(ra.bit(), rb.bit(), "bit {i} changed");
        }
        assert_eq!(after.rewrite_at, before.rewrite_at);
        // and the codec string the controller derives (profile/flags/level)
        assert_eq!(&rewritten[1..4], &original[1..4]);
    }

    #[test]
    fn rewriting_is_idempotent() {
        let once = without_reordering(&hex(VT_SPS)).unwrap();
        assert_eq!(
            without_reordering(&once),
            None,
            "already says no reordering"
        );
    }

    #[test]
    fn an_sps_without_vui_gets_a_minimal_one() {
        // Baseline 1280x720: profile 66, level 31, poc type 2, 1 ref frame,
        // no cropping, no VUI.
        let mut w = BitWriter::default();
        for (v, n) in [(66u32, 8u32), (0xC0, 8), (31, 8)] {
            for i in (0..n).rev() {
                w.bit((v >> i) & 1 == 1);
            }
        }
        w.ue(0); // sps id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // max_num_ref_frames
        w.bit(false); // gaps
        w.ue(79); // 1280/16 - 1
        w.ue(44); // 720/16 - 1
        w.bit(true); // frame_mbs_only
        w.bit(true); // direct_8x8
        w.bit(false); // no cropping
        w.bit(false); // no VUI
        w.bit(true); // stop bit
        let mut nal = vec![0x67];
        nal.extend(escape(&w.finish()));

        let before = parsed(&nal);
        assert!(!before.vui_present);
        let rewritten = without_reordering(&nal).expect("rewritten");
        let after = parsed(&rewritten);
        assert!(after.vui_present);
        assert_eq!(after.max_num_reorder_frames, Some(0));
        assert_eq!(after.restriction.unwrap().max_dec_frame_buffering, 1);
    }

    #[test]
    fn an_existing_restriction_keeps_its_fields_but_stops_reordering() {
        // A VT SPS rewritten by hand to claim 2 reorder frames and a DPB of 3.
        let mut w = BitWriter::default();
        let base = unescape(&hex(VT_SPS)[1..]);
        let p = parse(&base).unwrap();
        let mut r = BitReader::new(&base);
        for _ in 0..p.rewrite_at {
            w.bit(r.bit().unwrap());
        }
        w.bit(true);
        w.bit(false); // mv over boundaries: no
        w.ue(2);
        w.ue(1);
        w.ue(11);
        w.ue(10);
        w.ue(2); // max_num_reorder_frames
        w.ue(3); // max_dec_frame_buffering
        w.bit(true);
        let mut nal = vec![0x27];
        nal.extend(escape(&w.finish()));

        let after = parsed(&without_reordering(&nal).unwrap());
        assert_eq!(after.max_num_reorder_frames, Some(0));
        assert_eq!(
            after.restriction.unwrap(),
            Restriction {
                mv_over_pic_boundaries: false,
                max_bytes_per_pic_denom: 2,
                max_bits_per_mb_denom: 1,
                log2_max_mv_length_horizontal: 11,
                log2_max_mv_length_vertical: 10,
                max_dec_frame_buffering: 3,
            }
        );
    }

    #[test]
    fn anything_else_is_left_alone() {
        assert_eq!(without_reordering(&[]), None);
        assert_eq!(without_reordering(&hex("68ee3c80")), None, "a PPS");
        let sps = hex(VT_SPS);
        assert_eq!(without_reordering(&sps[..8]), None, "truncated");
        let mut trailing = sps.clone();
        trailing.push(0x80); // data after the stop bit: unknown syntax
        assert_eq!(without_reordering(&trailing), None);
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let rbsp = [0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x7f];
        let escaped = escape(&rbsp);
        assert!(
            !escaped
                .windows(3)
                .any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2),
            "no start-code prefix may survive: {escaped:02x?}"
        );
        assert_eq!(unescape(&escaped), rbsp);
    }

    #[test]
    fn exp_golomb_round_trips() {
        let values = [0u32, 1, 2, 3, 7, 8, 254, 255, 65_535, u32::MAX - 1];
        let mut w = BitWriter::default();
        for v in values {
            w.ue(v);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for v in values {
            assert_eq!(r.ue(), Some(v));
        }
    }
}
