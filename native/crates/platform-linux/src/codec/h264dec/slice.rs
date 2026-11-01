//! Slice headers (§7.3.3) — every field a stateless decoder must hand the
//! driver, plus the reference-list and marking commands the decoder itself
//! executes (§7.3.3.1 – §7.3.3.3).

use super::bits::BitReader;
use super::params::{PocType, Pps, Sps};
use super::Bad;

/// `slice_type % 5` (Table 7-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_raw(v: u32) -> Self {
        match v % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        }
    }

    /// The `slice_type` value VA-API expects (0–4, FFmpeg's convention).
    pub(crate) fn raw(self) -> u8 {
        match self {
            SliceType::P => 0,
            SliceType::B => 1,
            SliceType::I => 2,
            SliceType::Sp => 3,
            SliceType::Si => 4,
        }
    }

    /// Slices that predict from list 0.
    pub(crate) fn uses_list0(self) -> bool {
        matches!(self, SliceType::P | SliceType::Sp | SliceType::B)
    }
}

/// One `modification_of_pic_nums_idc` command (§7.4.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modification {
    /// idc 0: `abs_diff_pic_num_minus1`, subtracted from the prediction.
    ShortTermSub(u32),
    /// idc 1: added.
    ShortTermAdd(u32),
    /// idc 2: `long_term_pic_num`.
    LongTerm(u32),
}

/// One `memory_management_control_operation` (§7.4.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mmco {
    /// 1: short-term picture `CurrPicNum − (difference + 1)` → unused.
    ForgetShortTerm { difference_minus1: u32 },
    /// 2: long-term picture with this `LongTermPicNum` → unused.
    ForgetLongTerm { long_term_pic_num: u32 },
    /// 3: short-term picture → long-term with `LongTermFrameIdx`.
    ShortToLongTerm {
        difference_minus1: u32,
        long_term_frame_idx: u32,
    },
    /// 4: `MaxLongTermFrameIdx = max_long_term_frame_idx_plus1 − 1`.
    MaxLongTermIdx { plus1: u32 },
    /// 5: every reference → unused; the picture restarts frame_num/POC.
    ForgetAll,
    /// 6: the current picture becomes long-term with this index.
    CurrentToLongTerm { long_term_frame_idx: u32 },
}

/// `dec_ref_pic_marking()` of a reference picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marking {
    Idr { long_term_reference: bool },
    SlidingWindow,
    Adaptive(Vec<Mmco>),
}

/// Explicit weights of one reference (`None` = the inferred default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RefWeights {
    pub luma: Option<(i32, i32)>,
    pub chroma: Option<[(i32, i32); 2]>,
}

/// `pred_weight_table()` (§7.3.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PredWeights {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    pub l0: Vec<RefWeights>,
    pub l1: Vec<RefWeights>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SliceHeader {
    pub nal_ref_idc: u8,
    pub idr: bool,
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pps_id: usize,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_l0_active: u32,
    pub num_ref_idx_l1_active: u32,
    pub modifications_l0: Vec<Modification>,
    pub modifications_l1: Vec<Modification>,
    pub pred_weights: Option<PredWeights>,
    /// `None` for non-reference pictures.
    pub marking: Option<Marking>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    /// Bits from the start of the NAL unit (header byte included) to the
    /// first bit of `slice_data()`, in the unescaped RBSP — VA-API's
    /// `slice_data_bit_offset`.
    pub header_bits: usize,
}

/// Upper bound on list-modification / MMCO commands: one per list entry (+
/// the terminator) is the most a conforming stream sends; the bound keeps a
/// hostile header from looping until the bits run out.
const MAX_COMMANDS: usize = 66;

/// `pic_parameter_set_id` of a slice, read without the parameter sets (they
/// are needed for the rest). `rbsp` includes the NAL header byte.
pub(crate) fn pps_id_of(rbsp: &[u8]) -> Result<usize, Bad> {
    let mut r = BitReader::new(rbsp);
    r.bits(8)?;
    r.ue()?;
    r.ue()?;
    Ok(r.ue_max(255, "pic_parameter_set_id")? as usize)
}

impl SliceHeader {
    /// Parse the header of a slice NAL. `rbsp` is the whole unescaped NAL
    /// unit, header byte included; `sps`/`pps` are the sets it names.
    pub(crate) fn parse(rbsp: &[u8], sps: &Sps, pps: &Pps) -> Result<Self, Bad> {
        let mut r = BitReader::new(rbsp);
        let header = r.bits(8)?;
        let nal_ref_idc = ((header >> 5) & 3) as u8;
        let nal_unit_type = header & 0x1f;
        let idr = nal_unit_type == 5;
        // A frame has at most 1024×1024/… macroblocks; anything past the
        // picture is corrupt.
        let total_mbs = sps.width_mbs * sps.height_mbs();
        let first_mb_in_slice = r.ue()?;
        if first_mb_in_slice >= total_mbs {
            return Err(Bad::Malformed(format!(
                "first_mb_in_slice {first_mb_in_slice} past the {total_mbs}-macroblock picture"
            )));
        }
        let slice_type = SliceType::from_raw(r.ue_max(9, "slice_type")?);
        if idr && slice_type != SliceType::I && slice_type != SliceType::Si {
            return Err(Bad::Malformed(format!("IDR slice of type {slice_type:?}")));
        }
        let pps_id = r.ue_max(255, "pic_parameter_set_id")? as usize;
        if sps.separate_colour_plane {
            r.bits(2)?; // colour_plane_id
        }
        let frame_num = r.bits(sps.log2_max_frame_num)?;
        if idr && frame_num != 0 {
            return Err(Bad::Malformed(format!("IDR with frame_num {frame_num}")));
        }
        if !sps.frame_mbs_only && r.flag()? {
            // field_pic_flag: the caller refuses interlaced SPSs before this.
            return Err(Bad::Unsupported("field pictures (interlaced video)".into()));
        }
        let idr_pic_id = if idr {
            r.ue_max(65535, "idr_pic_id")?
        } else {
            0
        };
        let mut pic_order_cnt_lsb = 0;
        let mut delta_pic_order_cnt_bottom = 0;
        let mut delta_pic_order_cnt = [0; 2];
        match &sps.poc {
            PocType::Lsb { log2_max_lsb } => {
                pic_order_cnt_lsb = r.bits(*log2_max_lsb)?;
                if pps.bottom_field_pic_order_in_frame_present {
                    delta_pic_order_cnt_bottom = r.se()?;
                }
            }
            PocType::Cycle {
                delta_pic_order_always_zero: false,
                ..
            } => {
                delta_pic_order_cnt[0] = r.se()?;
                if pps.bottom_field_pic_order_in_frame_present {
                    delta_pic_order_cnt[1] = r.se()?;
                }
            }
            PocType::Cycle { .. } | PocType::FrameNum => {}
        }
        let redundant_pic_cnt = if pps.redundant_pic_cnt_present {
            r.ue_max(127, "redundant_pic_cnt")?
        } else {
            0
        };
        let direct_spatial_mv_pred = slice_type == SliceType::B && r.flag()?;
        let mut num_ref_idx_l0_active = pps.num_ref_idx_l0_default_active;
        let mut num_ref_idx_l1_active = pps.num_ref_idx_l1_default_active;
        if slice_type.uses_list0() && r.flag()? {
            num_ref_idx_l0_active = r.ue_max(31, "num_ref_idx_l0_active_minus1")? + 1;
            if slice_type == SliceType::B {
                num_ref_idx_l1_active = r.ue_max(31, "num_ref_idx_l1_active_minus1")? + 1;
            }
        }
        if !slice_type.uses_list0() {
            num_ref_idx_l0_active = 0;
        }
        if slice_type != SliceType::B {
            num_ref_idx_l1_active = 0;
        }
        let modifications_l0 = if slice_type.uses_list0() {
            modifications(&mut r)?
        } else {
            Vec::new()
        };
        let modifications_l1 = if slice_type == SliceType::B {
            modifications(&mut r)?
        } else {
            Vec::new()
        };
        let weighted = (pps.weighted_pred && matches!(slice_type, SliceType::P | SliceType::Sp))
            || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B);
        let pred_weights = if weighted {
            Some(pred_weight_table(
                &mut r,
                sps.chroma_array_type() != 0,
                num_ref_idx_l0_active,
                num_ref_idx_l1_active,
            )?)
        } else {
            None
        };
        let marking = if nal_ref_idc != 0 {
            Some(dec_ref_pic_marking(&mut r, idr)?)
        } else {
            None
        };
        let cabac_init_idc =
            if pps.entropy_coding_mode && !matches!(slice_type, SliceType::I | SliceType::Si) {
                r.ue_max(2, "cabac_init_idc")?
            } else {
                0
            };
        let qp_bd_offset = 6 * sps.bit_depth_luma_minus8 as i32;
        let slice_qp_delta = r.se()?;
        let qp = 26 + pps.pic_init_qp_minus26 + slice_qp_delta;
        if !(-qp_bd_offset..=51).contains(&qp) {
            return Err(Bad::Malformed(format!("slice QP {qp} out of range")));
        }
        if matches!(slice_type, SliceType::Sp | SliceType::Si) {
            if slice_type == SliceType::Sp {
                r.flag()?; // sp_for_switch_flag
            }
            r.se()?; // slice_qs_delta
        }
        let mut disable_deblocking_filter_idc = 0;
        let mut slice_alpha_c0_offset_div2 = 0;
        let mut slice_beta_offset_div2 = 0;
        if pps.deblocking_filter_control_present {
            disable_deblocking_filter_idc = r.ue_max(2, "disable_deblocking_filter_idc")?;
            if disable_deblocking_filter_idc != 1 {
                slice_alpha_c0_offset_div2 = r.se_range(-6, 6, "slice_alpha_c0_offset_div2")?;
                slice_beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")?;
            }
        }
        Ok(Self {
            nal_ref_idc,
            idr,
            first_mb_in_slice,
            slice_type,
            pps_id,
            frame_num,
            idr_pic_id,
            pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom,
            delta_pic_order_cnt,
            redundant_pic_cnt,
            direct_spatial_mv_pred,
            num_ref_idx_l0_active,
            num_ref_idx_l1_active,
            modifications_l0,
            modifications_l1,
            pred_weights,
            marking,
            cabac_init_idc,
            slice_qp_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
            header_bits: r.position(),
        })
    }

    /// Whether `other` is a slice of the same primary picture (the §7.4.1.2.4
    /// first-slice-of-a-new-picture tests that apply to frames).
    pub(crate) fn same_picture(&self, other: &SliceHeader) -> bool {
        self.frame_num == other.frame_num
            && self.pps_id == other.pps_id
            && (self.nal_ref_idc == 0) == (other.nal_ref_idc == 0)
            && self.idr == other.idr
            && self.idr_pic_id == other.idr_pic_id
            && self.pic_order_cnt_lsb == other.pic_order_cnt_lsb
            && self.delta_pic_order_cnt_bottom == other.delta_pic_order_cnt_bottom
            && self.delta_pic_order_cnt == other.delta_pic_order_cnt
    }
}

/// `ref_pic_list_modification()` for one list (§7.3.3.1).
fn modifications(r: &mut BitReader<'_>) -> Result<Vec<Modification>, Bad> {
    let mut out = Vec::new();
    if !r.flag()? {
        return Ok(out);
    }
    loop {
        if out.len() == MAX_COMMANDS {
            return Err(Bad::Malformed(
                "unterminated ref_pic_list_modification".into(),
            ));
        }
        out.push(match r.ue_max(3, "modification_of_pic_nums_idc")? {
            0 => Modification::ShortTermSub(r.ue()?),
            1 => Modification::ShortTermAdd(r.ue()?),
            2 => Modification::LongTerm(r.ue()?),
            _ => return Ok(out),
        });
    }
}

/// `pred_weight_table()` (§7.3.3.2).
fn pred_weight_table(
    r: &mut BitReader<'_>,
    chroma: bool,
    l0: u32,
    l1: u32,
) -> Result<PredWeights, Bad> {
    let luma_log2_denom = r.ue_max(7, "luma_log2_weight_denom")?;
    let chroma_log2_denom = if chroma {
        r.ue_max(7, "chroma_log2_weight_denom")?
    } else {
        0
    };
    let mut list = |n: u32| -> Result<Vec<RefWeights>, Bad> {
        (0..n)
            .map(|_| {
                let mut w = RefWeights::default();
                if r.flag()? {
                    w.luma = Some((
                        r.se_range(-128, 127, "luma_weight")?,
                        r.se_range(-128, 127, "luma_offset")?,
                    ));
                }
                if chroma && r.flag()? {
                    let mut c = [(0, 0); 2];
                    for pair in &mut c {
                        *pair = (
                            r.se_range(-128, 127, "chroma_weight")?,
                            r.se_range(-128, 127, "chroma_offset")?,
                        );
                    }
                    w.chroma = Some(c);
                }
                Ok(w)
            })
            .collect()
    };
    let l0 = list(l0)?;
    let l1 = list(l1)?;
    Ok(PredWeights {
        luma_log2_denom,
        chroma_log2_denom,
        l0,
        l1,
    })
}

/// `dec_ref_pic_marking()` (§7.3.3.3).
fn dec_ref_pic_marking(r: &mut BitReader<'_>, idr: bool) -> Result<Marking, Bad> {
    if idr {
        r.flag()?; // no_output_of_prior_pics_flag: nothing is held for output
        return Ok(Marking::Idr {
            long_term_reference: r.flag()?,
        });
    }
    if !r.flag()? {
        return Ok(Marking::SlidingWindow);
    }
    let mut ops = Vec::new();
    loop {
        if ops.len() == MAX_COMMANDS {
            return Err(Bad::Malformed("unterminated MMCO list".into()));
        }
        ops.push(match r.ue_max(6, "memory_management_control_operation")? {
            0 => return Ok(Marking::Adaptive(ops)),
            1 => Mmco::ForgetShortTerm {
                difference_minus1: r.ue()?,
            },
            2 => Mmco::ForgetLongTerm {
                long_term_pic_num: r.ue()?,
            },
            3 => Mmco::ShortToLongTerm {
                difference_minus1: r.ue()?,
                long_term_frame_idx: r.ue_max(15, "long_term_frame_idx")?,
            },
            4 => Mmco::MaxLongTermIdx {
                plus1: r.ue_max(16, "max_long_term_frame_idx_plus1")?,
            },
            5 => Mmco::ForgetAll,
            _ => Mmco::CurrentToLongTerm {
                long_term_frame_idx: r.ue_max(15, "long_term_frame_idx")?,
            },
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::h264::{BitWriter, Profile, SeqHeader};
    use crate::codec::h264dec::bits::unescape;
    use crate::codec::h264dec::params::tests::rbsp;

    pub(crate) fn main_sets() -> (Sps, Pps) {
        let h = SeqHeader::new(Profile::Main, 1280, 720, 30, 1);
        let sps = Sps::parse(&rbsp(&h.sps_nal())).expect("SPS");
        let pps = Pps::parse(&rbsp(&h.pps_nal()), |_| Some(&sps)).expect("PPS");
        (sps, pps)
    }

    /// The slice-header writer every test below starts from: a NAL header
    /// byte, then the fields for the VA-API encoder's own SPS/PPS
    /// (`log2_max_frame_num` 16, POC type 2, CABAC, deblocking control).
    pub(crate) fn header(nal_ref_idc: u8, nal_type: u8, body: impl Fn(&mut BitWriter)) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(((nal_ref_idc << 5) | nal_type) as u64, 8);
        body(&mut w);
        w.finish()
    }

    #[test]
    fn idr_slice() {
        let (sps, pps) = main_sets();
        let rbsp = header(3, 5, |w| {
            w.ue(0); // first_mb
            w.ue(7); // I (all slices)
            w.ue(0); // pps
            w.bits(0, 16); // frame_num
            w.ue(5); // idr_pic_id
            w.flag(false); // no_output_of_prior_pics
            w.flag(true); // long_term_reference_flag
            w.se(-3); // slice_qp_delta
            w.ue(0); // deblocking on
            w.se(1);
            w.se(-1);
        });
        let h = SliceHeader::parse(&rbsp, &sps, &pps).expect("slice");
        assert!(h.idr);
        assert_eq!(h.slice_type, SliceType::I);
        assert_eq!(h.idr_pic_id, 5);
        assert_eq!(
            h.marking,
            Some(Marking::Idr {
                long_term_reference: true
            })
        );
        assert_eq!(h.num_ref_idx_l0_active, 0);
        assert_eq!(h.slice_qp_delta, -3);
        assert_eq!(
            (h.slice_alpha_c0_offset_div2, h.slice_beta_offset_div2),
            (1, -1)
        );
        // 8 header + ue(0) 1 + ue(7) 7 + ue(0) 1 + 16 + ue(5) 5 + 2 flags +
        // se(-3) 5 + ue(0) 1 + se(1) 3 + se(-1) 3 = 52.
        assert_eq!(h.header_bits, 52);
        assert_eq!(pps_id_of(&rbsp).expect("pps id"), 0);
    }

    #[test]
    fn p_slice_with_modifications_and_mmcos() {
        let (sps, pps) = main_sets();
        let rbsp = header(2, 1, |w| {
            w.ue(0);
            w.ue(5); // P
            w.ue(0);
            w.bits(9, 16);
            w.flag(true); // override
            w.ue(2); // 3 active
            w.flag(true); // modification l0
            w.ue(0);
            w.ue(1);
            w.ue(2);
            w.ue(4);
            w.ue(1);
            w.ue(0);
            w.ue(3);
            w.flag(true); // adaptive marking
            w.ue(1);
            w.ue(0);
            w.ue(3);
            w.ue(2);
            w.ue(1);
            w.ue(4);
            w.ue(2);
            w.ue(6);
            w.ue(0);
            w.ue(0);
            w.ue(1); // cabac_init_idc
            w.se(2);
            w.ue(1); // deblocking off: no offsets
        });
        let h = SliceHeader::parse(&rbsp, &sps, &pps).expect("slice");
        assert_eq!(h.frame_num, 9);
        assert_eq!(h.num_ref_idx_l0_active, 3);
        assert_eq!(
            h.modifications_l0,
            vec![
                Modification::ShortTermSub(1),
                Modification::LongTerm(4),
                Modification::ShortTermAdd(0)
            ]
        );
        assert_eq!(
            h.marking,
            Some(Marking::Adaptive(vec![
                Mmco::ForgetShortTerm {
                    difference_minus1: 0
                },
                Mmco::ShortToLongTerm {
                    difference_minus1: 2,
                    long_term_frame_idx: 1
                },
                Mmco::MaxLongTermIdx { plus1: 2 },
                Mmco::CurrentToLongTerm {
                    long_term_frame_idx: 0
                },
            ]))
        );
        assert_eq!(h.cabac_init_idc, 1);
        assert_eq!(h.disable_deblocking_filter_idc, 1);
    }

    #[test]
    fn non_reference_slice_has_no_marking_and_weights_default() {
        let (sps, mut pps) = main_sets();
        pps.weighted_pred = true;
        let rbsp = header(0, 1, |w| {
            w.ue(0);
            w.ue(0); // P
            w.ue(0);
            w.bits(3, 16);
            w.flag(false); // no override: PPS default 1
            w.flag(false); // no modification
            w.ue(5); // luma denom
            w.ue(3); // chroma denom
            w.flag(true);
            w.se(40);
            w.se(-2);
            w.flag(false);
            w.ue(0);
            w.se(0);
            w.ue(0);
            w.se(0);
            w.se(0);
        });
        let h = SliceHeader::parse(&rbsp, &sps, &pps).expect("slice");
        assert_eq!(h.marking, None);
        let pw = h.pred_weights.expect("weights");
        assert_eq!((pw.luma_log2_denom, pw.chroma_log2_denom), (5, 3));
        assert_eq!(
            pw.l0,
            vec![RefWeights {
                luma: Some((40, -2)),
                chroma: None
            }]
        );
    }

    #[test]
    fn bad_headers_are_errors() {
        let (sps, pps) = main_sets();
        // first_mb past the picture (1280x720 = 3600 MBs).
        let rbsp = header(3, 1, |w| w.ue(3600));
        assert!(SliceHeader::parse(&rbsp, &sps, &pps).is_err());
        // IDR P slice.
        let rbsp = header(3, 5, |w| {
            w.ue(0);
            w.ue(0);
        });
        assert!(SliceHeader::parse(&rbsp, &sps, &pps).is_err());
        // Truncated.
        assert!(SliceHeader::parse(&[0x65], &sps, &pps).is_err());
        // Unterminated modification list.
        let rbsp = header(2, 1, |w| {
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.bits(1, 16);
            w.flag(false);
            w.flag(true);
            for _ in 0..80 {
                w.ue(0);
                w.ue(0);
            }
        });
        assert!(matches!(
            SliceHeader::parse(&rbsp, &sps, &pps),
            Err(Bad::Malformed(_))
        ));
    }

    #[test]
    fn header_bit_count_ignores_emulation_bytes() {
        // CAVLC (no cabac_init_idc), so the bits after frame_num can be laid
        // out to produce `00 00 02` and force an emulation-prevention byte.
        let h = SeqHeader::new(Profile::ConstrainedBaseline, 1280, 720, 30, 1);
        let sps = Sps::parse(&rbsp(&h.sps_nal())).expect("SPS");
        let pps = Pps::parse(&rbsp(&h.pps_nal()), |_| Some(&sps)).expect("PPS");
        let body = header(2, 1, |w| {
            w.ue(0); // first_mb: 1
            w.ue(5); // P: 00110            → byte 1 = 1_00110_1_0 (0x9a)
            w.ue(0); // pps: 1
            w.bits(0, 16); // frame_num 0   → byte 2 = 00
            w.flag(false); // override     → byte 3 = 00
            w.flag(false); // modification
            w.flag(false); // sliding window
            w.se(-8); // 000010001          → byte 4 = 02
            w.ue(1); // deblocking off
        });
        assert_eq!(&body[1..5], &[0x9a, 0, 0, 0x02]);
        let escaped = crate::codec::h264::nal(2, 1, &body[1..]);
        assert_eq!(
            &escaped[5..10],
            &[0x9a, 0, 0, 3, 0x02],
            "an emulation byte was added"
        );
        let s = SliceHeader::parse(&unescape(&escaped[4..]), &sps, &pps).expect("slice");
        assert_eq!(s.header_bits, 8 + 1 + 5 + 1 + 16 + 3 + 9 + 3);
        assert_eq!((s.frame_num, s.slice_qp_delta), (0, -8));
    }
}
