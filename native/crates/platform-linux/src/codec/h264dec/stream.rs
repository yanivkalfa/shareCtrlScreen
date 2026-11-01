//! One H.264 stream's decoding state: parameter sets, the reference buffer
//! and the picture-order state, advanced one access unit at a time.
//!
//! [`Stream::begin`] turns an access unit into a [`Picture`] — the slices to
//! submit, each slice's reference list, and whether the picture can be decoded
//! at all — and [`Stream::finish`] records the result (reference marking,
//! POC state). The hardware decode happens between the two; nothing here
//! touches the driver, so the whole state machine is unit-tested on real
//! encoder output.

use std::rc::Rc;

use super::bits::unescape;
use super::params::{Pps, Sps, MAX_PPS, MAX_SPS};
use super::refs::{Current, Dpb, ListEntry, Pixels, Poc, PocState};
use super::slice::{pps_id_of, SliceHeader, SliceType};
use super::Bad;
use crate::codec::annexb;

/// How much of a slice NAL is unescaped to parse its header. The longest
/// header the syntax allows (66 list commands per list, a full weight table
/// for 32 references, 66 MMCOs) is under 2 KiB unescaped; 4 KiB of escaped
/// bytes always yields more than that (emulation prevention adds at most one
/// byte in three), and saves unescaping whole slices just to read their
/// headers.
const HEADER_PREFIX: usize = 4096;

/// A slice as parsed, before planning: its NAL and header, and the
/// parameter sets it named.
type Parsed<'a> = (&'a [u8], SliceHeader, Rc<Sps>, Rc<Pps>);

/// One slice of the picture.
#[derive(Debug)]
pub(crate) struct Slice<'a> {
    /// The NAL unit as transmitted: header byte included, start code and
    /// trailing zero bytes excluded, emulation-prevention bytes kept — the
    /// bytes the hardware parses.
    pub nal: &'a [u8],
    pub header: SliceHeader,
    /// RefPicList0 (`num_ref_idx_l0_active` entries; empty for I slices).
    pub list0: Vec<ListEntry>,
}

/// One primary coded picture, planned.
#[derive(Debug)]
pub(crate) struct Picture<'a> {
    pub sps: Rc<Sps>,
    pub pps: Rc<Pps>,
    pub slices: Vec<Slice<'a>>,
    pub poc: Poc,
    /// Starts a new coded video sequence (IDR): the caller may need new
    /// surfaces for a new size.
    pub idr: bool,
    /// Every reference a slice may predict from has pixels. When false the
    /// picture must not be submitted (it would predict from garbage); it is
    /// still passed to [`Stream::finish`] as [`Pixels::Lost`].
    pub decodable: bool,
}

impl Picture<'_> {
    /// The first slice's header: frame-level fields are equal in all slices.
    pub(crate) fn first(&self) -> &SliceHeader {
        &self.slices[0].header
    }

    pub(crate) fn is_reference(&self) -> bool {
        self.first().nal_ref_idc != 0
    }
}

/// Decoding state of one stream.
pub(crate) struct Stream<S> {
    sps: Vec<Option<Rc<Sps>>>,
    pps: Vec<Option<Rc<Pps>>>,
    /// The SPS the current coded video sequence was started with.
    active: Option<Rc<Sps>>,
    dpb: Dpb<S>,
    poc: PocState,
    /// `PrevRefFrameNum` (§7.4.3).
    prev_ref_frame_num: u32,
}

impl<S: Copy> Default for Stream<S> {
    fn default() -> Self {
        Self {
            sps: vec![None; MAX_SPS],
            pps: vec![None; MAX_PPS],
            active: None,
            dpb: Dpb::default(),
            poc: PocState::default(),
            prev_ref_frame_num: 0,
        }
    }
}

impl<S: Copy> Stream<S> {
    pub(crate) fn dpb(&self) -> &Dpb<S> {
        &self.dpb
    }

    /// Parse an access unit and plan its picture. `Ok(None)`: nothing to
    /// decode — parameter sets only, or a non-IDR picture before the first
    /// IDR (its references were never received).
    pub(crate) fn begin<'a>(&mut self, au: &'a [u8]) -> Result<Option<Picture<'a>>, Bad> {
        let mut slices: Vec<Parsed<'a>> = Vec::new();
        for nal in annexb::nal_units(au) {
            match nal[0] & 0x1f {
                7 => {
                    let sps = Sps::parse(&unescape(&nal[1..]))?;
                    let id = sps.id;
                    self.sps[id] = Some(Rc::new(sps));
                }
                8 => {
                    let sps = &self.sps;
                    let pps = Pps::parse(&unescape(&nal[1..]), |id| sps[id].as_deref())?;
                    let id = pps.id;
                    self.pps[id] = Some(Rc::new(pps));
                }
                1 | 5 => {
                    let rbsp = unescape(&nal[..nal.len().min(HEADER_PREFIX)]);
                    let pps_id = pps_id_of(&rbsp)?;
                    let pps = self.pps[pps_id].clone().ok_or_else(|| {
                        Bad::Malformed(format!("slice names PPS {pps_id}, never received"))
                    })?;
                    let sps = self.sps[pps.sps_id].clone().ok_or_else(|| {
                        Bad::Malformed(format!(
                            "PPS {pps_id} names SPS {}, never received",
                            pps.sps_id
                        ))
                    })?;
                    check_supported(&sps)?;
                    let header = SliceHeader::parse(&rbsp, &sps, &pps)?;
                    // Redundant pictures only stand in for a lost primary; the
                    // primary is in this access unit.
                    if header.redundant_pic_cnt == 0 {
                        slices.push((nal, header, sps, pps));
                    }
                }
                2..=4 => {
                    return Err(Bad::Unsupported(
                        "data partitioning (Extended profile)".into(),
                    ))
                }
                // SEI, delimiters, end of sequence/stream, filler, SPS
                // extensions, auxiliary pictures and the SVC/MVC NAL types
                // carry nothing the base-layer picture needs.
                _ => {}
            }
        }
        let Some((_, first, sps, pps)) = slices.first() else {
            return Ok(None);
        };
        let (first, sps, pps) = (first.clone(), sps.clone(), pps.clone());
        for (_, h, _, _) in &slices[1..] {
            if !h.same_picture(&first) {
                return Err(Bad::Malformed(
                    "the access unit holds slices of more than one picture".into(),
                ));
            }
        }
        for (_, h, _, _) in &slices {
            match h.slice_type {
                SliceType::B => {
                    return Err(Bad::Unsupported(
                        "B-frames (picture reordering) — the low-latency hosts never send them"
                            .into(),
                    ))
                }
                SliceType::Sp | SliceType::Si => {
                    return Err(Bad::Unsupported("SP/SI slices (Extended profile)".into()))
                }
                SliceType::P | SliceType::I => {}
            }
        }

        if first.idr {
            self.dpb.flush();
            self.poc = PocState::default();
            self.prev_ref_frame_num = 0;
            self.active = Some(sps.clone());
        } else {
            match &self.active {
                None => return Ok(None),
                // An SPS may only change at an IDR (§7.4.1.2.1); an equal
                // re-send is fine.
                Some(active) if **active != *sps => {
                    return Err(Bad::Malformed(
                        "the SPS changed without an IDR picture".into(),
                    ))
                }
                Some(_) => {}
            }
            let max = sps.max_frame_num();
            let expected = (self.prev_ref_frame_num + 1) % max;
            if first.frame_num != self.prev_ref_frame_num && first.frame_num != expected {
                // §8.2.5.2. Without `gaps_in_frame_num_value_allowed_flag`
                // the gap is a loss: the inferred frames are `Lost`, and
                // whatever predicts from them is not decoded until an IDR (or
                // a picture predicting only from intact references) arrives.
                let pixels = if sps.gaps_in_frame_num_allowed {
                    Pixels::NonExisting
                } else {
                    tracing::warn!(
                        "H.264: frame_num jumped {} → {} — reference pictures were lost",
                        self.prev_ref_frame_num,
                        first.frame_num
                    );
                    Pixels::Lost
                };
                self.dpb
                    .fill_gap(expected, first.frame_num, max, max_refs(&sps), pixels);
                let mut n = expected;
                while n != first.frame_num {
                    self.poc.note_inferred_frame(&sps, n);
                    n = (n + 1) % max;
                }
                self.prev_ref_frame_num = (first.frame_num + max - 1) % max;
            }
        }

        let poc = self.poc.compute(&sps, &first);
        let mut planned = Vec::with_capacity(slices.len());
        let mut decodable = true;
        for (nal, header, _, _) in slices {
            let list0 = if header.slice_type == SliceType::P {
                self.dpb.p_list(&header, sps.max_frame_num())?
            } else {
                Vec::new()
            };
            decodable &= self.dpb.decodable(&list0);
            planned.push(Slice { nal, header, list0 });
        }
        Ok(Some(Picture {
            sps,
            pps,
            slices: planned,
            poc,
            idr: first.idr,
            decodable,
        }))
    }

    /// Record the outcome of `pic`: `pixels` is where it was decoded to, or
    /// [`Pixels::Lost`] when it was not (undecodable, or the driver failed).
    pub(crate) fn finish(&mut self, pic: &Picture<'_>, pixels: Pixels<S>) {
        let first = pic.first();
        let mut mmco5 = false;
        if let Some(marking) = &first.marking {
            let marked = self.dpb.mark_and_store(
                marking,
                Current {
                    frame_num: first.frame_num,
                    max_frame_num: pic.sps.max_frame_num(),
                    poc: pic.poc.as_i32(),
                    max_refs: max_refs(&pic.sps),
                },
                pixels,
            );
            mmco5 = marked.mmco5;
            self.prev_ref_frame_num = if mmco5 { 0 } else { first.frame_num };
        }
        self.poc.advance(first, &pic.poc, mmco5);
    }
}

/// `Max(max_num_ref_frames, 1)`: the reference buffer's capacity.
pub(crate) fn max_refs(sps: &Sps) -> usize {
    sps.max_num_ref_frames.max(1) as usize
}

/// Refuse stream shapes the planner (frames only) and the NV12 output (8-bit
/// 4:2:0) cannot handle, naming what was found.
fn check_supported(sps: &Sps) -> Result<(), Bad> {
    if !sps.frame_mbs_only {
        return Err(Bad::Unsupported(
            "interlaced (field / MBAFF) coding — the hosts send progressive frames".into(),
        ));
    }
    if sps.chroma_format_idc != 1 {
        return Err(Bad::Unsupported(format!(
            "chroma_format_idc {} (only 4:2:0 decodes to NV12)",
            sps.chroma_format_idc
        )));
    }
    if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
        return Err(Bad::Unsupported(format!(
            "{}-bit video (only 8-bit decodes to NV12)",
            sps.bit_depth_luma_minus8 + 8
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::h264::{nal, BitWriter, Profile, SeqHeader};
    use crate::codec::h264dec::slice::tests::header;

    /// An access unit of the VA-API encoder's shape: optional SPS+PPS, then
    /// one slice written by `body` (fields after `first_mb`/type/pps).
    pub(crate) fn au(
        sets: bool,
        nal_ref_idc: u8,
        idr: bool,
        body: impl Fn(&mut BitWriter),
    ) -> Vec<u8> {
        let h = SeqHeader::new(Profile::Main, 320, 240, 30, 1);
        let mut out = Vec::new();
        if sets {
            out.extend(h.sps_nal());
            out.extend(h.pps_nal());
        }
        let rbsp = header(nal_ref_idc, if idr { 5 } else { 1 }, body);
        out.extend(nal(nal_ref_idc, if idr { 5 } else { 1 }, &rbsp[1..]));
        // A few bytes standing in for slice data.
        out.extend([0x88, 0x84, 0x21]);
        out
    }

    fn idr(idr_pic_id: u32) -> Vec<u8> {
        au(true, 3, true, |w| {
            w.ue(0);
            w.ue(7);
            w.ue(0);
            w.bits(0, 16);
            w.ue(idr_pic_id);
            w.flag(false);
            w.flag(false);
            w.se(0);
            w.ue(0);
            w.se(0);
            w.se(0);
        })
    }

    fn p(frame_num: u32, nal_ref_idc: u8) -> Vec<u8> {
        au(false, nal_ref_idc, false, |w| {
            w.ue(0);
            w.ue(5);
            w.ue(0);
            w.bits(frame_num as u64, 16);
            w.flag(false); // PPS default: 1 active
            w.flag(false); // no modification
            if nal_ref_idc != 0 {
                w.flag(false); // sliding window
            }
            w.ue(0); // cabac_init_idc
            w.se(0);
            w.ue(0);
            w.se(0);
            w.se(0);
        })
    }

    /// Plan + finish with surface `n` (or Lost when undecodable).
    fn step(s: &mut Stream<u32>, au: &[u8], n: u32) -> Option<(bool, Vec<Option<u32>>)> {
        let pic = s.begin(au).expect("plan")?;
        let refs = pic.slices[0]
            .list0
            .iter()
            .map(|e| match *e {
                ListEntry::Ref(i) => s.dpb().frames()[i].surface().copied(),
                _ => None,
            })
            .collect();
        let decodable = pic.decodable;
        s.finish(
            &pic,
            if decodable {
                Pixels::Decoded(n)
            } else {
                Pixels::Lost
            },
        );
        Some((decodable, refs))
    }

    #[test]
    fn idr_then_p_frames_reference_the_previous_picture() {
        let mut s = Stream::default();
        assert_eq!(step(&mut s, &idr(0), 0), Some((true, vec![])));
        for n in 1..5 {
            assert_eq!(step(&mut s, &p(n, 2), n), Some((true, vec![Some(n - 1)])));
        }
        // max_num_ref_frames is 1: only the newest frame is held.
        assert_eq!(s.dpb().frames().len(), 1);
    }

    #[test]
    fn nothing_before_the_first_idr() {
        let mut s: Stream<u32> = Stream::default();
        // Parameter sets arrive with the IDR only; a P frame first has none.
        assert!(s.begin(&p(3, 2)).is_err());
        // With parameter sets known but no IDR yet: Ok(None).
        let mut s: Stream<u32> = Stream::default();
        let h = SeqHeader::new(Profile::Main, 320, 240, 30, 1);
        let sets = [h.sps_nal(), h.pps_nal()].concat();
        assert!(s.begin(&sets).expect("sets").is_none());
        assert!(s.begin(&p(3, 2)).expect("plan").is_none());
    }

    #[test]
    fn a_lost_reference_freezes_until_the_next_idr() {
        let mut s = Stream::default();
        step(&mut s, &idr(0), 0);
        step(&mut s, &p(1, 2), 1);
        // Frame 2 lost: frame 3 predicts from an inferred, lost frame.
        assert_eq!(step(&mut s, &p(3, 2), 3), Some((false, vec![None])));
        // ...and so does everything after it.
        assert_eq!(step(&mut s, &p(4, 2), 4), Some((false, vec![None])));
        assert_eq!(step(&mut s, &idr(1), 5), Some((true, vec![])));
        assert_eq!(step(&mut s, &p(1, 2), 6), Some((true, vec![Some(5)])));
    }

    #[test]
    fn non_reference_pictures_are_not_stored() {
        let mut s = Stream::default();
        step(&mut s, &idr(0), 0);
        assert_eq!(step(&mut s, &p(1, 0), 1), Some((true, vec![Some(0)])));
        // Next reference picture also has frame_num 1 (no gap) and still
        // predicts from the IDR.
        assert_eq!(step(&mut s, &p(1, 2), 2), Some((true, vec![Some(0)])));
    }

    #[test]
    fn unsupported_shapes_are_named() {
        let mut s: Stream<u32> = Stream::default();
        step(&mut s, &idr(0), 0);
        let b = au(false, 2, false, |w| {
            w.ue(0);
            w.ue(6); // B
            w.ue(0);
            w.bits(1, 16);
            w.flag(false); // direct_spatial_mv_pred
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.ue(0);
            w.se(0);
            w.ue(0);
            w.se(0);
            w.se(0);
        });
        assert!(matches!(s.begin(&b), Err(Bad::Unsupported(m)) if m.contains("B-frames")));
        let mut dp = idr(0);
        dp.extend([0, 0, 0, 1, 0x22, 0x80]);
        assert!(matches!(s.begin(&dp), Err(Bad::Unsupported(_))));
    }

    #[test]
    fn two_pictures_in_one_access_unit_are_refused() {
        let mut s: Stream<u32> = Stream::default();
        step(&mut s, &idr(0), 0);
        let both = [p(1, 2), p(2, 2)].concat();
        assert!(matches!(s.begin(&both), Err(Bad::Malformed(_))));
    }
}
