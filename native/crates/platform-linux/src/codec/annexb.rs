//! Annex-B access-unit helpers shared by the hardware encoders.
//!
//! The `platform-api` contract says every keyframe carries its parameter sets
//! in-band, because a viewer may join on any keyframe. Both backends are asked to
//! emit them (VA-API through packed headers, NVENC through `repeatSPSPPS` +
//! `OUTPUT_SPSPPS`), but drivers differ — some Mesa VA-API drivers ignore packed
//! headers and some write their own — so the access unit is checked after
//! encoding and the cached headers are prepended only when they are missing.
//! Never duplicated: a second SPS that differs from the driver's would be worse
//! than none.

use platform_api::Codec;

/// Iterates the NAL units of an Annex-B byte stream, yielding each payload
/// (header byte(s) included, start code excluded). Start codes may be 3 or 4
/// bytes; trailing zero bytes before a start code belong to it, not the NAL.
pub(crate) fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    let mut pos = next_start(stream, 0);
    std::iter::from_fn(move || {
        let start = pos?;
        let next = next_start(stream, start);
        let end = match next {
            // `next` points past the next start code; walk back over it and any
            // zero bytes that pad the previous NAL (trailing_zero_8bits).
            Some(n) => {
                let mut e = n - 3;
                while e > start && stream[e - 1] == 0 {
                    e -= 1;
                }
                e
            }
            None => stream.len(),
        };
        pos = next;
        Some(&stream[start..end])
    })
    .filter(|nal| !nal.is_empty())
}

/// Index just past the first `00 00 01` at or after `from`.
fn next_start(stream: &[u8], from: usize) -> Option<usize> {
    // WHY a plain scan is correct: emulation prevention guarantees no `00 00 01`
    // (or `00 00 00`) appears inside a NAL payload.
    stream
        .get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|i| from + i + 3)
}

/// The NAL unit type of a payload (`nal_unit_type` for H.264, the 6-bit HEVC one).
fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    match codec {
        Codec::H264 => Some(first & 0x1f),
        Codec::Hevc => Some((first >> 1) & 0x3f),
        Codec::Av1 => None,
    }
}

/// NAL types of the parameter sets a decoder needs before an IDR.
fn parameter_set_types(codec: Codec) -> &'static [u8] {
    match codec {
        // SPS, PPS
        Codec::H264 => &[7, 8],
        // VPS, SPS, PPS
        Codec::Hevc => &[32, 33, 34],
        Codec::Av1 => &[],
    }
}

/// True when `au` contains every parameter set `codec` needs.
pub(crate) fn has_parameter_sets(codec: Codec, au: &[u8]) -> bool {
    let types: Vec<u8> = nal_units(au).filter_map(|n| nal_type(codec, n)).collect();
    parameter_set_types(codec).iter().all(|t| types.contains(t))
}

/// The parameter-set NALs of `au`, in stream order, re-framed with 4-byte
/// start codes — what a later keyframe needs if it arrives without them.
/// (VA-API only: NVENC hands its headers over through an API call.)
#[cfg(feature = "vaapi")]
pub(crate) fn parameter_sets(codec: Codec, au: &[u8]) -> Vec<u8> {
    let types = parameter_set_types(codec);
    let mut out = Vec::new();
    for nal in nal_units(au).filter(|n| nal_type(codec, n).is_some_and(|t| types.contains(&t))) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// True when `au` holds an instantaneous-decoding-refresh picture.
pub(crate) fn is_idr(codec: Codec, au: &[u8]) -> bool {
    nal_units(au).any(|n| match (codec, nal_type(codec, n)) {
        (Codec::H264, Some(t)) => t == 5,
        // IDR_W_RADL, IDR_N_LP
        (Codec::Hevc, Some(t)) => t == 19 || t == 20,
        _ => false,
    })
}

/// A keyframe as the contract wants it: `headers` (Annex-B parameter sets)
/// prepended when `au` lacks them, untouched otherwise.
pub(crate) fn with_parameter_sets(codec: Codec, au: Vec<u8>, headers: &[u8]) -> Vec<u8> {
    if headers.is_empty() || has_parameter_sets(codec, &au) {
        return au;
    }
    let mut out = Vec::with_capacity(headers.len() + au.len());
    out.extend_from_slice(headers);
    out.extend_from_slice(&au);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x4d, 0x40, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let au = cat(&[SPS, PPS, IDR]);
        let nals: Vec<&[u8]> = nal_units(&au).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &SPS[4..]);
        assert_eq!(nals[1], &PPS[4..]);
        // A NAL that legitimately ends in 0x00 bytes loses them only when they
        // are followed by a start code; at the end of the stream they stay.
        assert_eq!(nals[2], &IDR[3..]);
    }

    #[test]
    fn trailing_zero_padding_belongs_to_the_start_code() {
        let au = cat(&[&[0, 0, 1, 0x67, 0x42, 0, 0], &[0, 0, 1, 0x68, 0xce]]);
        let nals: Vec<&[u8]> = nal_units(&au).collect();
        assert_eq!(nals, vec![&[0x67u8, 0x42][..], &[0x68, 0xce][..]]);
    }

    #[test]
    fn garbage_before_the_first_start_code_is_ignored() {
        let au = cat(&[&[0xff, 0xfe], P]);
        assert_eq!(nal_units(&au).count(), 1);
        assert!(nal_units(&[]).next().is_none());
        assert!(nal_units(&[0, 0, 0]).next().is_none());
    }

    #[test]
    fn detects_parameter_sets_and_idr() {
        let key = cat(&[SPS, PPS, IDR]);
        assert!(has_parameter_sets(Codec::H264, &key));
        assert!(is_idr(Codec::H264, &key));
        assert!(!has_parameter_sets(Codec::H264, &cat(&[SPS, IDR])));
        assert!(!is_idr(Codec::H264, P));
    }

    #[test]
    fn prepends_headers_only_when_missing() {
        let headers = cat(&[SPS, PPS]);
        let bare = IDR.to_vec();
        let fixed = with_parameter_sets(Codec::H264, bare, &headers);
        assert_eq!(fixed, cat(&[SPS, PPS, IDR]));
        // Already complete: unchanged, never duplicated.
        let complete = cat(&[SPS, PPS, IDR]);
        assert_eq!(
            with_parameter_sets(Codec::H264, complete.clone(), &headers),
            complete
        );
        // Nothing cached: unchanged.
        assert_eq!(
            with_parameter_sets(Codec::H264, IDR.to_vec(), &[]),
            IDR.to_vec()
        );
    }

    #[cfg(feature = "vaapi")]
    #[test]
    fn extracts_parameter_sets_with_four_byte_start_codes() {
        let three_byte_sps = &[0, 0, 1, 0x67, 0x4d, 0x40, 0x28][..];
        let au = cat(&[three_byte_sps, PPS, IDR]);
        assert_eq!(parameter_sets(Codec::H264, &au), cat(&[SPS, PPS]));
        assert!(parameter_sets(Codec::H264, P).is_empty());
    }

    #[test]
    fn hevc_types() {
        // VPS(32)=0x40, SPS(33)=0x42, PPS(34)=0x44, IDR_W_RADL(19)=0x26.
        let au = cat(&[
            &[0, 0, 0, 1, 0x40, 0x01],
            &[0, 0, 0, 1, 0x42, 0x01],
            &[0, 0, 0, 1, 0x44, 0x01],
            &[0, 0, 0, 1, 0x26, 0x01, 0xaf],
        ]);
        assert!(has_parameter_sets(Codec::Hevc, &au));
        assert!(is_idr(Codec::Hevc, &au));
        assert!(!has_parameter_sets(Codec::Hevc, &au[12..]));
    }
}
