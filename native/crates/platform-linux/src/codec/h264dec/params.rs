//! Sequence and picture parameter sets (§7.3.2.1.1, §7.3.2.2) and the
//! scaling-matrix resolution of §7.4.2.1.1 / §7.4.2.2 (Tables 7-2 … 7-4).
//!
//! Everything a stateless decoder hands the driver is parsed; VUI is not (it
//! carries no decoding parameter — colour description and timing only — and
//! the presenter assumes BT.709 limited range, which every host encoder
//! signals).

use super::bits::BitReader;
use super::Bad;

/// `seq_parameter_set_id` range (§7.4.2.1.1).
pub(crate) const MAX_SPS: usize = 32;
/// `pic_parameter_set_id` range (§7.4.2.2).
pub(crate) const MAX_PPS: usize = 256;

/// Zigzag (frame) scan: bitstream order → raster index (§8.5.6, Table 8-13).
pub(crate) const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
/// 8x8 zigzag (frame) scan (§8.5.7, Table 8-14).
pub(crate) const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Table 7-3, in bitstream (zigzag) order as the standard prints them.
const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
/// Table 7-4, zigzag order.
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

fn raster<const N: usize>(zigzag: &[u8; N], scan: &[usize; N]) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, &v) in zigzag.iter().enumerate() {
        out[scan[i]] = v;
    }
    out
}

/// The scaling matrices a picture is decoded with, in **raster** order — the
/// order `VAIQMatrixBufferH264` documents (and FFmpeg / GStreamer fill).
/// Lists 0–5 are the 4x4 Intra Y/Cb/Cr, Inter Y/Cb/Cr; 8x8 lists 0–5 are
/// Intra Y, Inter Y, Intra Cb, Inter Cb, Intra Cr, Inter Cr (§7.4.2.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScalingMatrices {
    pub l4x4: [[u8; 16]; 6],
    pub l8x8: [[u8; 64]; 6],
}

impl ScalingMatrices {
    /// `Flat_4x4_16` / `Flat_8x8_16`: what "no scaling matrix" means.
    pub(crate) const FLAT: Self = Self {
        l4x4: [[16; 16]; 6],
        l8x8: [[16; 64]; 6],
    };

    fn default_list(i: usize) -> List {
        match i {
            0..=2 => List::L4(raster(&DEFAULT_4X4_INTRA, &ZIGZAG_4X4)),
            3..=5 => List::L4(raster(&DEFAULT_4X4_INTER, &ZIGZAG_4X4)),
            _ if i.is_multiple_of(2) => List::L8(raster(&DEFAULT_8X8_INTRA, &ZIGZAG_8X8)),
            _ => List::L8(raster(&DEFAULT_8X8_INTER, &ZIGZAG_8X8)),
        }
    }

    fn get(&self, i: usize) -> List {
        if i < 6 {
            List::L4(self.l4x4[i])
        } else {
            List::L8(self.l8x8[i - 6])
        }
    }

    fn set(&mut self, i: usize, list: List) {
        match list {
            List::L4(l) => self.l4x4[i] = l,
            List::L8(l) => self.l8x8[i - 6] = l,
        }
    }

    /// Resolve the twelve list slots of one parameter set (Table 7-2):
    /// `parsed[i]` is what the bitstream said for slot `i`; an absent slot
    /// takes fall-back rule A (`fallback = None`: the defaults, then the
    /// previous list of the same kind) or rule B (`Some(sequence)`: the
    /// sequence-level list for slots 0, 3, 6, 7, then the previous list).
    fn resolve(parsed: &[Option<Parsed>; 12], fallback: Option<&ScalingMatrices>) -> Self {
        let mut m = Self::FLAT;
        for (i, slot) in parsed.iter().enumerate() {
            let list = match *slot {
                Some(Parsed::UseDefault) => Self::default_list(i),
                Some(Parsed::Explicit(list)) => list,
                None => match (i, fallback) {
                    (0 | 3 | 6 | 7, None) => Self::default_list(i),
                    (0 | 3 | 6 | 7, Some(seq)) => seq.get(i),
                    // Cb/Cr 4x4 follow the previous 4x4 list; the 4:4:4 chroma
                    // 8x8 lists (8–11) follow the previous one of the same
                    // intra/inter kind, two slots back.
                    (1 | 2 | 4 | 5, _) => m.get(i - 1),
                    _ => m.get(i - 2),
                },
            };
            m.set(i, list);
        }
        m
    }
}

/// One resolved list, raster order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum List {
    L4([u8; 16]),
    L8([u8; 64]),
}

/// One `scaling_list()` as transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// `useDefaultScalingMatrixFlag`: the Table 7-3/7-4 default.
    UseDefault,
    /// Raster order.
    Explicit(List),
}

/// `scaling_list()` (§7.3.2.1.1.1) for slot `i` (0–5 are 4x4, 6–11 8x8).
fn scaling_list(r: &mut BitReader<'_>, i: usize) -> Result<Parsed, Bad> {
    let size = if i < 6 { 16 } else { 64 };
    let mut zigzag = [0u8; 64];
    let mut last: i32 = 8;
    let mut next: i32 = 8;
    for (j, z) in zigzag.iter_mut().take(size).enumerate() {
        if next != 0 {
            let delta = r.se_range(-128, 127, "delta_scale")?;
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Ok(Parsed::UseDefault);
            }
        }
        let v = if next == 0 { last } else { next };
        *z = v as u8;
        last = v;
    }
    Ok(Parsed::Explicit(if i < 6 {
        let mut z = [0u8; 16];
        z.copy_from_slice(&zigzag[..16]);
        List::L4(raster(&z, &ZIGZAG_4X4))
    } else {
        List::L8(raster(&zigzag, &ZIGZAG_8X8))
    }))
}

/// The `n` scaling-list slots of a `*_scaling_matrix_present_flag` block.
fn scaling_lists(r: &mut BitReader<'_>, n: usize) -> Result<[Option<Parsed>; 12], Bad> {
    let mut parsed = [None; 12];
    for (i, slot) in parsed.iter_mut().enumerate().take(n) {
        if r.flag()? {
            *slot = Some(scaling_list(r, i)?);
        }
    }
    Ok(parsed)
}

/// Picture order count derivation parameters (§8.2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PocType {
    /// POC from transmitted LSBs.
    Lsb { log2_max_lsb: u32 },
    /// POC from frame_num and an expected-delta cycle.
    Cycle {
        delta_pic_order_always_zero: bool,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        offset_for_ref_frame: Vec<i32>,
    },
    /// POC = 2 × frame_num (display order = decode order).
    FrameNum,
}

/// Frame cropping, in luma samples (already multiplied by CropUnitX/Y).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sps {
    pub id: usize,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass: bool,
    /// `seq_scaling_matrix_present_flag` — decides fall-back rule A vs B.
    pub scaling_present: bool,
    /// Sequence-level matrices (Flat when not present).
    pub scaling: ScalingMatrices,
    pub log2_max_frame_num: u32,
    pub poc: PocType,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    pub height_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub crop: Crop,
}

impl Sps {
    /// Profiles whose SPS carries the chroma/bit-depth/scaling block.
    fn has_chroma_info(profile_idc: u8) -> bool {
        matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        )
    }

    /// Parse an SPS RBSP (NAL header byte excluded).
    pub(crate) fn parse(rbsp: &[u8]) -> Result<Self, Bad> {
        let mut r = BitReader::new(rbsp);
        let profile_idc = r.bits(8)? as u8;
        r.bits(8)?; // constraint_set0..5 flags: no decoding parameter
        let level_idc = r.bits(8)? as u8;
        let id = r.ue_max(MAX_SPS as u32 - 1, "seq_parameter_set_id")? as usize;
        let mut chroma_format_idc = 1;
        let mut separate_colour_plane = false;
        let mut bit_depth_luma_minus8 = 0;
        let mut bit_depth_chroma_minus8 = 0;
        let mut qpprime_y_zero_transform_bypass = false;
        let mut scaling_present = false;
        let mut scaling = ScalingMatrices::FLAT;
        if Self::has_chroma_info(profile_idc) {
            chroma_format_idc = r.ue_max(3, "chroma_format_idc")?;
            if chroma_format_idc == 3 {
                separate_colour_plane = r.flag()?;
            }
            bit_depth_luma_minus8 = r.ue_max(6, "bit_depth_luma_minus8")?;
            bit_depth_chroma_minus8 = r.ue_max(6, "bit_depth_chroma_minus8")?;
            qpprime_y_zero_transform_bypass = r.flag()?;
            scaling_present = r.flag()?;
            if scaling_present {
                let n = if chroma_format_idc != 3 { 8 } else { 12 };
                scaling = ScalingMatrices::resolve(&scaling_lists(&mut r, n)?, None);
            }
        }
        let log2_max_frame_num = r.ue_max(12, "log2_max_frame_num_minus4")? + 4;
        let poc = match r.ue_max(2, "pic_order_cnt_type")? {
            0 => PocType::Lsb {
                log2_max_lsb: r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4,
            },
            1 => {
                let delta_pic_order_always_zero = r.flag()?;
                let offset_for_non_ref_pic = r.se()?;
                let offset_for_top_to_bottom_field = r.se()?;
                let n = r.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
                let offset_for_ref_frame = (0..n).map(|_| r.se()).collect::<Result<_, _>>()?;
                PocType::Cycle {
                    delta_pic_order_always_zero,
                    offset_for_non_ref_pic,
                    offset_for_top_to_bottom_field,
                    offset_for_ref_frame,
                }
            }
            _ => PocType::FrameNum,
        };
        // §A.3.1: MaxDpbFrames never exceeds 16.
        let max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")?;
        let gaps_in_frame_num_allowed = r.flag()?;
        let width_mbs = r.ue_max(1023, "pic_width_in_mbs_minus1")? + 1;
        let height_map_units = r.ue_max(1023, "pic_height_in_map_units_minus1")? + 1;
        let frame_mbs_only = r.flag()?;
        let mb_adaptive_frame_field = !frame_mbs_only && r.flag()?;
        let direct_8x8_inference = r.flag()?;
        let mut crop = Crop::default();
        if r.flag()? {
            // CropUnitX/Y (§7.4.2.1.1): 4:2:0 subsamples both ways; field
            // coding doubles the vertical unit.
            let (sub_w, sub_h) = match (chroma_format_idc, separate_colour_plane) {
                (1, false) => (2, 2),
                (2, false) => (2, 1),
                _ => (1, 1),
            };
            let unit_x = if chroma_format_idc == 0 { 1 } else { sub_w };
            let unit_y =
                (if chroma_format_idc == 0 { 1 } else { sub_h }) * (2 - frame_mbs_only as u32);
            // Larger offsets cannot leave a visible picture at the 1024-MB size
            // limit; bounding them keeps the sums below in range.
            const MAX_CROP: u32 = 8192;
            crop = Crop {
                left: r.ue_max(MAX_CROP, "frame_crop_left_offset")? * unit_x,
                right: r.ue_max(MAX_CROP, "frame_crop_right_offset")? * unit_x,
                top: r.ue_max(MAX_CROP, "frame_crop_top_offset")? * unit_y,
                bottom: r.ue_max(MAX_CROP, "frame_crop_bottom_offset")? * unit_y,
            };
        }
        let sps = Self {
            id,
            profile_idc,
            level_idc,
            chroma_format_idc,
            separate_colour_plane,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            qpprime_y_zero_transform_bypass,
            scaling_present,
            scaling,
            log2_max_frame_num,
            poc,
            max_num_ref_frames,
            gaps_in_frame_num_allowed,
            width_mbs,
            height_map_units,
            frame_mbs_only,
            mb_adaptive_frame_field,
            direct_8x8_inference,
            crop,
        };
        let (cw, ch) = (sps.coded_width(), sps.coded_height());
        if sps.crop.left + sps.crop.right >= cw || sps.crop.top + sps.crop.bottom >= ch {
            return Err(Bad::Malformed(format!(
                "SPS cropping {:?} leaves nothing of the {cw}x{ch} picture",
                sps.crop
            )));
        }
        Ok(sps)
    }

    pub(crate) fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }

    /// `ChromaArrayType` (§7.4.2.1.1).
    pub(crate) fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// Height in macroblocks of a frame (`FrameHeightInMbs`).
    pub(crate) fn height_mbs(&self) -> u32 {
        (2 - self.frame_mbs_only as u32) * self.height_map_units
    }

    pub(crate) fn coded_width(&self) -> u32 {
        self.width_mbs * 16
    }

    pub(crate) fn coded_height(&self) -> u32 {
        self.height_mbs() * 16
    }

    /// The displayed size after cropping.
    pub(crate) fn visible_size(&self) -> (u32, u32) {
        (
            self.coded_width() - self.crop.left - self.crop.right,
            self.coded_height() - self.crop.top - self.crop.bottom,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pps {
    pub id: usize,
    pub sps_id: usize,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    /// `pic_scaling_matrix_present_flag` and the slots it transmitted;
    /// resolved against the active SPS by [`Pps::scaling`].
    pub scaling: Option<[Option<Parsed>; 12]>,
    pub second_chroma_qp_index_offset: i32,
}

impl Pps {
    /// Parse a PPS RBSP (NAL header excluded). `sps_of` finds the SPS it names:
    /// the scaling-list count and the QP range depend on it.
    pub(crate) fn parse<'s>(
        rbsp: &[u8],
        sps_of: impl Fn(usize) -> Option<&'s Sps>,
    ) -> Result<Self, Bad> {
        let mut r = BitReader::new(rbsp);
        let id = r.ue_max(MAX_PPS as u32 - 1, "pic_parameter_set_id")? as usize;
        let sps_id = r.ue_max(MAX_SPS as u32 - 1, "seq_parameter_set_id")? as usize;
        let sps = sps_of(sps_id).ok_or_else(|| {
            Bad::Malformed(format!("PPS {id} names SPS {sps_id}, never received"))
        })?;
        let entropy_coding_mode = r.flag()?;
        let bottom_field_pic_order_in_frame_present = r.flag()?;
        if r.ue()? != 0 {
            return Err(Bad::Unsupported(
                "slice groups (FMO, a Baseline-only tool no supported encoder emits)".into(),
            ));
        }
        let num_ref_idx_l0_default_active =
            r.ue_max(31, "num_ref_idx_l0_default_active_minus1")? + 1;
        let num_ref_idx_l1_default_active =
            r.ue_max(31, "num_ref_idx_l1_default_active_minus1")? + 1;
        let weighted_pred = r.flag()?;
        let weighted_bipred_idc = r.bits(2)?;
        if weighted_bipred_idc == 3 {
            return Err(Bad::Malformed("weighted_bipred_idc = 3 (reserved)".into()));
        }
        let qp_bd_offset = 6 * sps.bit_depth_luma_minus8 as i32;
        let pic_init_qp_minus26 = r.se_range(-(26 + qp_bd_offset), 25, "pic_init_qp_minus26")?;
        let pic_init_qs_minus26 = r.se_range(-26, 25, "pic_init_qs_minus26")?;
        let chroma_qp_index_offset = r.se_range(-12, 12, "chroma_qp_index_offset")?;
        let deblocking_filter_control_present = r.flag()?;
        let constrained_intra_pred = r.flag()?;
        let redundant_pic_cnt_present = r.flag()?;
        let mut transform_8x8_mode = false;
        let mut scaling = None;
        let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
        if r.more_rbsp_data() {
            transform_8x8_mode = r.flag()?;
            if r.flag()? {
                let per_8x8 = if sps.chroma_format_idc != 3 { 2 } else { 6 };
                let n = 6 + per_8x8 * transform_8x8_mode as usize;
                scaling = Some(scaling_lists(&mut r, n)?);
            }
            second_chroma_qp_index_offset = r.se_range(-12, 12, "second_chroma_qp_index_offset")?;
        }
        Ok(Self {
            id,
            sps_id,
            entropy_coding_mode,
            bottom_field_pic_order_in_frame_present,
            num_ref_idx_l0_default_active,
            num_ref_idx_l1_default_active,
            weighted_pred,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present,
            constrained_intra_pred,
            redundant_pic_cnt_present,
            transform_8x8_mode,
            scaling,
            second_chroma_qp_index_offset,
        })
    }

    /// The matrices a picture using this PPS with `sps` is decoded with
    /// (§7.4.2.2: the sequence-level ones unless the PPS transmits its own,
    /// fall-back rule A or B depending on whether the SPS sent any).
    pub(crate) fn scaling(&self, sps: &Sps) -> ScalingMatrices {
        match &self.scaling {
            None => sps.scaling,
            Some(parsed) => ScalingMatrices::resolve(
                parsed,
                if sps.scaling_present {
                    Some(&sps.scaling)
                } else {
                    None
                },
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::h264::{BitWriter, Profile, SeqHeader};
    use crate::codec::h264dec::bits::unescape;

    /// The RBSP of an Annex-B NAL (4-byte start code), header byte dropped.
    pub(crate) fn rbsp(nal: &[u8]) -> Vec<u8> {
        let body = nal.strip_prefix(&[0, 0, 0, 1]).unwrap_or(nal);
        unescape(&body[1..])
    }

    /// FFmpeg's `default_scaling4/8` tables (libavcodec/h264_ps.c), which are
    /// stored in raster order — an independent copy of Tables 7-3/7-4 that
    /// pins both the zigzag tables and the spec transcription above.
    #[test]
    fn default_matrices_match_ffmpegs_raster_tables() {
        assert_eq!(
            raster(&DEFAULT_4X4_INTRA, &ZIGZAG_4X4),
            [6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42]
        );
        assert_eq!(
            raster(&DEFAULT_4X4_INTER, &ZIGZAG_4X4),
            [10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34]
        );
        let intra: [u8; 64] = [
            6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27,
            29, 31, 16, 18, 23, 25, 27, 29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29,
            31, 33, 36, 38, 25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
        ];
        let inter: [u8; 64] = [
            9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24,
            25, 27, 17, 19, 21, 22, 24, 25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25,
            27, 28, 30, 32, 22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
        ];
        assert_eq!(raster(&DEFAULT_8X8_INTRA, &ZIGZAG_8X8), intra);
        assert_eq!(raster(&DEFAULT_8X8_INTER, &ZIGZAG_8X8), inter);
    }

    #[test]
    fn zigzag_tables_are_permutations() {
        let mut seen4 = ZIGZAG_4X4;
        seen4.sort_unstable();
        assert_eq!(seen4, std::array::from_fn::<usize, 16, _>(|i| i));
        let mut seen8 = ZIGZAG_8X8;
        seen8.sort_unstable();
        assert_eq!(seen8, std::array::from_fn::<usize, 64, _>(|i| i));
    }

    /// The encoder's own headers (pinned byte-exact against FFmpeg in
    /// `h264::tests`) read back field for field.
    #[test]
    fn parses_the_vaapi_encoders_sps_and_pps() {
        let h = SeqHeader::new(Profile::Main, 1920, 1080, 60, 1);
        let sps = Sps::parse(&rbsp(&h.sps_nal())).expect("SPS");
        assert_eq!((sps.profile_idc, sps.level_idc, sps.id), (77, 42, 0));
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.log2_max_frame_num, 16);
        assert_eq!(sps.poc, PocType::FrameNum);
        assert_eq!(sps.max_num_ref_frames, 1);
        assert!(sps.frame_mbs_only && sps.direct_8x8_inference);
        assert_eq!((sps.coded_width(), sps.coded_height()), (1920, 1088));
        assert_eq!(sps.visible_size(), (1920, 1080));
        assert_eq!(
            sps.crop,
            Crop {
                left: 0,
                right: 0,
                top: 0,
                bottom: 8
            }
        );
        assert_eq!(sps.scaling, ScalingMatrices::FLAT);

        let pps = Pps::parse(&rbsp(&h.pps_nal()), |id| (id == 0).then_some(&sps)).expect("PPS");
        assert!(pps.entropy_coding_mode);
        assert_eq!(pps.num_ref_idx_l0_default_active, 1);
        assert!(pps.deblocking_filter_control_present);
        assert!(!pps.transform_8x8_mode);
        assert_eq!(pps.second_chroma_qp_index_offset, 0);
        assert_eq!(pps.scaling(&sps), ScalingMatrices::FLAT);
        assert!(Pps::parse(&rbsp(&h.pps_nal()), |_| None).is_err());
    }

    /// A High-profile SPS with scaling lists, POC type 0 and a left/top crop,
    /// written field by field.
    fn high_sps(write_lists: impl Fn(&mut BitWriter)) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(100, 8);
        w.bits(0, 8);
        w.bits(40, 8);
        w.ue(3); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(true); // seq_scaling_matrix_present_flag
        write_lists(&mut w);
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // poc type 0
        w.ue(2); // log2_max_poc_lsb_minus4
        w.ue(4); // refs
        w.flag(false);
        w.ue(79); // 1280
        w.ue(44); // 720
        w.flag(true);
        w.flag(true);
        w.flag(true); // cropping
        w.ue(1);
        w.ue(2);
        w.ue(3);
        w.ue(4);
        w.flag(false); // no VUI
        w.finish()
    }

    #[test]
    fn high_profile_scaling_and_fallback_rule_a() {
        // Slot 0 explicit (all deltas 0 except the first: every value 8+1=9),
        // slot 3 "use default", every other slot absent.
        let rbsp = high_sps(|w| {
            w.flag(true);
            w.se(1);
            for _ in 1..16 {
                w.se(0);
            }
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.se(-8); // next = 0 at j = 0 → useDefault
            for _ in 4..8 {
                w.flag(false);
            }
        });
        let sps = Sps::parse(&rbsp).expect("SPS");
        assert_eq!(sps.id, 3);
        assert_eq!(sps.poc, PocType::Lsb { log2_max_lsb: 6 });
        assert_eq!(sps.max_num_ref_frames, 4);
        assert_eq!(
            sps.crop,
            Crop {
                left: 2,
                right: 4,
                top: 6,
                bottom: 8
            }
        );
        assert_eq!(sps.visible_size(), (1280 - 6, 720 - 14));
        let m = sps.scaling;
        assert_eq!(m.l4x4[0], [9; 16]);
        // Rule A: 1 and 2 copy 0.
        assert_eq!(m.l4x4[1], [9; 16]);
        assert_eq!(m.l4x4[2], [9; 16]);
        let inter = raster(&DEFAULT_4X4_INTER, &ZIGZAG_4X4);
        assert_eq!(m.l4x4[3], inter);
        assert_eq!(m.l4x4[5], inter);
        assert_eq!(m.l8x8[0], raster(&DEFAULT_8X8_INTRA, &ZIGZAG_8X8));
        assert_eq!(m.l8x8[1], raster(&DEFAULT_8X8_INTER, &ZIGZAG_8X8));
    }

    #[test]
    fn explicit_list_is_converted_from_zigzag() {
        // Deltas 1,1,1… give zigzag values 9,10,11,…: raster[ZIGZAG[i]] = 9+i.
        let rbsp = high_sps(|w| {
            w.flag(true);
            for _ in 0..16 {
                w.se(1);
            }
            for _ in 1..8 {
                w.flag(false);
            }
        });
        let sps = Sps::parse(&rbsp).expect("SPS");
        for (i, &pos) in ZIGZAG_4X4.iter().enumerate() {
            assert_eq!(sps.scaling.l4x4[0][pos], 9 + i as u8);
        }
        // A list that hits 0 mid-way repeats the last value to the end.
        let rbsp = high_sps(|w| {
            w.flag(true);
            w.se(4); // 12
            w.se(-12); // next = 0 → 12 for the rest
            for _ in 1..8 {
                w.flag(false);
            }
        });
        let sps = Sps::parse(&rbsp).expect("SPS");
        assert_eq!(sps.scaling.l4x4[0], [12; 16]);
    }

    #[test]
    fn pps_scaling_fallback_rule_b_uses_the_sequence_lists() {
        let rbsp = high_sps(|w| {
            w.flag(true);
            w.se(1); // all 9
            for _ in 1..16 {
                w.se(0);
            }
            for _ in 1..8 {
                w.flag(false);
            }
        });
        let sps = Sps::parse(&rbsp).expect("SPS");
        let mut w = BitWriter::default();
        w.ue(7); // pps id
        w.ue(3); // sps id
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.ue(2);
        w.ue(0);
        w.flag(false);
        w.bits(0, 2);
        w.se(0);
        w.se(0);
        w.se(-2);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        // more_rbsp_data: transform_8x8, matrix present, slot 1 explicit only.
        w.flag(true);
        w.flag(true);
        w.flag(false); // slot 0 absent → rule B: SPS list 0 (all 9)
        w.flag(true); // slot 1: all 20
        w.se(12);
        for _ in 1..16 {
            w.se(0);
        }
        for _ in 2..8 {
            w.flag(false);
        }
        w.se(3); // second_chroma_qp_index_offset
        let pps = Pps::parse(&w.finish(), |id| (id == 3).then_some(&sps)).expect("PPS");
        assert_eq!(pps.id, 7);
        assert_eq!(pps.num_ref_idx_l0_default_active, 3);
        assert_eq!(pps.chroma_qp_index_offset, -2);
        assert_eq!(pps.second_chroma_qp_index_offset, 3);
        assert!(pps.transform_8x8_mode);
        let m = pps.scaling(&sps);
        assert_eq!(m.l4x4[0], [9; 16]);
        assert_eq!(m.l4x4[1], [20; 16]);
        assert_eq!(m.l4x4[2], [20; 16]); // previous (rule B for slot 2)
        assert_eq!(m.l4x4[3], sps.scaling.l4x4[3]); // SPS list 3
        assert_eq!(m.l8x8[0], sps.scaling.l8x8[0]);
    }

    #[test]
    fn hostile_values_are_refused() {
        // log2_max_frame_num_minus4 = 13.
        let mut w = BitWriter::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(0);
        w.ue(13);
        assert!(matches!(Sps::parse(&w.finish()), Err(Bad::Malformed(_))));
        // Truncated.
        assert!(Sps::parse(&[66, 0xc0]).is_err());
        // Crop larger than the picture.
        let mut w = BitWriter::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(0);
        w.ue(0);
        w.ue(2);
        w.ue(1);
        w.flag(false);
        w.ue(0); // 16 wide
        w.ue(0); // 16 high
        w.flag(true);
        w.flag(true);
        w.flag(true);
        w.ue(4);
        w.ue(4); // 16 cropped of 16
        w.ue(0);
        w.ue(0);
        w.flag(false);
        assert!(matches!(Sps::parse(&w.finish()), Err(Bad::Malformed(_))));
    }
}
