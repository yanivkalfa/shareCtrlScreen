//! The shape every encoder's output must have, because every decoder in the
//! project (and the engine's viewer-join logic) relies on it:
//!
//! * every access unit is Annex-B (starts with a `00 00 01` / `00 00 00 01`
//!   start code) and holds exactly one picture;
//! * the first access unit is an IDR, and the `keyframe` flag is set on
//!   exactly the IDR access units (a viewer joins on any of them);
//! * every IDR access unit carries the parameter sets — SPS + PPS, and the
//!   VPS for HEVC — in front of its first slice (`platform_api::EncodedUnit`);
//! * no reordering: every picture is output in decode order, i.e. the
//!   picture order count strictly increases from one IDR to the next (the
//!   decoders have no reorder buffer, `platform_api::VideoDecoder`). B
//!   slices are accepted as long as that holds — "low-delay B" predicts
//!   only from earlier pictures, adds no delay, and is what some HEVC
//!   encoders use for every inter picture; what breaks a viewer is a
//!   picture shown before one decoded earlier, never the slice type. SP/SI
//!   slices (Extended profile) are rejected: no decoder here supports them.
//!
//! [`check_session`] adds what [`crate::session`] scripts: a keyframe on the
//! first frame and on the forced one, and nowhere else.

use std::collections::BTreeSet;

use platform_api::{Codec, EncodedUnit};

use crate::nal::{nal_type, nal_units, Parser, PocTracker, SliceKind};
use crate::session::Session;

/// What a stream that passed [`check`] contains, for the test log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub access_units: usize,
    /// Indexes of the keyframe (IDR) access units.
    pub keyframes: Vec<usize>,
    /// `profile_idc` of the SPS (H.264: 66 Baseline, 77 Main, 100 High;
    /// HEVC: 1 Main).
    pub profile_idc: Option<u8>,
    pub slice_kinds: BTreeSet<String>,
}

/// Parameter-set NAL types an IDR access unit must carry.
fn parameter_set_types(codec: Codec) -> &'static [u8] {
    match codec {
        Codec::H264 => &[7, 8],
        Codec::Hevc => &[32, 33, 34],
        Codec::Av1 => &[],
    }
}

/// Check `aus` (`(access unit, keyframe flag)` in decode order) against the
/// engine's rules; the first rule broken, with the access unit's index.
pub fn check<'a>(
    codec: Codec,
    aus: impl IntoIterator<Item = (&'a [u8], bool)>,
) -> Result<Shape, String> {
    if codec == Codec::Av1 {
        return Err("AV1 has no Annex-B shape to check".into());
    }
    let mut parser = Parser::default();
    let mut poc = PocTracker::default();
    let mut last_poc: Option<i64> = None;
    let mut shape = Shape {
        access_units: 0,
        keyframes: Vec::new(),
        profile_idc: None,
        slice_kinds: BTreeSet::new(),
    };
    for (i, (au, keyframe)) in aus.into_iter().enumerate() {
        let fail = |what: String| Err(format!("access unit {i}: {what}"));
        if !(au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1])) {
            return fail("does not start with an Annex-B start code".into());
        }
        let mut headers_before_slice = Vec::new();
        let mut pictures = 0;
        let mut idr = None;
        for nal in nal_units(au) {
            if let Err(e) = parser.parameter_set(codec, nal) {
                return fail(format!("parameter set: {e}"));
            }
            let slice = match parser.slice(codec, nal) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    if pictures == 0 {
                        headers_before_slice.extend(nal_type(codec, nal));
                    }
                    continue;
                }
                Err(e) => return fail(format!("slice header: {e}")),
            };
            match idr {
                None => idr = Some(slice.idr),
                Some(prev) if prev != slice.idr => {
                    return fail("mixes IDR and non-IDR slices".into())
                }
                Some(_) => {}
            }
            match slice.kind {
                Some(SliceKind::Switching) => return fail("has an SP/SI slice".into()),
                Some(kind) => {
                    shape.slice_kinds.insert(format!("{kind:?}"));
                }
                None => {}
            }
            if !slice.first_in_picture {
                continue;
            }
            pictures += 1;
            if let Some(lsb) = slice.poc_lsb {
                let p = poc.poc(lsb, slice.max_poc_lsb, slice.idr, slice.poc_anchor);
                if let Some(last) = last_poc.filter(|_| !slice.idr) {
                    if p <= last {
                        return fail(format!(
                            "picture order count {p} after {last}: the encoder reorders"
                        ));
                    }
                }
                last_poc = Some(p);
            }
        }
        if pictures != 1 {
            return fail(format!("{pictures} pictures (one per access unit)"));
        }
        let idr = idr.unwrap_or(false);
        if i == 0 && !idr {
            return fail("the stream does not start with an IDR".into());
        }
        if keyframe != idr {
            return fail(format!("keyframe flag {keyframe} on an IDR={idr} picture"));
        }
        if idr {
            let missing: Vec<u8> = parameter_set_types(codec)
                .iter()
                .copied()
                .filter(|t| !headers_before_slice.contains(t))
                .collect();
            if !missing.is_empty() {
                return fail(format!(
                    "IDR without its parameter sets in front (missing NAL types {missing:?})"
                ));
            }
            shape.keyframes.push(i);
        }
        shape.access_units += 1;
    }
    if shape.access_units == 0 {
        return Err("no access units".into());
    }
    shape.profile_idc = parser.profile_idc(codec);
    Ok(shape)
}

/// [`check`] on encoded units.
pub fn check_units(codec: Codec, units: &[EncodedUnit]) -> Result<Shape, String> {
    check(codec, units.iter().map(|u| (u.data.as_slice(), u.keyframe)))
}

/// [`check`] plus the session script: one access unit per frame, keyframes
/// exactly on frame 0 and on the forced frame (unit `n` is frame `n`:
/// encoders in this recipe neither reorder nor skip frames).
///
/// Deliberately stricter than `platform_api::VideoEncoder`, which allows
/// zero units while an encoder warms up: here every frame must come out of
/// its own `encode` call, because a frame held back is a frame of latency
/// on the wire. A failure of this rule on a device is therefore a latency
/// finding about that encoder, not a broken stream.
pub fn check_session(codec: Codec, session: &Session) -> Result<Shape, String> {
    let shape = check_units(codec, &session.units)?;
    if session.units.len() != session.frames {
        return Err(format!(
            "{} access units for {} frames (one per frame: none dropped, none held back)",
            session.units.len(),
            session.frames
        ));
    }
    let want = vec![0, session.forced_keyframe];
    if shape.keyframes != want {
        return Err(format!(
            "keyframes at {:?}, expected {want:?} (the first frame and the forced one only)",
            shape.keyframes
        ));
    }
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::test_headers::{h264_pps, h264_slice, h264_sps};

    // Minimal hand-assembled headers (only the fields the checker reads are
    // meaningful; the rest of each NAL is the stop bit). SPS: profile 66,
    // level 30, id 0, log2_max_frame_num_minus4 0, POC type 2, 1 reference,
    // 20x15 macroblocks, frame_mbs_only.
    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0xda, 0x05, 0x07, 0xe0];
    // PPS: id 0, SPS 0.
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xce];
    // IDR I slice: first_mb 0, slice_type 7, PPS 0, frame_num 0, idr_pic_id 0.
    const IDR: &[u8] = &[0, 0, 0, 1, 0x65, 0x88, 0x86];
    // P slice: first_mb 0, slice_type 5, PPS 0, frame_num 1.
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x30];

    /// An access unit with its keyframe flag, as `check` takes it.
    type Au<'a> = (&'a [u8], bool);

    fn au(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// Annex-B framing of the test NALs (none of them contains `00 00`).
    fn framed(nals: &[Vec<u8>]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat())
            .collect()
    }

    #[test]
    fn a_conforming_stream_passes() {
        let key = au(&[SPS, PPS, IDR]);
        let shape = check(Codec::H264, [(&key[..], true), (P, false), (P, false)]).unwrap();
        assert_eq!(shape.access_units, 3);
        assert_eq!(shape.keyframes, vec![0]);
        assert_eq!(shape.profile_idc, Some(66));
    }

    #[test]
    fn each_rule_is_enforced() {
        let key = au(&[SPS, PPS, IDR]);
        let delta_first = au(&[SPS, PPS, P]);
        let no_pps = au(&[SPS, IDR]);
        let cases: [(&str, Vec<Au<'_>>); 5] = [
            (
                "does not start with an IDR",
                vec![(&delta_first[..], false)],
            ),
            ("keyframe flag false", vec![(&key[..], false)]),
            ("keyframe flag true", vec![(&key[..], true), (P, true)]),
            // The PPS is known from the first keyframe, but a later
            // keyframe must repeat it.
            (
                "missing NAL types [8]",
                vec![(&key[..], true), (&no_pps[..], true)],
            ),
            ("start code", vec![(&key[4..], true)]),
        ];
        for (want, aus) in cases {
            let err = check(Codec::H264, aus).unwrap_err();
            assert!(err.contains(want), "expected `{want}` in `{err}`");
        }
        let two = au(&[SPS, PPS, IDR, IDR]);
        assert!(check(Codec::H264, [(&two[..], true)])
            .unwrap_err()
            .contains("2 pictures"));
    }

    /// Low-delay B (POC still rising) passes; a picture whose POC goes
    /// backwards — output before one decoded earlier — fails.
    #[test]
    fn b_slices_pass_only_without_reordering() {
        let key = framed(&[h264_sps(0), h264_pps(), h264_slice(0x65, 7, true, 0)]);
        let b = |lsb| framed(&[h264_slice(0x01, 6, false, lsb)]);
        let p = |lsb| framed(&[h264_slice(0x41, 5, false, lsb)]);
        let (p2, b4, p6) = (p(2), b(4), p(6));
        let shape = check(
            Codec::H264,
            [
                (&key[..], true),
                (&p2[..], false),
                (&b4[..], false),
                (&p6[..], false),
            ],
        )
        .unwrap();
        assert!(shape.slice_kinds.contains("B"));

        let (p8, b4) = (p(8), b(4));
        let err = check(
            Codec::H264,
            [(&key[..], true), (&p8[..], false), (&b4[..], false)],
        )
        .unwrap_err();
        assert!(err.contains("reorders"), "{err}");
    }
}
