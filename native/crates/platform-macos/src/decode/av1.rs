//! AV1 temporal units → what VideoToolbox takes: an `av1C` configuration
//! record for the format description, and the temporal unit as one sample
//! in the ISOBMFF form (every OBU with its size field, no temporal
//! delimiters).
//!
//! AV1 has no out-of-band parameter sets like H.264; the sequence header OBU
//! plays that role. It is tracked like the H.264 SPS: a changed sequence
//! header (new resolution, profile…) needs a new format description.

use platform_api::CodecError;

/// OBU types (AV1 §6.2.2).
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
const OBU_PADDING: u8 = 15;

/// `frame_type` of a key frame (AV1 §6.8.2).
const KEY_FRAME: u32 = 0;

fn corrupt(what: &str) -> CodecError {
    CodecError::Backend(format!(
        "the host's AV1 stream is malformed ({what}); the viewer will ask for a new keyframe"
    ))
}

/// MSB-first bit reader over an OBU payload (AV1 §4.10.2 `f(n)`).
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32, CodecError> {
        let byte = self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| corrupt("sequence header ends early"))?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    /// `f(n)`, n ≤ 32.
    fn bits(&mut self, n: u32) -> Result<u32, CodecError> {
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        u32::try_from(v).map_err(|_| corrupt("field wider than 32 bits"))
    }

    fn flag(&mut self) -> Result<bool, CodecError> {
        Ok(self.bit()? == 1)
    }

    /// `uvlc()` (AV1 §4.10.3).
    fn uvlc(&mut self) -> Result<u32, CodecError> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
        }
        if leading >= 32 {
            return Ok(u32::MAX);
        }
        Ok(self.bits(leading)? + ((1u32 << leading) - 1))
    }
}

/// `leb128()` (AV1 §4.10.5): at most 8 bytes, value below 2^32. Returns the
/// value and the number of bytes read.
fn read_leb128(data: &[u8]) -> Result<(usize, usize), CodecError> {
    let mut value: u64 = 0;
    for (i, &byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            let value = usize::try_from(value)
                .ok()
                .filter(|&v| v <= u32::MAX as usize)
                .ok_or_else(|| corrupt("OBU size out of range"))?;
            return Ok((value, i + 1));
        }
    }
    Err(corrupt("unterminated OBU size"))
}

fn write_leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One OBU of a temporal unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Obu<'a> {
    pub kind: u8,
    /// `obu_header()`: 1 byte, or 2 with the extension.
    header: &'a [u8],
    pub payload: &'a [u8],
}

impl Obu<'_> {
    /// Appends the OBU with `obu_has_size_field = 1`, which ISOBMFF requires
    /// of every OBU in a sample and in `configOBUs` (AV1-ISOBMFF §2.3.4, §2.4).
    fn write_sized(&self, out: &mut Vec<u8>) {
        out.push(self.header[0] | 0b0000_0010);
        out.extend_from_slice(&self.header[1..]);
        write_leb128(self.payload.len(), out);
        out.extend_from_slice(self.payload);
    }
}

/// Splits a low-overhead bitstream (AV1 §5.2) into OBUs. An OBU without a
/// size field runs to the end of the data, which is only valid for the last.
pub(crate) fn obus(mut data: &[u8]) -> Result<Vec<Obu<'_>>, CodecError> {
    let mut out = Vec::new();
    while let Some(&first) = data.first() {
        if first & 0x80 != 0 {
            return Err(corrupt("forbidden bit set in an OBU header"));
        }
        let kind = (first >> 3) & 0x0f;
        let has_extension = first & 0b0000_0100 != 0;
        let has_size = first & 0b0000_0010 != 0;
        let header_len = if has_extension { 2 } else { 1 };
        let header = data
            .get(..header_len)
            .ok_or_else(|| corrupt("truncated OBU header"))?;
        let rest = &data[header_len..];
        let (size, size_len) = if has_size {
            read_leb128(rest)?
        } else {
            (rest.len(), 0)
        };
        let payload = rest
            .get(size_len..size_len + size)
            .ok_or_else(|| corrupt("OBU larger than the data"))?;
        out.push(Obu {
            kind,
            header,
            payload,
        });
        data = &rest[size_len + size..];
    }
    Ok(out)
}

/// The fields of a sequence header (AV1 §5.5) that the format description
/// and the key-frame check need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceHeader {
    pub profile: u8,
    pub level_idx_0: u8,
    pub tier_0: u8,
    pub reduced_still_picture_header: bool,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono_chrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub max_width: u32,
    pub max_height: u32,
}

impl SequenceHeader {
    /// Parses `sequence_header_obu()` up to and including `color_config()`.
    pub(crate) fn parse(payload: &[u8]) -> Result<Self, CodecError> {
        let mut r = Bits::new(payload);
        let profile = r.bits(3)? as u8;
        if profile > 2 {
            return Err(corrupt("unknown seq_profile"));
        }
        let _still_picture = r.flag()?;
        let reduced = r.flag()?;
        let (level_idx_0, tier_0);
        if reduced {
            level_idx_0 = r.bits(5)? as u8;
            tier_0 = 0;
        } else {
            let mut decoder_model_info_present = false;
            let mut buffer_delay_length = 0;
            if r.flag()? {
                // timing_info()
                r.bits(32)?; // num_units_in_display_tick
                r.bits(32)?; // time_scale
                if r.flag()? {
                    r.uvlc()?; // num_ticks_per_picture_minus_1
                }
                decoder_model_info_present = r.flag()?;
                if decoder_model_info_present {
                    // decoder_model_info()
                    buffer_delay_length = r.bits(5)? + 1;
                    r.bits(32)?; // num_units_in_decoding_tick
                    r.bits(5)?; // buffer_removal_time_length_minus_1
                    r.bits(5)?; // frame_presentation_time_length_minus_1
                }
            }
            let initial_display_delay_present = r.flag()?;
            let operating_points = r.bits(5)? + 1;
            let mut first = None;
            for _ in 0..operating_points {
                r.bits(12)?; // operating_point_idc
                let level = r.bits(5)? as u8;
                let tier = if level > 7 { r.bits(1)? as u8 } else { 0 };
                if decoder_model_info_present && r.flag()? {
                    // operating_parameters_info()
                    r.bits(buffer_delay_length)?; // decoder_buffer_delay
                    r.bits(buffer_delay_length)?; // encoder_buffer_delay
                    r.bits(1)?; // low_delay_mode_flag
                }
                if initial_display_delay_present && r.flag()? {
                    r.bits(4)?; // initial_display_delay_minus_1
                }
                // av1C records operating point 0 (the full stream).
                if first.is_none() {
                    first = Some((level, tier));
                }
            }
            (level_idx_0, tier_0) = first.unwrap_or((0, 0));
        }
        let width_bits = r.bits(4)? + 1;
        let height_bits = r.bits(4)? + 1;
        let max_width = r.bits(width_bits)? + 1;
        let max_height = r.bits(height_bits)? + 1;
        if !reduced && r.flag()? {
            // frame_id_numbers_present_flag
            r.bits(4)?; // delta_frame_id_length_minus_2
            r.bits(3)?; // additional_frame_id_length_minus_1
        }
        r.bits(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
        if !reduced {
            r.bits(4)?; // interintra, masked compound, warped motion, dual filter
            let order_hint = r.flag()?;
            if order_hint {
                r.bits(2)?; // enable_jnt_comp, enable_ref_frame_mvs
            }
            let force_screen_content_tools = if r.flag()? {
                2 // seq_choose_screen_content_tools → SELECT_SCREEN_CONTENT_TOOLS
            } else {
                r.bits(1)?
            };
            if force_screen_content_tools > 0 && !r.flag()? {
                r.bits(1)?; // seq_force_integer_mv
            }
            if order_hint {
                r.bits(3)?; // order_hint_bits_minus_1
            }
        }
        r.bits(3)?; // enable_superres, enable_cdef, enable_restoration

        // color_config() (AV1 §5.5.2)
        let high_bitdepth = r.flag()?;
        let twelve_bit = profile == 2 && high_bitdepth && r.flag()?;
        let mono_chrome = profile != 1 && r.flag()?;
        let (mut cp, mut tc, mut mc) = (2, 2, 2); // *_UNSPECIFIED
        if r.flag()? {
            cp = r.bits(8)?;
            tc = r.bits(8)?;
            mc = r.bits(8)?;
        }
        let (subsampling_x, subsampling_y, mut chroma_sample_position) = if mono_chrome {
            (true, true, 0)
        } else if cp == 1 && tc == 13 && mc == 0 {
            // BT.709 primaries + sRGB transfer + identity matrix: 4:4:4 RGB.
            (false, false, 0)
        } else {
            r.bits(1)?; // color_range
            match profile {
                0 => (true, true, 0),
                1 => (false, false, 0),
                _ if twelve_bit => {
                    let x = r.flag()?;
                    let y = x && r.flag()?;
                    (x, y, 0)
                }
                _ => (true, false, 0),
            }
        };
        if !mono_chrome && subsampling_x && subsampling_y {
            chroma_sample_position = r.bits(2)? as u8;
        }
        Ok(Self {
            profile,
            level_idx_0,
            tier_0,
            reduced_still_picture_header: reduced,
            high_bitdepth,
            twelve_bit,
            mono_chrome,
            subsampling_x,
            subsampling_y,
            chroma_sample_position,
            max_width,
            max_height,
        })
    }

    /// The `AV1CodecConfigurationRecord` (AV1-ISOBMFF §2.3.3) carrying
    /// `config_obu` (the sequence header OBU, size field set) as `configOBUs`.
    /// VideoToolbox takes it as the `av1C` sample description extension atom.
    pub(crate) fn av1c(&self, config_obu: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + config_obu.len());
        out.push(0x81); // marker = 1, version = 1
        out.push(self.profile << 5 | (self.level_idx_0 & 0x1f));
        out.push(
            self.tier_0 << 7
                | u8::from(self.high_bitdepth) << 6
                | u8::from(self.twelve_bit) << 5
                | u8::from(self.mono_chrome) << 4
                | u8::from(self.subsampling_x) << 3
                | u8::from(self.subsampling_y) << 2
                | (self.chroma_sample_position & 0b11),
        );
        // reserved = 0, initial_presentation_delay_present = 0
        out.push(0);
        out.extend_from_slice(config_obu);
        out
    }
}

/// One temporal unit, split for VideoToolbox.
#[derive(Debug, Default)]
pub(crate) struct Split {
    /// The last sequence header of the unit, parsed and as an OBU with its
    /// size field set.
    pub sequence_header: Option<(SequenceHeader, Vec<u8>)>,
    /// True when the unit holds a key frame that is shown or decoded now.
    pub random_access: bool,
}

/// Splits temporal unit `tu` and writes its OBUs to `sample` (cleared first)
/// in the ISOBMFF sample form: size fields everywhere, temporal delimiters
/// and padding dropped (AV1-ISOBMFF §2.4). The sequence header stays in the
/// sample as well — a repeated identical one is a no-op for the decoder.
///
/// `current` is the sequence header in force, needed to read the frame
/// header when this unit does not carry one.
pub(crate) fn split_into_sample(
    tu: &[u8],
    current: Option<&SequenceHeader>,
    sample: &mut Vec<u8>,
) -> Result<Split, CodecError> {
    sample.clear();
    let mut split = Split::default();
    let mut header = current.copied();
    for obu in obus(tu)? {
        match obu.kind {
            OBU_TEMPORAL_DELIMITER | OBU_PADDING => continue,
            OBU_SEQUENCE_HEADER => {
                let parsed = SequenceHeader::parse(obu.payload)?;
                let mut sized = Vec::with_capacity(obu.payload.len() + 4);
                obu.write_sized(&mut sized);
                header = Some(parsed);
                split.sequence_header = Some((parsed, sized));
            }
            OBU_FRAME_HEADER | OBU_FRAME => {
                if let Some(h) = &header {
                    split.random_access |= is_key_frame(h, obu.payload)?;
                }
            }
            _ => {}
        }
        obu.write_sized(sample);
    }
    Ok(split)
}

/// True when a frame header starts a new key frame (AV1 §5.9.2:
/// `show_existing_frame`, then `frame_type`).
fn is_key_frame(seq: &SequenceHeader, frame_header: &[u8]) -> Result<bool, CodecError> {
    if seq.reduced_still_picture_header {
        // A still picture: every frame is a key frame.
        return Ok(true);
    }
    let mut r = Bits::new(frame_header);
    if r.flag()? {
        // show_existing_frame: re-shows a decoded frame, not an entry point.
        return Ok(false);
    }
    Ok(r.bits(2)? == KEY_FRAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::synth::BitWriter;

    /// A 4:2:0 8-bit main-profile sequence header with one operating point.
    fn sequence_header(width: u32, height: u32, level: u64) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(3, 0); // seq_profile
        w.put(1, 0); // still_picture
        w.put(1, 0); // reduced_still_picture_header
        w.put(1, 1); // timing_info_present_flag
        w.put(32, 1); // num_units_in_display_tick
        w.put(32, 60); // time_scale
        w.put(1, 1); // equal_picture_interval
        w.put(1, 1); // uvlc(0) = "1"
        w.put(1, 1); // decoder_model_info_present_flag
        w.put(5, 9); // buffer_delay_length_minus_1 → 10 bits
        w.put(32, 1); // num_units_in_decoding_tick
        w.put(5, 4); // buffer_removal_time_length_minus_1
        w.put(5, 4); // frame_presentation_time_length_minus_1
        w.put(1, 1); // initial_display_delay_present_flag
        w.put(5, 0); // operating_points_cnt_minus_1
        w.put(12, 0); // operating_point_idc
        w.put(5, level); // seq_level_idx
        if level > 7 {
            w.put(1, 1); // seq_tier
        }
        w.put(1, 1); // decoder_model_present_for_this_op
        w.put(10, 5); // decoder_buffer_delay
        w.put(10, 6); // encoder_buffer_delay
        w.put(1, 1); // low_delay_mode_flag
        w.put(1, 1); // initial_display_delay_present_for_this_op
        w.put(4, 3); // initial_display_delay_minus_1
        w.put(4, 15); // frame_width_bits_minus_1
        w.put(4, 15); // frame_height_bits_minus_1
        w.put(16, u64::from(width - 1));
        w.put(16, u64::from(height - 1));
        w.put(1, 0); // frame_id_numbers_present_flag
        w.put(3, 0b011); // 128x128 sb = 0, filter intra = 1, intra edge = 1
        w.put(4, 0); // interintra, masked, warped, dual filter
        w.put(1, 1); // enable_order_hint
        w.put(2, 0); // jnt_comp, ref_frame_mvs
        w.put(1, 0); // seq_choose_screen_content_tools
        w.put(1, 1); // seq_force_screen_content_tools
        w.put(1, 0); // seq_choose_integer_mv
        w.put(1, 0); // seq_force_integer_mv
        w.put(3, 6); // order_hint_bits_minus_1
        w.put(3, 0b010); // superres, cdef, restoration
        w.put(1, 0); // high_bitdepth
        w.put(1, 0); // mono_chrome
        w.put(1, 1); // color_description_present_flag
        w.put(8, 1); // BT.709
        w.put(8, 1); // BT.709
        w.put(8, 1); // BT.709
        w.put(1, 0); // color_range
        w.put(2, 1); // chroma_sample_position = VERTICAL
        w.put(1, 0); // separate_uv_delta_q
        w.put(1, 0); // film_grain_params_present
        w.put(1, 1); // trailing one bit
        w.finish()
    }

    fn obu(kind: u8, payload: &[u8], sized: bool) -> Vec<u8> {
        let mut out = vec![kind << 3 | if sized { 0b10 } else { 0 }];
        if sized {
            write_leb128(payload.len(), &mut out);
        }
        out.extend_from_slice(payload);
        out
    }

    /// `show_existing_frame = 0`, `frame_type`, then filler.
    fn frame_header(frame_type: u64) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(1, 0);
        w.put(2, frame_type);
        w.put(5, 0b10101);
        w.put(8, 0x5a);
        w.finish()
    }

    #[test]
    fn leb128_round_trips_and_rejects_garbage() {
        for v in [0usize, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as usize] {
            let mut out = Vec::new();
            write_leb128(v, &mut out);
            assert_eq!(read_leb128(&out).ok(), Some((v, out.len())), "{v}");
        }
        assert!(read_leb128(&[0x80; 9]).is_err());
        assert!(read_leb128(&[0x80, 0x80]).is_err());
        // 2^32 does not fit.
        assert!(read_leb128(&[0x80, 0x80, 0x80, 0x80, 0x10]).is_err());
    }

    #[test]
    fn parses_a_sequence_header_with_timing_and_decoder_model() {
        let payload = sequence_header(1920, 1080, 8);
        let h = SequenceHeader::parse(&payload).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            h,
            SequenceHeader {
                profile: 0,
                level_idx_0: 8,
                tier_0: 1,
                reduced_still_picture_header: false,
                high_bitdepth: false,
                twelve_bit: false,
                mono_chrome: false,
                subsampling_x: true,
                subsampling_y: true,
                chroma_sample_position: 1,
                max_width: 1920,
                max_height: 1080,
            }
        );
        // A truncated header is an error, not a panic.
        assert!(SequenceHeader::parse(&payload[..10]).is_err());
    }

    #[test]
    fn av1c_record_layout() {
        let payload = sequence_header(1280, 720, 5);
        let h = SequenceHeader::parse(&payload).unwrap_or_else(|e| panic!("{e}"));
        let config = obu(OBU_SEQUENCE_HEADER, &payload, true);
        let record = h.av1c(&config);
        // marker|version, profile 0 / level 5, tier 0 / 8-bit / 4:2:0 / csp 1.
        assert_eq!(&record[..4], &[0x81, 0x05, 0b0000_1101, 0x00]);
        assert_eq!(&record[4..], &config[..]);
    }

    #[test]
    fn sample_drops_delimiters_adds_sizes_and_finds_key_frames() {
        let seq = sequence_header(640, 480, 4);
        let key = frame_header(0);
        let inter = frame_header(1);
        // TD, sequence header and a key frame; the last OBU has no size field.
        let tu = [
            obu(OBU_TEMPORAL_DELIMITER, &[], true),
            obu(OBU_SEQUENCE_HEADER, &seq, true),
            obu(OBU_FRAME, &key, false),
        ]
        .concat();
        let mut sample = Vec::new();
        let split = split_into_sample(&tu, None, &mut sample).unwrap_or_else(|e| panic!("{e}"));
        assert!(split.random_access);
        let current = SequenceHeader::parse(&seq).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            split.sequence_header,
            Some((current, obu(OBU_SEQUENCE_HEADER, &seq, true)))
        );
        assert_eq!(
            sample,
            [
                obu(OBU_SEQUENCE_HEADER, &seq, true),
                obu(OBU_FRAME, &key, true)
            ]
            .concat()
        );

        // An inter frame: no sequence header, judged with the current one.
        let tu = [
            obu(OBU_TEMPORAL_DELIMITER, &[], true),
            obu(OBU_FRAME, &inter, true),
            obu(OBU_PADDING, &[0, 0, 0], true),
        ]
        .concat();
        let split =
            split_into_sample(&tu, Some(&current), &mut sample).unwrap_or_else(|e| panic!("{e}"));
        assert!(!split.random_access);
        assert!(split.sequence_header.is_none());
        assert_eq!(sample, obu(OBU_FRAME, &inter, true));
    }

    #[test]
    fn malformed_obus_are_errors() {
        assert!(obus(&[0x80]).is_err()); // forbidden bit
        assert!(obus(&[0x0a, 0x05, 1, 2]).is_err()); // size beyond the data
        assert!(obus(&[0x0e]).is_err()); // extension byte missing
        assert_eq!(obus(&[]).map(|o| o.len()).ok(), Some(0));
    }

    #[test]
    fn uvlc_decodes_exp_golomb_style_codes() {
        // "1" = 0, "010" = 1, "011" = 2, "00100" = 3
        let mut w = BitWriter::default();
        for (n, v) in [(1, 1), (3, 0b010), (3, 0b011), (5, 0b00100)] {
            w.put(n, v);
        }
        let bytes = w.finish();
        let mut r = Bits::new(&bytes);
        for expected in [0, 1, 2, 3] {
            assert_eq!(r.uvlc().ok(), Some(expected));
        }
    }
}
