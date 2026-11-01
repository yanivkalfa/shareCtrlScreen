//! Just enough H.264 / HEVC header parsing to check an encoder's output
//! shape: NAL framing, parameter-set ids, slice types and picture order
//! counts. Not a decoder — nothing past the slice header's picture order
//! count is read.
//!
//! WHY a parser of its own: the full H.264 header parser in the tree
//! (`platform-linux/src/codec/h264dec`) is private to the VA-API backend and
//! compiled only on Linux; this check runs on every OS and covers HEVC too.
//! Every read is bounds-checked and reports an error instead of panicking,
//! since the bytes come from encoders under test.

use std::collections::HashMap;

use platform_api::Codec;

/// The NAL units of an Annex-B byte stream (start codes excluded, header
/// byte(s) included). `00 00 01` cannot occur inside a NAL payload
/// (emulation prevention), so a plain scan finds every boundary; zero bytes
/// in front of a start code are its leading `zero_byte`s, not payload.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(stream.len(), |&n| n - 3);
        if k + 1 < starts.len() {
            while end > s && stream[end - 1] == 0 {
                end -= 1;
            }
        }
        if end > s {
            out.push(&stream[s..end]);
        }
    }
    out
}

/// The NAL unit type: 5 bits of the H.264 header, 6 bits of the HEVC one.
pub fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    match codec {
        Codec::H264 => Some(first & 0x1f),
        Codec::Hevc => Some((first >> 1) & 0x3f),
        Codec::Av1 => None,
    }
}

/// The RBSP of a NAL unit: emulation-prevention bytes removed (H.264 and
/// HEVC §7.4.2: `00 00 03 0x` → `00 00 0x`).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Big-endian bit reader over an RBSP.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn flag(&mut self) -> Result<bool, String> {
        let byte = self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| "header ends early".to_string())?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    /// `u(n)`, `n` ≤ 32.
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.flag()? as u64;
        }
        u32::try_from(v).map_err(|_| format!("u({n}) out of range"))
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        if self.pos + n > self.data.len() * 8 {
            return Err("header ends early".into());
        }
        self.pos += n;
        Ok(())
    }

    /// `ue(v)`. More than 31 leading zeros does not fit a u32 and is never
    /// produced by a conforming encoder for the fields read here.
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err("exp-Golomb code longer than 32 bits".into());
            }
        }
        let rest = self.bits(zeros)? as u64;
        u32::try_from((1u64 << zeros) - 1 + rest).map_err(|_| "ue(v) out of range".to_string())
    }

    fn se(&mut self) -> Result<i32, String> {
        let k = self.ue()? as i64;
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).map_err(|_| "se(v) out of range".to_string())
    }
}

/// The slice type of a picture's slices, normalised across codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SliceKind {
    I,
    P,
    B,
    /// H.264 SP / SI (Extended profile only; no encoder here makes them).
    Switching,
}

/// What [`Parser::slice`] reads from one slice header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceInfo {
    /// `None` for an HEVC dependent slice segment, which has no slice type
    /// of its own.
    pub kind: Option<SliceKind>,
    /// The first slice of its picture (H.264 `first_mb_in_slice == 0`, HEVC
    /// `first_slice_segment_in_pic_flag`).
    pub first_in_picture: bool,
    /// `None` for an HEVC dependent slice segment, which repeats nothing.
    pub poc_lsb: Option<u32>,
    /// `MaxPicOrderCntLsb` of the slice's SPS (meaningful with `poc_lsb`).
    pub max_poc_lsb: u32,
    pub idr: bool,
    /// Later pictures derive their `PicOrderCntMsb` from this one — H.264: a
    /// reference picture (`nal_ref_idc != 0`, §8.2.1.1); HEVC: `TemporalId`
    /// 0 and not a RADL, RASL or sub-layer non-reference picture (§8.3.1).
    pub poc_anchor: bool,
}

#[derive(Debug, Clone)]
struct H264Sps {
    profile_idc: u8,
    separate_colour_plane: bool,
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    frame_mbs_only: bool,
}

#[derive(Debug, Clone)]
struct HevcSps {
    profile_idc: u8,
    separate_colour_plane: bool,
    log2_max_poc_lsb: u32,
    /// `Ceil(Log2(PicSizeInCtbsY))`, the width of `slice_segment_address`.
    address_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct HevcPps {
    sps_id: u32,
    dependent_slice_segments: bool,
    output_flag_present: bool,
    num_extra_slice_header_bits: u32,
}

/// Parameter sets seen so far in a stream, keyed by id as the slices refer
/// to them.
#[derive(Debug, Default)]
pub struct Parser {
    h264_sps: HashMap<u32, H264Sps>,
    h264_pps: HashMap<u32, u32>,
    hevc_sps: HashMap<u32, HevcSps>,
    hevc_pps: HashMap<u32, HevcPps>,
}

/// `profile_idc` values whose SPS carries the chroma / bit-depth / scaling
/// fields (H.264 §7.3.2.1.1).
const H264_HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

impl Parser {
    /// Record a parameter set. Other NAL types are ignored.
    pub fn parameter_set(&mut self, codec: Codec, nal: &[u8]) -> Result<(), String> {
        match (codec, nal_type(codec, nal)) {
            (Codec::H264, Some(7)) => self.h264_sps(nal),
            (Codec::H264, Some(8)) => self.h264_pps(nal),
            (Codec::Hevc, Some(33)) => self.hevc_sps(nal),
            (Codec::Hevc, Some(34)) => self.hevc_pps(nal),
            _ => Ok(()),
        }
    }

    /// `profile_idc` of the most recent SPS with id 0, for reports.
    pub fn profile_idc(&self, codec: Codec) -> Option<u8> {
        match codec {
            Codec::H264 => self.h264_sps.get(&0).map(|s| s.profile_idc),
            Codec::Hevc => self.hevc_sps.get(&0).map(|s| s.profile_idc),
            Codec::Av1 => None,
        }
    }

    fn h264_sps(&mut self, nal: &[u8]) -> Result<(), String> {
        let rbsp = unescape(nal);
        let mut r = Bits::new(&rbsp);
        r.skip(8)?; // NAL header
        let profile_idc = r.bits(8)? as u8;
        r.skip(16)?; // constraint flags + reserved, level_idc
        let id = r.ue()?;
        let mut separate_colour_plane = false;
        if H264_HIGH_PROFILES.contains(&profile_idc) {
            let chroma_format_idc = r.ue()?;
            if chroma_format_idc == 3 {
                separate_colour_plane = r.flag()?;
            }
            r.ue()?; // bit_depth_luma_minus8
            r.ue()?; // bit_depth_chroma_minus8
            r.flag()?; // qpprime_y_zero_transform_bypass_flag
            if r.flag()? {
                // seq_scaling_matrix_present_flag: skip every list (§7.3.2.1.1.1).
                let lists = if chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..lists {
                    if r.flag()? {
                        let size = if i < 6 { 16 } else { 64 };
                        let (mut last, mut next) = (8i32, 8i32);
                        for _ in 0..size {
                            if next != 0 {
                                next = (last + r.se()? + 256) % 256;
                            }
                            if next != 0 {
                                last = next;
                            }
                        }
                    }
                }
            }
        }
        let log2_max_frame_num = r.ue()? + 4;
        let poc_type = r.ue()?;
        let mut log2_max_poc_lsb = 0;
        match poc_type {
            0 => log2_max_poc_lsb = r.ue()? + 4,
            1 => {
                r.flag()?; // delta_pic_order_always_zero_flag
                r.se()?; // offset_for_non_ref_pic
                r.se()?; // offset_for_top_to_bottom_field
                for _ in 0..r.ue()? {
                    r.se()?;
                }
            }
            2 => {}
            other => return Err(format!("pic_order_cnt_type {other}")),
        }
        r.ue()?; // max_num_ref_frames
        r.flag()?; // gaps_in_frame_num_value_allowed_flag
        r.ue()?; // pic_width_in_mbs_minus1
        r.ue()?; // pic_height_in_map_units_minus1
        let frame_mbs_only = r.flag()?;
        if log2_max_frame_num > 16 || log2_max_poc_lsb > 16 {
            return Err("SPS field out of range".into());
        }
        self.h264_sps.insert(
            id,
            H264Sps {
                profile_idc,
                separate_colour_plane,
                log2_max_frame_num,
                poc_type,
                log2_max_poc_lsb,
                frame_mbs_only,
            },
        );
        Ok(())
    }

    fn h264_pps(&mut self, nal: &[u8]) -> Result<(), String> {
        let rbsp = unescape(nal);
        let mut r = Bits::new(&rbsp);
        r.skip(8)?;
        let id = r.ue()?;
        let sps_id = r.ue()?;
        self.h264_pps.insert(id, sps_id);
        Ok(())
    }

    fn hevc_sps(&mut self, nal: &[u8]) -> Result<(), String> {
        let rbsp = unescape(nal);
        let mut r = Bits::new(&rbsp);
        r.skip(16)?; // NAL header
        r.skip(4)?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = r.bits(3)?;
        r.skip(1)?; // sps_temporal_id_nesting_flag
                    // profile_tier_level(1, sps_max_sub_layers_minus1), §7.3.3.
        r.skip(3)?; // general_profile_space, general_tier_flag
        let profile_idc = r.bits(5)? as u8;
        r.skip(32 + 4 + 43 + 1)?; // compatibility flags .. general_inbld_flag
        r.skip(8)?; // general_level_idc
        let mut sub_layers = Vec::new();
        for _ in 0..max_sub_layers_minus1 {
            sub_layers.push((r.flag()?, r.flag()?));
        }
        if max_sub_layers_minus1 > 0 {
            r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
        }
        for (profile_present, level_present) in sub_layers {
            if profile_present {
                r.skip(88)?;
            }
            if level_present {
                r.skip(8)?;
            }
        }
        let id = r.ue()?;
        let chroma_format_idc = r.ue()?;
        let separate_colour_plane = chroma_format_idc == 3 && r.flag()?;
        let width = r.ue()?;
        let height = r.ue()?;
        if r.flag()? {
            // conformance_window_flag: four offsets.
            for _ in 0..4 {
                r.ue()?;
            }
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        let log2_max_poc_lsb = r.ue()? + 4;
        let all_sub_layers = r.flag()?; // sps_sub_layer_ordering_info_present_flag
        let first = if all_sub_layers {
            0
        } else {
            max_sub_layers_minus1
        };
        for _ in first..=max_sub_layers_minus1 {
            r.ue()?; // max_dec_pic_buffering_minus1
            r.ue()?; // max_num_reorder_pics
            r.ue()?; // max_latency_increase_plus1
        }
        let log2_min_cb = r.ue()? + 3;
        let log2_ctb = log2_min_cb + r.ue()?;
        if log2_max_poc_lsb > 16 || !(4..=6).contains(&log2_ctb) || width == 0 || height == 0 {
            return Err("SPS field out of range".into());
        }
        let ctb = 1u64 << log2_ctb;
        let ctbs = (width as u64).div_ceil(ctb) * (height as u64).div_ceil(ctb);
        // Ceil(Log2(n)): the bits needed for addresses 0..n.
        let address_bits = u64::BITS - (ctbs - 1).leading_zeros();
        self.hevc_sps.insert(
            id,
            HevcSps {
                profile_idc,
                separate_colour_plane,
                log2_max_poc_lsb,
                address_bits,
            },
        );
        Ok(())
    }

    fn hevc_pps(&mut self, nal: &[u8]) -> Result<(), String> {
        let rbsp = unescape(nal);
        let mut r = Bits::new(&rbsp);
        r.skip(16)?;
        let id = r.ue()?;
        let sps_id = r.ue()?;
        let dependent_slice_segments = r.flag()?;
        let output_flag_present = r.flag()?;
        let num_extra_slice_header_bits = r.bits(3)?;
        self.hevc_pps.insert(
            id,
            HevcPps {
                sps_id,
                dependent_slice_segments,
                output_flag_present,
                num_extra_slice_header_bits,
            },
        );
        Ok(())
    }

    /// Parse the start of a slice header (H.264 types 1 and 5, HEVC types
    /// 0–21); `None` for any other NAL type. Fails when the slice names a
    /// parameter set this stream has not sent yet.
    pub fn slice(&self, codec: Codec, nal: &[u8]) -> Result<Option<SliceInfo>, String> {
        match (codec, nal_type(codec, nal)) {
            (Codec::H264, Some(t @ (1 | 5))) => self.h264_slice(nal, t == 5).map(Some),
            // 0–9 and 16–21 are the defined VCL types; 10–15 and 22–31 reserved.
            (Codec::Hevc, Some(t @ (0..=9 | 16..=21))) => self.hevc_slice(nal, t).map(Some),
            _ => Ok(None),
        }
    }

    fn h264_slice(&self, nal: &[u8], idr: bool) -> Result<SliceInfo, String> {
        let rbsp = unescape(nal);
        let mut r = Bits::new(&rbsp);
        let reference = (rbsp.first().copied().unwrap_or(0) >> 5) & 3 != 0;
        r.skip(8)?;
        let first_mb = r.ue()?;
        let slice_type = r.ue()?;
        let pps_id = r.ue()?;
        let sps = self
            .h264_pps
            .get(&pps_id)
            .and_then(|sps_id| self.h264_sps.get(sps_id))
            .ok_or_else(|| format!("slice refers to PPS {pps_id} before its SPS/PPS"))?;
        if sps.separate_colour_plane {
            r.skip(2)?; // colour_plane_id
        }
        r.skip(sps.log2_max_frame_num as usize)?; // frame_num
        if !sps.frame_mbs_only && r.flag()? {
            return Err("field-coded picture".into());
        }
        if idr {
            r.ue()?; // idr_pic_id
        }
        let poc_lsb = match sps.poc_type {
            0 => Some(r.bits(sps.log2_max_poc_lsb)?),
            // Type 2: output order is decode order by definition (§8.2.1.3).
            2 => None,
            other => return Err(format!("pic_order_cnt_type {other} is not checked here")),
        };
        let kind = match slice_type % 5 {
            0 => SliceKind::P,
            1 => SliceKind::B,
            2 => SliceKind::I,
            _ => SliceKind::Switching,
        };
        Ok(SliceInfo {
            kind: Some(kind),
            first_in_picture: first_mb == 0,
            poc_lsb,
            max_poc_lsb: 1 << sps.log2_max_poc_lsb,
            idr,
            poc_anchor: reference,
        })
    }

    fn hevc_slice(&self, nal: &[u8], t: u8) -> Result<SliceInfo, String> {
        let rbsp = unescape(nal);
        if rbsp.len() < 2 {
            return Err("NAL header ends early".into());
        }
        let temporal_id = (rbsp[1] & 7).saturating_sub(1);
        let mut r = Bits::new(&rbsp);
        r.skip(16)?;
        let first = r.flag()?;
        if (16..=23).contains(&t) {
            r.flag()?; // no_output_of_prior_pics_flag
        }
        let pps_id = r.ue()?;
        let pps = self
            .hevc_pps
            .get(&pps_id)
            .ok_or_else(|| format!("slice refers to PPS {pps_id} before it was sent"))?;
        let sps = self
            .hevc_sps
            .get(&pps.sps_id)
            .ok_or_else(|| format!("PPS {pps_id} refers to SPS {} before it", pps.sps_id))?;
        let idr = t == 19 || t == 20;
        // Sub-layer non-reference pictures are the even types below 16
        // (§7.4.2.2); RADL is 6–7, RASL 8–9.
        let sub_layer_non_reference = t < 16 && t.is_multiple_of(2);
        let poc_anchor = temporal_id == 0 && !sub_layer_non_reference && !(6..=9).contains(&t);
        let max_poc_lsb = 1 << sps.log2_max_poc_lsb;
        let mut dependent = false;
        if !first {
            if pps.dependent_slice_segments {
                dependent = r.flag()?;
            }
            r.skip(sps.address_bits as usize)?; // slice_segment_address
        }
        if dependent {
            // A dependent segment carries no slice type or POC of its own: it
            // continues the independent segment before it, already checked.
            return Ok(SliceInfo {
                kind: None,
                first_in_picture: false,
                poc_lsb: None,
                max_poc_lsb,
                idr,
                poc_anchor,
            });
        }
        r.skip(pps.num_extra_slice_header_bits as usize)?; // slice_reserved_flag[]
        let kind = match r.ue()? {
            0 => SliceKind::B,
            1 => SliceKind::P,
            2 => SliceKind::I,
            other => return Err(format!("slice_type {other}")),
        };
        if pps.output_flag_present {
            r.flag()?; // pic_output_flag
        }
        if sps.separate_colour_plane {
            r.skip(2)?; // colour_plane_id
        }
        let poc_lsb = if idr {
            Some(0)
        } else {
            Some(r.bits(sps.log2_max_poc_lsb)?)
        };
        Ok(SliceInfo {
            kind: Some(kind),
            first_in_picture: first,
            poc_lsb,
            max_poc_lsb,
            idr,
            poc_anchor,
        })
    }
}

/// `PicOrderCntMsb` tracking (H.264 §8.2.1.1 for type 0, HEVC §8.3.1): the
/// full picture order count of each picture from its lsb.
#[derive(Debug, Default)]
pub struct PocTracker {
    prev_msb: i64,
    prev_lsb: i64,
}

impl PocTracker {
    /// The picture order count of a picture with `lsb`. `reset` for an IDR
    /// (both codecs restart at 0); `anchor` when this picture is what later
    /// pictures derive their msb from (H.264: a reference picture; HEVC: a
    /// `TemporalId` 0 picture that is not a RADL, RASL or sub-layer
    /// non-reference picture).
    pub fn poc(&mut self, lsb: u32, max_lsb: u32, reset: bool, anchor: bool) -> i64 {
        if reset {
            self.prev_msb = 0;
            self.prev_lsb = 0;
        }
        let (lsb, max) = (lsb as i64, max_lsb as i64);
        let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        if anchor || reset {
            self.prev_msb = msb;
            self.prev_lsb = lsb;
        }
        msb + lsb
    }
}

/// Hand-built H.264 headers for this module's and [`crate::shape`]'s tests:
/// a Main-profile SPS (640x368, 4-bit `frame_num`, POC type 0 with a 6-bit
/// lsb, or type 2), its PPS, and slice headers.
#[cfg(test)]
pub(crate) mod test_headers {
    /// Bit writer.
    #[derive(Default)]
    pub(crate) struct W {
        bits: Vec<bool>,
    }

    impl W {
        pub(crate) fn u(&mut self, n: u32, v: u32) -> &mut Self {
            for i in (0..n).rev() {
                self.bits.push((v >> i) & 1 == 1);
            }
            self
        }
        pub(crate) fn ue(&mut self, v: u32) -> &mut Self {
            let x = v as u64 + 1;
            let len = 64 - x.leading_zeros();
            for _ in 0..len - 1 {
                self.bits.push(false);
            }
            for i in (0..len).rev() {
                self.bits.push((x >> i) & 1 == 1);
            }
            self
        }
        pub(crate) fn bytes(&mut self) -> Vec<u8> {
            self.bits.push(true); // rbsp_stop_one_bit
            while !self.bits.len().is_multiple_of(8) {
                self.bits.push(false);
            }
            self.bits
                .chunks(8)
                .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8))
                .collect()
        }
    }

    pub(crate) fn h264_sps(poc_type: u32) -> Vec<u8> {
        let mut w = W::default();
        w.u(8, 0x67).u(8, 77).u(8, 0).u(8, 30).ue(0); // Main, id 0
        w.ue(0).ue(poc_type); // log2_max_frame_num 4
        if poc_type == 0 {
            w.ue(2); // log2_max_poc_lsb 6
        }
        w.ue(1).u(1, 0).ue(39).ue(22).u(1, 1); // refs, gaps, 640x368, frame_mbs_only
        w.bytes()
    }

    pub(crate) fn h264_pps() -> Vec<u8> {
        W::default().u(8, 0x68).ue(0).ue(0).u(1, 1).bytes()
    }

    /// A slice of a POC-type-0 stream: `nal_header` 0x65 IDR, 0x41
    /// reference, 0x01 non-reference; `slice_type` 5 P, 6 B, 7 I.
    pub(crate) fn h264_slice(nal_header: u32, slice_type: u32, idr: bool, lsb: u32) -> Vec<u8> {
        let mut w = W::default();
        w.u(8, nal_header).ue(0).ue(slice_type).ue(0).u(4, 0);
        if idr {
            w.ue(0);
        }
        w.u(6, lsb);
        w.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_headers::*;
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let s = [
            0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 0, 1, 0x65, 3, 0,
        ];
        let nals = nal_units(&s);
        assert_eq!(
            nals,
            vec![&[0x67, 1][..], &[0x68, 2][..], &[0x65, 3, 0][..]]
        );
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn exp_golomb_and_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
        let data = W::default().ue(0).ue(1).ue(7).ue(300).bytes();
        let mut r = Bits::new(&data);
        assert_eq!(
            (r.ue(), r.ue(), r.ue(), r.ue()),
            (Ok(0), Ok(1), Ok(7), Ok(300))
        );
        let neg = W::default().ue(4).ue(3).bytes();
        let mut r = Bits::new(&neg);
        assert_eq!((r.se(), r.se()), (Ok(-2), Ok(2)));
        assert!(Bits::new(&[0, 0, 0, 0]).ue().is_err());
    }

    #[test]
    fn h264_slice_header_fields() {
        let mut p = Parser::default();
        p.parameter_set(Codec::H264, &h264_sps(0)).unwrap();
        p.parameter_set(Codec::H264, &h264_pps()).unwrap();
        assert_eq!(p.profile_idc(Codec::H264), Some(77));
        let idr = h264_slice(0x65, 7, true, 0);
        let s = p.slice(Codec::H264, &idr).unwrap().unwrap();
        assert_eq!(
            (s.kind, s.idr, s.poc_lsb),
            (Some(SliceKind::I), true, Some(0))
        );
        let b = h264_slice(0x01, 6, false, 5);
        let s = p.slice(Codec::H264, &b).unwrap().unwrap();
        assert_eq!(
            (s.kind, s.poc_anchor, s.poc_lsb),
            (Some(SliceKind::B), false, Some(5))
        );
        assert_eq!(s.max_poc_lsb, 64);
        // Unknown PPS.
        let orphan = W::default().u(8, 0x41).ue(0).ue(5).ue(3).bytes();
        assert!(p.slice(Codec::H264, &orphan).is_err());
        // Type 2: no lsb in the header.
        let mut p2 = Parser::default();
        p2.parameter_set(Codec::H264, &h264_sps(2)).unwrap();
        p2.parameter_set(Codec::H264, &h264_pps()).unwrap();
        let p_slice = W::default().u(8, 0x41).ue(0).ue(5).ue(0).u(4, 1).bytes();
        let s = p2.slice(Codec::H264, &p_slice).unwrap().unwrap();
        assert_eq!((s.kind, s.poc_lsb), (Some(SliceKind::P), None));
    }

    #[test]
    fn poc_msb_wraps_both_ways() {
        let mut t = PocTracker::default();
        assert_eq!(t.poc(0, 16, true, true), 0);
        assert_eq!(t.poc(6, 16, false, true), 6);
        assert_eq!(t.poc(12, 16, false, true), 12);
        // 14 → 2 wraps forward.
        assert_eq!(t.poc(14, 16, false, true), 14);
        assert_eq!(t.poc(2, 16, false, true), 18);
        // A non-anchor picture does not move the reference point.
        assert_eq!(t.poc(4, 16, false, false), 20);
        assert_eq!(t.poc(3, 16, false, true), 19);
    }
}
