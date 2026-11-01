//! The H.264 sequence/picture headers the VA-API encoder writes itself.
//!
//! VA-API is a "stateless" encode API: the application owns the bitstream
//! headers. Intel's iHD/i965 drivers expect them as packed headers (that is what
//! `VA_ENC_PACKED_HEADER_SEQUENCE|PICTURE` means), so the SPS/PPS below must
//! describe exactly what the parameter buffers tell the driver — the values both
//! sides use come from one [`SeqHeader`].
//!
//! Stream shape (the low-latency recipe of `platform_api::VideoEncoder`):
//! progressive, 4:2:0 8-bit, one reference frame, P-only (`max_num_reorder_frames
//! = 0`, so decoders output every picture immediately), `pic_order_cnt_type = 2`
//! (display order = decode order, no POC LSBs in slice headers), 16-bit
//! `frame_num` so it wraps rarely, BT.709 limited-range VUI to match the
//! presenter's NV12 shader.
//!
//! ## References
//! * ITU-T H.264 (08/2021) §7.3.2.1.1 (SPS), §7.3.2.2 (PPS), Annex E.1.1
//!   (VUI), §7.4.1 (emulation prevention). Levels: `super::level`.

use super::level::h264_level;

/// H.264 profile this encoder can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// CAVLC, no B/interlace/FMO — decodable by everything, including OpenH264.
    ConstrainedBaseline,
    /// CABAC: ~10–15% fewer bits for the same picture; what Windows sends.
    Main,
}

impl Profile {
    pub(crate) fn profile_idc(self) -> u8 {
        match self {
            Profile::ConstrainedBaseline => 66,
            Profile::Main => 77,
        }
    }

    /// constraint_set0..5 flags + 2 reserved zero bits, MSB first. Constrained
    /// Baseline is Baseline with set0 *and* set1 (= also conforms to Main);
    /// set1 on Main is the conventional redundant "conforms to Main".
    fn constraint_byte(self) -> u8 {
        match self {
            Profile::ConstrainedBaseline => 0b1100_0000,
            Profile::Main => 0b0100_0000,
        }
    }

    pub(crate) fn cabac(self) -> bool {
        self == Profile::Main
    }
}

/// `log2_max_frame_num`. 16 bits: wraps after ~18 min at 60 fps, which the
/// decoder handles, and costs one extra byte per slice header over 4 bits.
pub(crate) const LOG2_MAX_FRAME_NUM: u32 = 16;

/// The one set of stream parameters both the packed headers and the VA-API
/// parameter buffers are derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeqHeader {
    pub profile: Profile,
    pub level_idc: u8,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl SeqHeader {
    /// `level_idc` comes from the size and rate alone (see `level`: the live
    /// bitrate moves without a new SPS).
    pub(crate) fn new(
        profile: Profile,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Self {
        let fps_num = fps_num.max(1);
        let fps_den = fps_den.max(1);
        Self {
            profile,
            level_idc: h264_level(width, height, fps_num, fps_den),
            width,
            height,
            fps_num,
            fps_den,
        }
    }

    pub(crate) fn width_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub(crate) fn height_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    pub(crate) fn macroblocks(&self) -> u32 {
        self.width_mbs() * self.height_mbs()
    }

    /// Right/bottom cropping in the SPS's units (2 luma pixels for 4:2:0
    /// progressive, §7.4.2.1.1 CropUnitX/Y). The coded size is whole macroblocks;
    /// 1080 lines are coded as 1088 and 8 are cropped.
    pub(crate) fn crop_right(&self) -> u32 {
        (self.width_mbs() * 16 - self.width) / 2
    }

    pub(crate) fn crop_bottom(&self) -> u32 {
        (self.height_mbs() * 16 - self.height) / 2
    }

    pub(crate) fn cropped(&self) -> bool {
        self.crop_right() != 0 || self.crop_bottom() != 0
    }

    /// VUI timing: one tick per field, so `time_scale = 2 × num_units × fps`.
    pub(crate) fn num_units_in_tick(&self) -> u32 {
        self.fps_den
    }

    pub(crate) fn time_scale(&self) -> u32 {
        self.fps_num.saturating_mul(2)
    }

    /// Annex-B SPS NAL (4-byte start code).
    pub(crate) fn sps_nal(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(self.profile.profile_idc() as u64, 8);
        w.bits(self.profile.constraint_byte() as u64, 8);
        w.bits(self.level_idc as u64, 8);
        w.ue(0); // seq_parameter_set_id
        w.ue(LOG2_MAX_FRAME_NUM - 4);
        w.ue(2); // pic_order_cnt_type: POC follows frame_num, no reordering
        w.ue(1); // max_num_ref_frames
        w.flag(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(self.width_mbs() - 1);
        w.ue(self.height_mbs() - 1); // pic_height_in_map_units_minus1 (frame_mbs_only)
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag (required from level 3 on)
        w.flag(self.cropped());
        if self.cropped() {
            w.ue(0);
            w.ue(self.crop_right());
            w.ue(0);
            w.ue(self.crop_bottom());
        }
        w.flag(true); // vui_parameters_present_flag
        self.vui(&mut w);
        nal(3, 7, &w.finish())
    }

    fn vui(&self, w: &mut BitWriter) {
        w.flag(false); // aspect_ratio_info_present_flag (square pixels)
        w.flag(false); // overscan_info_present_flag
        w.flag(true); // video_signal_type_present_flag
        w.bits(5, 3); // video_format: unspecified
        w.flag(false); // video_full_range_flag: limited (16–235)
        w.flag(true); // colour_description_present_flag
        w.bits(1, 8); // colour_primaries: BT.709
        w.bits(1, 8); // transfer_characteristics: BT.709
        w.bits(1, 8); // matrix_coefficients: BT.709
        w.flag(false); // chroma_loc_info_present_flag
        w.flag(true); // timing_info_present_flag
        w.bits(self.num_units_in_tick() as u64, 32);
        w.bits(self.time_scale() as u64, 32);
        // Not fixed: the capture loop skips frames on an idle screen.
        w.flag(false); // fixed_frame_rate_flag
        w.flag(false); // nal_hrd_parameters_present_flag
        w.flag(false); // vcl_hrd_parameters_present_flag
        w.flag(false); // pic_struct_present_flag
        w.flag(true); // bitstream_restriction_flag
        w.flag(true); // motion_vectors_over_pic_boundaries_flag
        w.ue(0); // max_bytes_per_pic_denom (no limit)
        w.ue(0); // max_bits_per_mb_denom (no limit)
        w.ue(15); // log2_max_mv_length_horizontal (the pre-2016 maximum)
        w.ue(15); // log2_max_mv_length_vertical
                  // WHY these two matter: without them a decoder must assume the level's
                  // full DPB and may hold several frames before output — added latency
                  // for a stream that never reorders.
        w.ue(0); // max_num_reorder_frames
        w.ue(1); // max_dec_frame_buffering
    }

    /// Annex-B PPS NAL (4-byte start code). Mirrors the picture parameter
    /// buffer: PPS 0 → SPS 0, `pic_init_qp` 26, deblocking control present,
    /// one active reference.
    pub(crate) fn pps_nal(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.flag(self.profile.cabac()); // entropy_coding_mode_flag
        w.flag(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.flag(false); // weighted_pred_flag
        w.bits(0, 2); // weighted_bipred_idc
        w.se(0); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.flag(true); // deblocking_filter_control_present_flag
        w.flag(false); // constrained_intra_pred_flag
        w.flag(false); // redundant_pic_cnt_present_flag
        nal(3, 8, &w.finish())
    }
}

/// Big-endian RBSP bit writer.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    acc: u8,
    used: u8,
}

impl BitWriter {
    pub(crate) fn flag(&mut self, b: bool) {
        self.acc = (self.acc << 1) | b as u8;
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.acc);
            self.acc = 0;
            self.used = 0;
        }
    }

    /// The low `n` bits of `value`, MSB first.
    pub(crate) fn bits(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            self.flag((value >> i) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb `ue(v)` (§9.1).
    pub(crate) fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        self.bits(x, len);
    }

    /// Signed Exp-Golomb `se(v)` (§9.1.1): 0, 1, −1, 2, −2 … → 0, 1, 2, 3, 4 …
    pub(crate) fn se(&mut self, v: i32) {
        let v = v as i64;
        let k = if v > 0 { 2 * v - 1 } else { -2 * v };
        // |i32::MIN|·2 does not fit u32; nothing here writes values that large.
        self.ue(k.min(u32::MAX as i64) as u32);
    }

    /// `rbsp_trailing_bits()`: a stop bit, then zero-pad to a byte boundary.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.flag(true);
        while self.used != 0 {
            self.flag(false);
        }
        self.bytes
    }
}

/// An Annex-B NAL unit: 4-byte start code, header, and the RBSP with emulation
/// prevention (`00 00 0x` with x ≤ 3 → `00 00 03 0x`, §7.4.1).
pub(crate) fn nal(nal_ref_idc: u8, nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 5);
    out.extend_from_slice(&[0, 0, 0, 1, (nal_ref_idc << 5) | nal_unit_type]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal RBSP reader to check what the writer produced.
    pub(crate) struct BitReader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> BitReader<'a> {
        pub(crate) fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }
        pub(crate) fn bit(&mut self) -> u32 {
            let b = (self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            b as u32
        }
        pub(crate) fn bits(&mut self, n: u32) -> u32 {
            (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
        }
        pub(crate) fn ue(&mut self) -> u32 {
            let mut zeros = 0;
            while self.bit() == 0 {
                zeros += 1;
            }
            ((1u64 << zeros) - 1 + self.bits(zeros) as u64) as u32
        }
        pub(crate) fn se(&mut self) -> i32 {
            let k = self.ue() as i64;
            if k % 2 == 1 {
                ((k + 1) / 2) as i32
            } else {
                -(k / 2) as i32
            }
        }
    }

    /// Strip the start code + header and undo emulation prevention.
    pub(crate) fn rbsp_of(nal: &[u8]) -> (u8, Vec<u8>) {
        let body = nal.strip_prefix(&[0, 0, 0, 1]).unwrap_or(nal);
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in &body[1..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        (body[0], out)
    }

    #[test]
    fn exp_golomb_matches_the_spec_table() {
        // §9.1 Table 9-2: 0→1, 1→010, 2→011, 3→00100, 7→0001000.
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 3, 7] {
            w.ue(v);
        }
        // 1 010 011 00100 0001000, stop bit, pad: 1010_0110 0100_0001 0001_0000.
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for v in [0, 1, 2, 3, 7] {
            assert_eq!(r.ue(), v);
        }
        assert_eq!(bytes, vec![0xa6, 0x41, 0x10]);

        let mut w = BitWriter::default();
        for v in [0, 1, -1, 2, -2, 26, -26] {
            w.se(v);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for v in [0, 1, -1, 2, -2, 26, -26] {
            assert_eq!(r.se(), v);
        }
    }

    #[test]
    fn rbsp_trailing_bits_pad_to_a_byte() {
        let mut w = BitWriter::default();
        w.bits(0b101, 3);
        assert_eq!(w.finish(), vec![0b1011_0000]);
        let mut w = BitWriter::default();
        w.bits(0xff, 8);
        assert_eq!(w.finish(), vec![0xff, 0x80]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(
            nal(3, 7, &[0, 0, 1, 0, 0, 0, 0, 0, 3, 0xff, 0, 0]),
            vec![0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 3, 0xff, 0, 0]
        );
        // Round-trips through the reader-side removal.
        let rbsp = [0u8, 0, 2, 0, 0, 0, 7];
        let (header, back) = rbsp_of(&nal(0, 1, &rbsp));
        assert_eq!(header, 0x01);
        assert_eq!(back, rbsp);
    }

    #[test]
    fn sps_round_trips_for_1080p() {
        let h = SeqHeader::new(Profile::Main, 1920, 1080, 60, 1);
        assert_eq!(h.level_idc, 42);
        assert_eq!((h.width_mbs(), h.height_mbs()), (120, 68));
        assert_eq!((h.crop_right(), h.crop_bottom()), (0, 4));
        let (header, rbsp) = rbsp_of(&h.sps_nal());
        assert_eq!(header, 0x67);
        let mut r = BitReader::new(&rbsp);
        assert_eq!(r.bits(8), 77);
        assert_eq!(r.bits(8), 0b0100_0000);
        assert_eq!(r.bits(8), 42);
        assert_eq!(r.ue(), 0); // sps id
        assert_eq!(r.ue(), LOG2_MAX_FRAME_NUM - 4);
        assert_eq!(r.ue(), 2); // poc type
        assert_eq!(r.ue(), 1); // refs
        assert_eq!(r.bit(), 0); // gaps
        assert_eq!(r.ue(), 119);
        assert_eq!(r.ue(), 67);
        assert_eq!(r.bit(), 1); // frame_mbs_only
        assert_eq!(r.bit(), 1); // direct_8x8
        assert_eq!(r.bit(), 1); // cropping
        assert_eq!([r.ue(), r.ue(), r.ue(), r.ue()], [0, 0, 0, 4]);
        assert_eq!(r.bit(), 1); // vui
        assert_eq!(r.bit(), 0); // aspect
        assert_eq!(r.bit(), 0); // overscan
        assert_eq!(r.bit(), 1); // signal type
        assert_eq!(r.bits(3), 5);
        assert_eq!(r.bit(), 0); // limited range
        assert_eq!(r.bit(), 1);
        assert_eq!([r.bits(8), r.bits(8), r.bits(8)], [1, 1, 1]);
        assert_eq!(r.bit(), 0); // chroma loc
        assert_eq!(r.bit(), 1); // timing
        assert_eq!(r.bits(32), 1);
        assert_eq!(r.bits(32), 120);
        assert_eq!(r.bit(), 0); // fixed rate
        assert_eq!([r.bit(), r.bit(), r.bit()], [0, 0, 0]); // hrd, hrd, pic_struct
        assert_eq!(r.bit(), 1); // restriction
        assert_eq!(r.bit(), 1);
        assert_eq!([r.ue(), r.ue(), r.ue(), r.ue()], [0, 0, 15, 15]);
        assert_eq!(r.ue(), 0); // reorder
        assert_eq!(r.ue(), 1); // dpb
        assert_eq!(r.bit(), 1); // stop bit
    }

    /// Golden bytes, cross-checked with an independent parser: FFmpeg 6.1's
    /// `trace_headers` bitstream filter reads them back as exactly the fields
    /// `sps_round_trips_for_1080p` asserts (and no errors).
    #[test]
    fn main_1080p60_headers_are_byte_exact() {
        let h = SeqHeader::new(Profile::Main, 1920, 1080, 60, 1);
        assert_eq!(
            h.sps_nal(),
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x4d, 0x40, 0x2a, 0x8d, 0x68, 0x07, 0x80, 0x22, 0x7e,
                0x59, 0xa8, 0x08, 0x08, 0x0a, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00,
                0xf0, 0x1e, 0x10, 0x08, 0x54,
            ]
        );
        assert_eq!(
            h.pps_nal(),
            [0x00, 0x00, 0x00, 0x01, 0x68, 0xee, 0x3c, 0x80]
        );
    }

    #[test]
    fn uncropped_baseline_sps_and_pps() {
        let h = SeqHeader::new(Profile::ConstrainedBaseline, 1280, 720, 30, 1);
        assert!(!h.cropped());
        let (_, rbsp) = rbsp_of(&h.sps_nal());
        // Level 4.1: the lowest whose MaxBR holds the 50 Mbit/s ceiling.
        assert_eq!(&rbsp[..3], &[66, 0b1100_0000, 41]);

        let (header, rbsp) = rbsp_of(&h.pps_nal());
        assert_eq!(header, 0x68);
        let mut r = BitReader::new(&rbsp);
        assert_eq!([r.ue(), r.ue()], [0, 0]);
        assert_eq!(r.bit(), 0); // CAVLC for baseline
        assert_eq!(r.bit(), 0);
        assert_eq!([r.ue(), r.ue(), r.ue()], [0, 0, 0]);
        assert_eq!(r.bit(), 0);
        assert_eq!(r.bits(2), 0);
        assert_eq!([r.se(), r.se(), r.se()], [0, 0, 0]);
        assert_eq!([r.bit(), r.bit(), r.bit()], [1, 0, 0]);
        assert_eq!(r.bit(), 1); // stop bit

        let main = SeqHeader::new(Profile::Main, 1280, 720, 30, 1);
        let (_, rbsp) = rbsp_of(&main.pps_nal());
        // ue(0)=1, ue(0)=1, then entropy_coding_mode_flag=1.
        assert_eq!(rbsp[0] >> 5, 0b111);
    }
}
