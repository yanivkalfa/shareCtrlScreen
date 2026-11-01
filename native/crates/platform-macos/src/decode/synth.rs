//! Test bitstreams written bit by bit, so the decoder is tested on real
//! VideoToolbox without an encoder (M2 lands separately) and with exactly
//! known pixels.
//!
//! [`H264Synth`] writes Baseline H.264 whose keyframes are all `I_PCM`
//! macroblocks — raw samples, so the decoded picture must equal the source
//! bit for bit — and whose deltas are P frames with every macroblock
//! skipped (a copy of the reference). Syntax: ITU-T H.264 §7.3.2.1.1 (SPS),
//! §7.3.2.2 (PPS), §7.3.3 (slice header), §7.3.4 (slice data), §7.3.5
//! (`mb_type` 25 = I_PCM in an I slice), §7.4.1 (emulation prevention).

/// MSB-first bit writer with the H.264 Exp-Golomb codes.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    /// `u(n)`.
    pub(crate) fn put(&mut self, n: u32, value: u64) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            if let Some(last) = self.bytes.last_mut() {
                *last |= bit << (7 - self.bits % 8);
            }
            self.bits += 1;
        }
    }

    /// `ue(v)`.
    pub(crate) fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(len - 1, 0);
        self.put(len, x);
    }

    /// `se(v)`.
    pub(crate) fn se(&mut self, v: i32) {
        let mapped = if v > 0 {
            2 * v.unsigned_abs() - 1
        } else {
            2 * v.unsigned_abs()
        };
        self.ue(mapped);
    }

    /// Zero bits up to the next byte boundary.
    pub(crate) fn align(&mut self) {
        while !self.bits.is_multiple_of(8) {
            self.put(1, 0);
        }
    }

    /// `rbsp_trailing_bits()`: a one bit, then zeros to the byte boundary.
    pub(crate) fn trailing(&mut self) {
        self.put(1, 1);
        self.align();
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) {
        for &b in data {
            self.put(8, u64::from(b));
        }
    }

    /// The bytes written; a partial last byte is zero-padded.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Annex-B NAL unit: 4-byte start code, header, RBSP with emulation
/// prevention (`00 00 0x` with x ≤ 3 gets a `03` inserted).
fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, header];
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

/// A sample value in 16..236: never zero, so PCM data never needs
/// emulation prevention, and different at every nearby position.
pub(crate) fn pattern(x: u32, y: u32, seed: u32) -> u8 {
    16 + ((x * 3 + y * 5 + seed) % 220) as u8
}

/// Writes one H.264 stream of a fixed picture size.
pub(crate) struct H264Synth {
    height: u32,
    mbs_w: u32,
    mbs_h: u32,
    seed: u32,
    frame_num: u32,
    idr_id: u32,
}

impl H264Synth {
    /// `width` a multiple of 16, `height` even; a height that is not a
    /// multiple of 16 is coded rounded up and cropped (as 1080p is coded as
    /// 1088 rows).
    pub(crate) fn new(width: u32, height: u32, seed: u32) -> Self {
        assert!(
            width.is_multiple_of(16) && height.is_multiple_of(2),
            "{width}x{height}"
        );
        Self {
            height,
            mbs_w: width / 16,
            mbs_h: height.div_ceil(16),
            seed,
            frame_num: 0,
            idr_id: 0,
        }
    }

    /// The source luma at (x, y).
    pub(crate) fn luma(&self, x: u32, y: u32) -> u8 {
        pattern(x, y, self.seed)
    }

    /// The source (Cb, Cr) at chroma position (x, y).
    pub(crate) fn chroma(&self, x: u32, y: u32) -> (u8, u8) {
        (
            pattern(2 * x + 1, 3 * y, self.seed + 40),
            pattern(x, 2 * y + 7, self.seed + 90),
        )
    }

    fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(8, 66); // profile_idc: Baseline
        w.put(8, 0); // constraint flags
        w.put(8, 51); // level_idc 5.1: room for 1080p I_PCM frames
        w.ue(0); // seq_parameter_set_id
        w.ue(0); // log2_max_frame_num_minus4 → 4-bit frame_num
        w.ue(2); // pic_order_cnt_type 2: output order = decode order
        w.ue(1); // max_num_ref_frames
        w.put(1, 0); // gaps_in_frame_num_value_allowed_flag
        w.ue(self.mbs_w - 1);
        w.ue(self.mbs_h - 1);
        w.put(1, 1); // frame_mbs_only_flag
        w.put(1, 1); // direct_8x8_inference_flag
        let crop_rows = self.mbs_h * 16 - self.height;
        if crop_rows > 0 {
            w.put(1, 1); // frame_cropping_flag
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.ue(crop_rows / 2); // in units of 2 rows (4:2:0, frames)
        } else {
            w.put(1, 0);
        }
        w.put(1, 0); // vui_parameters_present_flag
        w.trailing();
        nal(0x67, &w.finish())
    }

    fn pps() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.put(1, 0); // entropy_coding_mode_flag: CAVLC
        w.put(1, 0); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.put(1, 0); // weighted_pred_flag
        w.put(2, 0); // weighted_bipred_idc
        w.se(0); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.put(1, 1); // deblocking_filter_control_present_flag
        w.put(1, 0); // constrained_intra_pred_flag
        w.put(1, 0); // redundant_pic_cnt_present_flag
        w.trailing();
        nal(0x68, &w.finish())
    }

    /// SPS + PPS + an IDR of I_PCM macroblocks, as one Annex-B access unit.
    pub(crate) fn keyframe(&mut self) -> Vec<u8> {
        self.frame_num = 0;
        let mut w = BitWriter::default();
        w.ue(0); // first_mb_in_slice
        w.ue(7); // slice_type: I (all slices)
        w.ue(0); // pic_parameter_set_id
        w.put(4, 0); // frame_num
        w.ue(self.idr_id); // idr_pic_id: differs between consecutive IDRs
        w.put(1, 0); // no_output_of_prior_pics_flag
        w.put(1, 0); // long_term_reference_flag
        w.se(0); // slice_qp_delta
        w.ue(1); // disable_deblocking_filter_idc: off, so PCM stays exact
        for mby in 0..self.mbs_h {
            for mbx in 0..self.mbs_w {
                w.ue(25); // mb_type: I_PCM
                w.align(); // pcm_alignment_zero_bit
                for y in 0..16 {
                    for x in 0..16 {
                        w.bytes(&[self.luma(mbx * 16 + x, mby * 16 + y)]);
                    }
                }
                for plane in 0..2 {
                    for y in 0..8 {
                        for x in 0..8 {
                            let (cb, cr) = self.chroma(mbx * 8 + x, mby * 8 + y);
                            w.bytes(&[if plane == 0 { cb } else { cr }]);
                        }
                    }
                }
            }
        }
        w.trailing();
        self.idr_id ^= 1;
        [self.sps(), Self::pps(), nal(0x65, &w.finish())].concat()
    }

    /// A P frame with every macroblock skipped: the previous picture again.
    pub(crate) fn repeat(&mut self) -> Vec<u8> {
        self.frame_num = (self.frame_num + 1) % 16;
        let mut w = BitWriter::default();
        w.ue(0); // first_mb_in_slice
        w.ue(5); // slice_type: P (all slices)
        w.ue(0); // pic_parameter_set_id
        w.put(4, u64::from(self.frame_num));
        w.put(1, 0); // num_ref_idx_active_override_flag
        w.put(1, 0); // ref_pic_list_modification_flag_l0
        w.put(1, 0); // adaptive_ref_pic_marking_mode_flag: sliding window
        w.se(0); // slice_qp_delta
        w.ue(1); // disable_deblocking_filter_idc
        w.ue(self.mbs_w * self.mbs_h); // mb_skip_run: the whole picture
        w.trailing();
        nal(0x41, &w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::annexb::{nal_units, role, NalRole};
    use platform_api::Codec;

    #[test]
    fn exp_golomb_codes() {
        let mut w = BitWriter::default();
        w.ue(0); // 1
        w.ue(1); // 010
        w.ue(4); // 00101
        w.se(-1); // ue(2) = 011
        w.se(2); // ue(3) = 00100
        w.trailing();
        // 1010 0010 1011 0010 0|1 → 0xa2 0xb2 0x40
        assert_eq!(w.finish(), vec![0xa2, 0xb2, 0x40]);
    }

    #[test]
    fn emulation_prevention_is_inserted() {
        assert_eq!(
            nal(0x65, &[0, 0, 1, 0, 0, 0, 5]),
            vec![0, 0, 0, 1, 0x65, 0, 0, 3, 1, 0, 0, 3, 0, 5]
        );
    }

    #[test]
    fn keyframes_carry_parameter_sets_and_an_idr() {
        let mut s = H264Synth::new(32, 18, 0);
        let au = s.keyframe();
        let roles: Vec<_> = nal_units(&au)
            .filter_map(|n| role(Codec::H264, n))
            .collect();
        assert_eq!(
            roles,
            vec![
                NalRole::ParameterSet(0),
                NalRole::ParameterSet(1),
                NalRole::RandomAccess
            ]
        );
        let roles: Vec<_> = nal_units(&s.repeat())
            .filter_map(|n| role(Codec::H264, n))
            .collect();
        assert_eq!(roles, vec![NalRole::Other]);
    }
}
