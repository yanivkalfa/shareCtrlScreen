//! Annex-B access units → what VideoToolbox takes: parameter sets for the
//! format description, and the remaining NAL units as one AVCC sample
//! (4-byte big-endian length prefixes, ISO/IEC 14496-15 §5.3.2 / §8.3.2).
//!
//! The transport carries Annex-B (`platform_api::EncodedUnit`), and every
//! keyframe repeats its parameter sets in-band because a viewer may join on
//! any keyframe. VideoToolbox wants them out of band, in a
//! `CMVideoFormatDescription`; [`ParameterSets`] keeps the current ones and
//! says when they changed, which is how a mid-stream resolution change (the
//! host follows the viewer's window size) is detected.

use platform_api::{Codec, CodecError};

/// AVCC length prefix size; also passed to CoreMedia as the
/// `NALUnitHeaderLength` of the format description.
pub(crate) const LENGTH_SIZE: usize = 4;

/// Iterates the NAL units of an Annex-B byte stream, yielding each payload
/// (header byte(s) included, start code excluded). Start codes may be 3 or 4
/// bytes; zero bytes before a start code belong to it (`trailing_zero_8bits`),
/// not to the NAL before it.
pub(crate) fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    let mut pos = next_start(stream, 0);
    std::iter::from_fn(move || {
        let start = pos?;
        let next = next_start(stream, start);
        let end = match next {
            // `next` points past the next start code; walk back over it and any
            // zero padding of the previous NAL.
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
    // WHY a plain scan is correct: emulation prevention guarantees that no
    // `00 00 01` appears inside a NAL payload (H.264 §7.4.1, H.265 §7.4.2).
    stream
        .get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|i| from + i + 3)
}

/// What a NAL unit is to the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NalRole {
    /// A parameter set; the index orders the kinds the way CoreMedia lists
    /// them (H.264: SPS, PPS; HEVC: VPS, SPS, PPS).
    ParameterSet(usize),
    /// A slice of a random-access picture: decoding can start here.
    RandomAccess,
    /// Anything else (other slices, SEI, access unit delimiters).
    Other,
}

/// Classifies a NAL unit of `codec` (H.264 Table 7-1, H.265 Table 7-1).
/// `None` for an empty NAL or a codec without NAL units (AV1).
pub(crate) fn role(codec: Codec, nal: &[u8]) -> Option<NalRole> {
    let first = *nal.first()?;
    let role = match codec {
        Codec::H264 => match first & 0x1f {
            7 => NalRole::ParameterSet(0),
            8 => NalRole::ParameterSet(1),
            5 => NalRole::RandomAccess,
            _ => NalRole::Other,
        },
        Codec::Hevc => match (first >> 1) & 0x3f {
            32 => NalRole::ParameterSet(0),
            33 => NalRole::ParameterSet(1),
            34 => NalRole::ParameterSet(2),
            // IRAP: BLA_W_LP … CRA_NUT. The encoder never reorders, so a CRA
            // has no leading pictures and is as good an entry point as an IDR.
            16..=21 => NalRole::RandomAccess,
            _ => NalRole::Other,
        },
        Codec::Av1 => return None,
    };
    Some(role)
}

/// Number of parameter-set kinds `codec` needs before its first picture.
fn kinds(codec: Codec) -> usize {
    match codec {
        Codec::H264 => 2,
        Codec::Hevc => 3,
        Codec::Av1 => 0,
    }
}

/// One access unit, split for VideoToolbox.
#[derive(Debug, Default)]
pub(crate) struct Split<'a> {
    /// Parameter-set NAL units, in stream order.
    pub parameter_sets: Vec<&'a [u8]>,
    /// True when a slice of a random-access picture is present.
    pub random_access: bool,
}

/// Splits `au` and writes every NAL unit that is **not** a parameter set to
/// `sample` (cleared first) in AVCC form. Parameter sets go into the format
/// description instead: CoreMedia builds an `avc1`/`hvc1` description from
/// them, whose samples carry none in-band (ISO/IEC 14496-15 §5.4.2.1), and
/// leaving them out keeps the sample the same whatever the encoder repeats.
pub(crate) fn split_into_avcc<'a>(
    codec: Codec,
    au: &'a [u8],
    sample: &mut Vec<u8>,
) -> Result<Split<'a>, CodecError> {
    sample.clear();
    let mut split = Split::default();
    for nal in nal_units(au) {
        match role(codec, nal) {
            Some(NalRole::ParameterSet(_)) => split.parameter_sets.push(nal),
            Some(r) => {
                split.random_access |= r == NalRole::RandomAccess;
                let len = u32::try_from(nal.len()).map_err(|_| {
                    CodecError::Backend(format!(
                        "a {} MiB NAL unit is larger than any {codec:?} stream carries; \
                         the host sent corrupt data",
                        nal.len() >> 20
                    ))
                })?;
                sample.extend_from_slice(&len.to_be_bytes());
                sample.extend_from_slice(nal);
            }
            None => {}
        }
    }
    Ok(split)
}

/// The parameter sets the current format description was built from.
///
/// Each access unit that carries parameter sets replaces, kind by kind, the
/// ones it carries (all its SPSs, all its PPSs, …) — so several ids of one
/// kind in one unit are kept together, and a unit that only refreshes the PPS
/// keeps the previous SPS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParameterSets {
    codec: Codec,
    /// Indexed by the [`NalRole::ParameterSet`] kind.
    by_kind: Vec<Vec<Vec<u8>>>,
}

impl ParameterSets {
    pub(crate) fn new(codec: Codec) -> Self {
        Self {
            codec,
            by_kind: vec![Vec::new(); kinds(codec)],
        }
    }

    /// Takes the parameter sets of one access unit. Returns true when the
    /// tracked set changed, i.e. a new format description is needed. A
    /// keyframe that repeats identical parameter sets returns false, so the
    /// running session is kept.
    pub(crate) fn absorb(&mut self, nals: &[&[u8]]) -> bool {
        let mut fresh: Vec<Option<Vec<Vec<u8>>>> = vec![None; self.by_kind.len()];
        for nal in nals {
            if let Some(NalRole::ParameterSet(kind)) = role(self.codec, nal) {
                if let Some(slot) = fresh.get_mut(kind) {
                    slot.get_or_insert_with(Vec::new).push(nal.to_vec());
                }
            }
        }
        let mut changed = false;
        for (current, new) in self.by_kind.iter_mut().zip(fresh) {
            if let Some(new) = new {
                if *current != new {
                    *current = new;
                    changed = true;
                }
            }
        }
        changed
    }

    /// True once every kind the codec needs has been seen.
    pub(crate) fn complete(&self) -> bool {
        self.by_kind.iter().all(|sets| !sets.is_empty())
    }

    /// All parameter sets, kinds in CoreMedia's order.
    pub(crate) fn all(&self) -> Vec<&[u8]> {
        self.by_kind.iter().flatten().map(Vec::as_slice).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e];
    const SPS2: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80];
    const PPS2: &[u8] = &[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x21];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];
    const AUD: &[u8] = &[0, 0, 0, 1, 0x09, 0xf0];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let au = cat(&[SPS, PPS, IDR]);
        let nals: Vec<&[u8]> = nal_units(&au).collect();
        assert_eq!(nals, vec![&SPS[4..], &PPS[4..], &IDR[3..]]);
    }

    #[test]
    fn zero_padding_before_a_start_code_is_not_payload() {
        let au = cat(&[&[0, 0, 1, 0x67, 0x42, 0, 0], &[0, 0, 1, 0x68, 0xce]]);
        let nals: Vec<&[u8]> = nal_units(&au).collect();
        assert_eq!(nals, vec![&[0x67u8, 0x42][..], &[0x68, 0xce][..]]);
        assert!(nal_units(&[]).next().is_none());
        assert!(nal_units(&[0, 0, 0]).next().is_none());
        assert_eq!(nal_units(&cat(&[&[0xff, 0xfe], P])).count(), 1);
    }

    #[test]
    fn annex_b_becomes_length_prefixed_without_parameter_sets() {
        let au = cat(&[AUD, SPS, PPS, IDR]);
        let mut sample = vec![0xaa; 3];
        let split =
            split_into_avcc(Codec::H264, &au, &mut sample).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(split.parameter_sets, vec![&SPS[4..], &PPS[4..]]);
        assert!(split.random_access);
        // The scratch buffer is reset, then AUD and IDR follow with lengths.
        let expected = cat(&[
            &[0, 0, 0, 2, 0x09, 0xf0],
            &[0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x21],
        ]);
        assert_eq!(sample, expected);

        let split = split_into_avcc(Codec::H264, P, &mut sample).unwrap_or_else(|e| panic!("{e}"));
        assert!(split.parameter_sets.is_empty());
        assert!(!split.random_access);
        assert_eq!(sample, vec![0, 0, 0, 3, 0x41, 0x9a, 0x02]);
    }

    #[test]
    fn hevc_roles_and_coremedia_order() {
        // VPS(32)=0x40, SPS(33)=0x42, PPS(34)=0x44, IDR_W_RADL(19)=0x26,
        // CRA(21)=0x2a, TRAIL_R(1)=0x02.
        let vps: &[u8] = &[0x40, 0x01, 0x0c];
        let sps: &[u8] = &[0x42, 0x01, 0x01];
        let pps: &[u8] = &[0x44, 0x01, 0xc1];
        assert_eq!(role(Codec::Hevc, vps), Some(NalRole::ParameterSet(0)));
        assert_eq!(role(Codec::Hevc, sps), Some(NalRole::ParameterSet(1)));
        assert_eq!(role(Codec::Hevc, pps), Some(NalRole::ParameterSet(2)));
        assert_eq!(
            role(Codec::Hevc, &[0x26, 0x01]),
            Some(NalRole::RandomAccess)
        );
        assert_eq!(
            role(Codec::Hevc, &[0x2a, 0x01]),
            Some(NalRole::RandomAccess)
        );
        assert_eq!(role(Codec::Hevc, &[0x02, 0x01]), Some(NalRole::Other));
        assert_eq!(role(Codec::Av1, &[0x12, 0x00]), None);
        assert_eq!(role(Codec::H264, &[]), None);

        let mut ps = ParameterSets::new(Codec::Hevc);
        // Out-of-order arrival is put back into VPS, SPS, PPS order.
        assert!(ps.absorb(&[pps, sps, vps]));
        assert!(ps.complete());
        assert_eq!(ps.all(), vec![vps, sps, pps]);
    }

    #[test]
    fn parameter_set_tracking_detects_changes_only() {
        let mut ps = ParameterSets::new(Codec::H264);
        assert!(!ps.complete());
        assert!(!ps.absorb(&[]));

        // First keyframe: new.
        assert!(ps.absorb(&[&SPS[4..], &PPS[4..]]));
        assert!(ps.complete());
        // Every later keyframe repeats them: unchanged, the session is kept.
        assert!(!ps.absorb(&[&SPS[4..], &PPS[4..]]));

        // A resolution change brings a new SPS.
        assert!(ps.absorb(&[&SPS2[4..], &PPS[4..]]));
        assert_eq!(ps.all(), vec![&SPS2[4..], &PPS[4..]]);

        // A PPS-only refresh keeps the SPS.
        assert!(ps.absorb(&[&PPS2[4..]]));
        assert_eq!(ps.all(), vec![&SPS2[4..], &PPS2[4..]]);

        // Two PPS ids in one unit are both kept, in order.
        assert!(ps.absorb(&[&PPS[4..], &PPS2[4..]]));
        assert_eq!(ps.all(), vec![&SPS2[4..], &PPS[4..], &PPS2[4..]]);
    }

    #[test]
    fn half_a_parameter_set_pair_is_not_complete() {
        let mut ps = ParameterSets::new(Codec::H264);
        assert!(ps.absorb(&[&SPS[4..]]));
        assert!(!ps.complete());
        // Non-parameter-set NALs are ignored.
        assert!(!ps.absorb(&[&IDR[3..]]));
        assert!(ps.absorb(&[&PPS[4..]]));
        assert!(ps.complete());
    }
}
