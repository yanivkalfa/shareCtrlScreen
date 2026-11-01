//! VideoToolbox output → the Annex-B access units `platform-api` promises.
//!
//! A `VTCompressionSession` delivers each picture as AVCC/HVCC sample data:
//! every NAL unit is preceded by a big-endian length field (normally 4 bytes;
//! the size is in the format description) and the parameter sets live only in
//! that format description, never in the samples. The transport and every
//! decoder in this project expect Annex-B (start codes) with the parameter
//! sets in-band on every keyframe, because a viewer may join on any keyframe.
//!
//! Pure byte handling, no OS calls; the OS side (`vt_encoder`) reads the
//! length size and parameter sets out of the `CMFormatDescription`.
//!
//! ## References
//! * ISO/IEC 14496-15 §5.3.2 (AVCDecoderConfigurationRecord:
//!   `lengthSizeMinusOne` ∈ {0, 1, 3}) and §8.3.3 (HEVCDecoderConfigurationRecord).
//! * ITU-T H.264 (08/2021) Annex B (byte stream) and Table 7-1 (`nal_unit_type`:
//!   5 IDR, 7 SPS, 8 PPS).
//! * ITU-T H.265 (09/2023) Annex B and Table 7-1 (16–23 IRAP, 32 VPS, 33 SPS,
//!   34 PPS).
//! * `CMVideoFormatDescriptionGetH264ParameterSetAtIndex` ("The parameter set
//!   NAL units returned will already have any emulation prevention bytes
//!   needed"): <https://developer.apple.com/documentation/coremedia/cmvideoformatdescriptiongeth264parametersetatindex(_:parametersetindex:parametersetpointerout:parametersetsizeout:parametersetcountout:nalunitheaderlengthout:)>

use std::fmt;

use platform_api::Codec;

/// The 4-byte Annex-B start code. Four bytes rather than three before every
/// NAL: some decoders (Media Foundation among them) look for the long form
/// at the start of an access unit, and it costs one byte per NAL.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Why a sample could not be read as length-prefixed NAL units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AvccError {
    /// The format description declared a length-field size other than 1, 2
    /// or 4 bytes.
    LengthSize(usize),
    /// A length field or NAL unit runs past the end of the sample.
    Truncated { offset: usize },
    /// A zero-length NAL unit (would become two adjacent start codes).
    EmptyNal { offset: usize },
    /// The sample held no NAL unit at all.
    NoNal,
}

impl fmt::Display for AvccError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AvccError::LengthSize(n) => {
                write!(f, "unsupported NAL length-field size of {n} bytes")
            }
            AvccError::Truncated { offset } => {
                write!(
                    f,
                    "NAL unit at byte {offset} runs past the end of the sample"
                )
            }
            AvccError::EmptyNal { offset } => write!(f, "empty NAL unit at byte {offset}"),
            AvccError::NoNal => f.write_str("the sample holds no NAL unit"),
        }
    }
}

/// The NAL units of one length-prefixed access unit, borrowed from the sample.
#[derive(Debug)]
pub(crate) struct Nals<'a> {
    units: Vec<&'a [u8]>,
}

impl<'a> Nals<'a> {
    /// Split `sample` into its NAL units. `length_size` is the format
    /// description's `NALUnitHeaderLength`.
    pub(crate) fn parse(sample: &'a [u8], length_size: usize) -> Result<Self, AvccError> {
        if !matches!(length_size, 1 | 2 | 4) {
            return Err(AvccError::LengthSize(length_size));
        }
        let mut units = Vec::new();
        let mut pos = 0;
        while pos < sample.len() {
            let field = sample
                .get(pos..pos + length_size)
                .ok_or(AvccError::Truncated { offset: pos })?;
            let len = field.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
            if len == 0 {
                return Err(AvccError::EmptyNal { offset: pos });
            }
            let start = pos + length_size;
            let nal = start
                .checked_add(len)
                .and_then(|end| sample.get(start..end))
                .ok_or(AvccError::Truncated { offset: pos })?;
            units.push(nal);
            pos = start + len;
        }
        if units.is_empty() {
            return Err(AvccError::NoNal);
        }
        Ok(Self { units })
    }

    /// True when the access unit is a random-access point: an IDR picture
    /// (H.264) or an IRAP picture (HEVC). Read from the bitstream rather than
    /// from the sample's `NotSync` attachment so the flag the viewer acts on
    /// always agrees with what it will actually decode.
    pub(crate) fn is_keyframe(&self, codec: Codec) -> bool {
        self.types(codec).any(|t| is_random_access(codec, t))
    }

    /// True when every parameter set a decoder needs is already in-band.
    pub(crate) fn has_parameter_sets(&self, codec: Codec) -> bool {
        let present: Vec<u8> = self.types(codec).collect();
        parameter_set_types(codec)
            .iter()
            .all(|t| present.contains(t))
    }

    /// The Annex-B access unit: `prefix` NAL units (the parameter sets from
    /// the format description, already escaped) and then this sample's NAL
    /// units, each behind a 4-byte start code.
    pub(crate) fn to_annexb(&self, prefix: &[&[u8]]) -> Vec<u8> {
        let all = || prefix.iter().copied().chain(self.units.iter().copied());
        let size: usize = all().map(|n| START_CODE.len() + n.len()).sum();
        let mut out = Vec::with_capacity(size);
        for nal in all() {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out
    }

    fn types(&self, codec: Codec) -> impl Iterator<Item = u8> + '_ {
        self.units.iter().filter_map(move |n| nal_type(codec, n))
    }
}

/// `nal_unit_type` of a NAL unit (5 bits for H.264, 6 bits for HEVC).
fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    match codec {
        Codec::H264 => Some(first & 0x1f),
        Codec::Hevc => Some((first >> 1) & 0x3f),
        Codec::Av1 => None,
    }
}

fn is_random_access(codec: Codec, nal_type: u8) -> bool {
    match codec {
        Codec::H264 => nal_type == 5,
        // BLA_W_LP … RSV_IRAP_VCL23: every IRAP picture. No reordering means
        // no leading pictures, so a CRA is as good a join point as an IDR.
        Codec::Hevc => (16..=23).contains(&nal_type),
        Codec::Av1 => false,
    }
}

/// NAL types of the parameter sets a decoder needs before a keyframe.
fn parameter_set_types(codec: Codec) -> &'static [u8] {
    match codec {
        Codec::H264 => &[7, 8],
        Codec::Hevc => &[32, 33, 34],
        Codec::Av1 => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Length-prefix `nals` with `size`-byte big-endian fields.
    fn avcc(size: usize, nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            let len = (n.len() as u32).to_be_bytes();
            out.extend_from_slice(&len[4 - size..]);
            out.extend_from_slice(n);
        }
        out
    }

    const SPS: &[u8] = &[0x67, 0x4d, 0x40, 0x29];
    const PPS: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0x41, 0x9a, 0x02];
    const SEI: &[u8] = &[0x06, 0x05, 0x01];

    #[test]
    fn idr_gets_parameter_sets_in_front_with_start_codes() {
        let sample = avcc(4, &[IDR]);
        let nals = Nals::parse(&sample, 4).expect("parse");
        assert!(nals.is_keyframe(Codec::H264));
        assert!(!nals.has_parameter_sets(Codec::H264));
        let au = nals.to_annexb(&[SPS, PPS]);
        let mut want = Vec::new();
        for n in [SPS, PPS, IDR] {
            want.extend_from_slice(&[0, 0, 0, 1]);
            want.extend_from_slice(n);
        }
        assert_eq!(au, want);
    }

    #[test]
    fn delta_frames_are_not_keyframes_and_keep_every_nal() {
        let sample = avcc(4, &[SEI, P]);
        let nals = Nals::parse(&sample, 4).expect("parse");
        assert!(!nals.is_keyframe(Codec::H264));
        assert_eq!(
            nals.to_annexb(&[]),
            [&[0, 0, 0, 1][..], SEI, &[0, 0, 0, 1], P].concat()
        );
    }

    #[test]
    fn in_band_parameter_sets_are_detected() {
        let sample = avcc(4, &[SPS, PPS, IDR]);
        let nals = Nals::parse(&sample, 4).expect("parse");
        assert!(nals.has_parameter_sets(Codec::H264));
        // Only SPS: still missing the PPS.
        let sample = avcc(4, &[SPS, IDR]);
        assert!(!Nals::parse(&sample, 4)
            .expect("parse")
            .has_parameter_sets(Codec::H264));
    }

    #[test]
    fn short_length_fields_are_supported() {
        for size in [1, 2] {
            let sample = avcc(size, &[IDR, P]);
            let nals = Nals::parse(&sample, size).expect("parse");
            assert_eq!(
                nals.to_annexb(&[]),
                [&[0, 0, 0, 1][..], IDR, &[0, 0, 0, 1], P].concat()
            );
        }
    }

    #[test]
    fn hevc_irap_and_parameter_sets() {
        let vps: &[u8] = &[0x40, 0x01, 0x0c];
        let sps: &[u8] = &[0x42, 0x01, 0x01];
        let pps: &[u8] = &[0x44, 0x01, 0xc1];
        let idr_w_radl: &[u8] = &[0x26, 0x01, 0xaf];
        let cra: &[u8] = &[0x2a, 0x01, 0xaf];
        let trail_r: &[u8] = &[0x02, 0x01, 0xd0];
        let key = avcc(4, &[idr_w_radl]);
        let key = Nals::parse(&key, 4).expect("parse");
        assert!(key.is_keyframe(Codec::Hevc));
        assert!(!key.has_parameter_sets(Codec::Hevc));
        let cra = avcc(4, &[cra]);
        assert!(Nals::parse(&cra, 4)
            .expect("parse")
            .is_keyframe(Codec::Hevc));
        let delta = avcc(4, &[trail_r]);
        assert!(!Nals::parse(&delta, 4)
            .expect("parse")
            .is_keyframe(Codec::Hevc));
        let full = avcc(4, &[vps, sps, pps, idr_w_radl]);
        assert!(Nals::parse(&full, 4)
            .expect("parse")
            .has_parameter_sets(Codec::Hevc));
        // The H.264 reading of an HEVC IDR header byte (0x26 & 0x1f = 6, SEI)
        // must not be mistaken for a keyframe.
        assert!(!key.is_keyframe(Codec::H264));
    }

    #[test]
    fn malformed_samples_are_refused_not_sliced() {
        assert_eq!(
            Nals::parse(&[0, 0, 0, 1, 0x65], 3).err(),
            Some(AvccError::LengthSize(3))
        );
        // Declares 9 bytes, has 2.
        assert_eq!(
            Nals::parse(&[0, 0, 0, 9, 0x65, 0x88], 4).err(),
            Some(AvccError::Truncated { offset: 0 })
        );
        // Length field itself cut short after a good NAL.
        assert_eq!(
            Nals::parse(&[0, 0, 0, 1, 0x65, 0, 0], 4).err(),
            Some(AvccError::Truncated { offset: 5 })
        );
        assert_eq!(
            Nals::parse(&[0, 0, 0, 0, 0x65], 4).err(),
            Some(AvccError::EmptyNal { offset: 0 })
        );
        assert_eq!(Nals::parse(&[], 4).err(), Some(AvccError::NoNal));
        // A huge declared length must not overflow the bounds arithmetic.
        assert_eq!(
            Nals::parse(&[0xff, 0xff, 0xff, 0xff, 0x65], 4).err(),
            Some(AvccError::Truncated { offset: 0 })
        );
    }

    #[test]
    fn errors_read_as_sentences() {
        assert!(AvccError::Truncated { offset: 7 }
            .to_string()
            .contains("byte 7"));
    }
}
