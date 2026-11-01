//! Reference pictures of a frame-coded (progressive) H.264 stream: picture
//! order counts (§8.2.1), reference list construction for P slices (§8.2.4)
//! and reference marking (§8.2.5) — the bookkeeping a stateless decode API
//! leaves to the application.
//!
//! Scope, and WHY it is enough: every host encoder sends progressive frames
//! without B-frames (the `platform_api::VideoEncoder` recipe), so fields,
//! MBAFF and B-slice list ordering are refused upstream. What *is* in scope is
//! everything a P-only encoder may use: several short-term references,
//! long-term references (Media Foundation's LTR recovery), explicit list
//! reordering, MMCOs including 5, and frame-number gaps.

use super::params::{PocType, Sps};
use super::slice::{Marking, Mmco, Modification, SliceHeader};
use super::Bad;

/// What a reference frame's pixels are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pixels<S> {
    /// Decoded into this surface.
    Decoded(S),
    /// Inferred for a frame-number gap the SPS allows (§8.2.5.2): no pixels,
    /// and a conforming stream never predicts from it.
    NonExisting,
    /// Never decoded (missing reference, failed decode, or a gap the SPS does
    /// not allow — a lost frame). Pictures predicting from it are not
    /// decoded either, so a loss freezes the picture instead of smearing it
    /// until the next IDR.
    Lost,
}

/// One frame in the decoded picture buffer, "used for reference".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefFrame<S> {
    /// `FrameNum` (short-term); kept for long-term frames as VA-API wants it.
    pub frame_num: u32,
    /// `LongTermFrameIdx` when marked long-term.
    pub long_term_idx: Option<u32>,
    /// Top and bottom field order counts.
    pub poc: [i32; 2],
    pub pixels: Pixels<S>,
}

impl<S> RefFrame<S> {
    pub(crate) fn surface(&self) -> Option<&S> {
        match &self.pixels {
            Pixels::Decoded(s) => Some(s),
            _ => None,
        }
    }
}

/// One slot of a reference picture list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListEntry {
    /// Index into [`Dpb::frames`].
    Ref(usize),
    /// "No reference picture": past the end of the initial list. Legal as
    /// long as no macroblock uses it.
    Empty,
    /// A list modification named a picture the buffer does not hold.
    Missing,
}

/// The picture a list-modification command names.
#[derive(Debug, Clone, Copy)]
enum Pick {
    /// Short-term, by `PicNum`.
    Short(i64),
    /// Long-term, by `LongTermPicNum`.
    Long(u32),
}

/// The decoded picture buffer: reference frames only. Non-reference pictures
/// are output (copied out) as soon as they are decoded and never stored,
/// because nothing is held for reordering.
#[derive(Debug)]
pub(crate) struct Dpb<S> {
    frames: Vec<RefFrame<S>>,
    /// `MaxLongTermFrameIdx`; `None` = "no long-term frame indices".
    max_long_term_idx: Option<u32>,
}

impl<S> Default for Dpb<S> {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            max_long_term_idx: None,
        }
    }
}

/// The current picture's identity, as reference marking needs it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Current {
    pub frame_num: u32,
    pub max_frame_num: u32,
    pub poc: [i32; 2],
    /// `Max(max_num_ref_frames, 1)`.
    pub max_refs: usize,
}

/// Outcome of marking the current picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Marked {
    /// The picture carried MMCO 5: frame_num and POC restart from it.
    pub mmco5: bool,
}

impl<S> Dpb<S> {
    pub(crate) fn frames(&self) -> &[RefFrame<S>] {
        &self.frames
    }

    /// Every reference → unused (IDR, MMCO 5).
    pub(crate) fn flush(&mut self) {
        self.frames.clear();
        self.max_long_term_idx = None;
    }

    /// `PicNum` of a short-term frame relative to the current picture
    /// (§8.2.4.1, frames: `PicNum = FrameNumWrap`).
    fn pic_num(f: &RefFrame<S>, curr_frame_num: u32, max_frame_num: u32) -> i64 {
        if f.frame_num > curr_frame_num {
            f.frame_num as i64 - max_frame_num as i64
        } else {
            f.frame_num as i64
        }
    }

    fn short_term(&self) -> impl Iterator<Item = (usize, &RefFrame<S>)> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.long_term_idx.is_none())
    }

    fn short_with_pic_num(&self, pic_num: i64, curr: u32, max: u32) -> Option<usize> {
        self.short_term()
            .find(|(_, f)| Self::pic_num(f, curr, max) == pic_num)
            .map(|(i, _)| i)
    }

    fn long_with_idx(&self, idx: u32) -> Option<usize> {
        self.frames
            .iter()
            .position(|f| f.long_term_idx == Some(idx))
    }

    /// Sliding-window marking (§8.2.5.3): while the buffer is full, the
    /// short-term frame with the smallest `FrameNumWrap` goes.
    fn sliding_window(&mut self, curr: u32, max_frame_num: u32, max_refs: usize) {
        while self.frames.len() >= max_refs {
            let Some(oldest) = self
                .short_term()
                .min_by_key(|(_, f)| Self::pic_num(f, curr, max_frame_num))
                .map(|(i, _)| i)
            else {
                // Full of long-term frames: a conforming stream never gets
                // here (§8.2.5.3 requires numShortTerm > 0).
                break;
            };
            self.frames.remove(oldest);
        }
    }

    /// The decoding process for gaps in `frame_num` (§8.2.5.2): one inferred
    /// short-term frame per missing number, each through the sliding window.
    pub(crate) fn fill_gap(
        &mut self,
        from: u32,
        to: u32,
        max_frame_num: u32,
        max_refs: usize,
        pixels: Pixels<S>,
    ) where
        S: Copy,
    {
        let mut n = from;
        while n != to {
            self.sliding_window(n, max_frame_num, max_refs);
            self.frames.push(RefFrame {
                frame_num: n,
                long_term_idx: None,
                poc: [0, 0],
                pixels,
            });
            n = (n + 1) % max_frame_num;
        }
    }

    /// The initial list 0 for a P/SP slice (§8.2.4.2.1): short-term frames by
    /// descending `PicNum`, then long-term frames by ascending
    /// `LongTermPicNum`.
    fn initial_p_list(&self, curr: u32, max_frame_num: u32) -> Vec<ListEntry> {
        let mut short: Vec<(i64, usize)> = self
            .short_term()
            .map(|(i, f)| (Self::pic_num(f, curr, max_frame_num), i))
            .collect();
        short.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut long: Vec<(u32, usize)> = self
            .frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.long_term_idx.map(|idx| (idx, i)))
            .collect();
        long.sort();
        short
            .into_iter()
            .map(|(_, i)| i)
            .chain(long.into_iter().map(|(_, i)| i))
            .map(ListEntry::Ref)
            .collect()
    }

    /// RefPicList0 of a P/SP slice: initialised, modified (§8.2.4.3) and cut
    /// to `num_ref_idx_l0_active` entries.
    pub(crate) fn p_list(
        &self,
        slice: &SliceHeader,
        max_frame_num: u32,
    ) -> Result<Vec<ListEntry>, Bad> {
        let curr = slice.frame_num;
        let n = slice.num_ref_idx_l0_active as usize;
        let mut list = self.initial_p_list(curr, max_frame_num);
        // §8.2.4.2: the initial list is cut to the active size, padded with
        // "no reference picture", and one spare slot is used while modifying.
        list.truncate(n);
        list.resize(n + 1, ListEntry::Empty);
        let max = max_frame_num as i64;
        let mut pred = curr as i64;
        let mut idx = 0usize;
        for m in &slice.modifications_l0 {
            if idx >= n {
                return Err(Bad::Malformed(format!(
                    "more list modifications than the {n} active references"
                )));
            }
            let pick = match *m {
                Modification::ShortTermSub(d) | Modification::ShortTermAdd(d) => {
                    let diff = d as i64 + 1;
                    if diff > max {
                        return Err(Bad::Malformed(format!(
                            "abs_diff_pic_num_minus1 {d} ≥ MaxPicNum {max}"
                        )));
                    }
                    // §8.2.4.3.1, (8-34)…(8-36).
                    let no_wrap = if matches!(m, Modification::ShortTermSub(_)) {
                        let v = pred - diff;
                        if v < 0 {
                            v + max
                        } else {
                            v
                        }
                    } else {
                        let v = pred + diff;
                        if v >= max {
                            v - max
                        } else {
                            v
                        }
                    };
                    pred = no_wrap;
                    Pick::Short(if no_wrap > curr as i64 {
                        no_wrap - max
                    } else {
                        no_wrap
                    })
                }
                Modification::LongTerm(lt) => Pick::Long(lt),
            };
            let target = match pick {
                Pick::Short(pic_num) => self.short_with_pic_num(pic_num, curr, max_frame_num),
                Pick::Long(lt) => self.long_with_idx(lt),
            }
            .map_or(ListEntry::Missing, ListEntry::Ref);
            // (8-37) / (8-38): shift right, insert, then drop the later
            // duplicate of the inserted picture.
            list.copy_within(idx..n, idx + 1);
            list[idx] = target;
            idx += 1;
            let mut keep = idx;
            for c in idx..=n {
                if !self.is_pick(&list[c], pick, curr, max_frame_num) {
                    list[keep] = list[c];
                    keep += 1;
                }
            }
        }
        list.truncate(n);
        Ok(list)
    }

    /// `PicNumF(e) == picNum` / `LongTermPicNumF(e) == LongTermPicNum` of
    /// (8-37)/(8-38): the entry is the picture the command just inserted.
    fn is_pick(&self, e: &ListEntry, pick: Pick, curr: u32, max_frame_num: u32) -> bool {
        let ListEntry::Ref(i) = *e else {
            return false;
        };
        let f = &self.frames[i];
        match pick {
            Pick::Short(pic_num) => {
                f.long_term_idx.is_none() && Self::pic_num(f, curr, max_frame_num) == pic_num
            }
            Pick::Long(lt) => f.long_term_idx == Some(lt),
        }
    }

    /// Whether every entry a slice may predict from has pixels (or is legally
    /// absent). A `Missing` or `Lost` reference makes the picture undecodable.
    pub(crate) fn decodable(&self, list: &[ListEntry]) -> bool {
        list.iter().all(|e| match *e {
            ListEntry::Ref(i) => !matches!(self.frames[i].pixels, Pixels::Lost),
            ListEntry::Empty => true,
            ListEntry::Missing => false,
        })
    }

    /// Reference marking for the current (reference) picture (§8.2.5.1), then
    /// store it.
    pub(crate) fn mark_and_store(
        &mut self,
        marking: &Marking,
        cur: Current,
        pixels: Pixels<S>,
    ) -> Marked {
        let curr = cur.frame_num;
        let max = cur.max_frame_num;
        let mut long_term = None;
        let mut mmco5 = false;
        match marking {
            Marking::Idr {
                long_term_reference,
            } => {
                self.flush();
                if *long_term_reference {
                    self.max_long_term_idx = Some(0);
                    long_term = Some(0);
                }
            }
            Marking::SlidingWindow => self.sliding_window(curr, max, cur.max_refs),
            Marking::Adaptive(ops) => {
                for op in ops {
                    match *op {
                        Mmco::ForgetShortTerm { difference_minus1 } => {
                            let x = curr as i64 - (difference_minus1 as i64 + 1);
                            if let Some(i) = self.short_with_pic_num(x, curr, max) {
                                self.frames.remove(i);
                            }
                        }
                        Mmco::ForgetLongTerm { long_term_pic_num } => {
                            if let Some(i) = self.long_with_idx(long_term_pic_num) {
                                self.frames.remove(i);
                            }
                        }
                        Mmco::ShortToLongTerm {
                            difference_minus1,
                            long_term_frame_idx,
                        } => {
                            let x = curr as i64 - (difference_minus1 as i64 + 1);
                            if let Some(i) = self.long_with_idx(long_term_frame_idx) {
                                self.frames.remove(i);
                            }
                            if let Some(i) = self.short_with_pic_num(x, curr, max) {
                                self.frames[i].long_term_idx = Some(long_term_frame_idx);
                            }
                        }
                        Mmco::MaxLongTermIdx { plus1 } => {
                            self.max_long_term_idx = plus1.checked_sub(1);
                            let limit = self.max_long_term_idx;
                            self.frames.retain(|f| match (f.long_term_idx, limit) {
                                (None, _) => true,
                                (Some(idx), Some(max)) => idx <= max,
                                (Some(_), None) => false,
                            });
                        }
                        Mmco::ForgetAll => {
                            self.flush();
                            mmco5 = true;
                        }
                        Mmco::CurrentToLongTerm {
                            long_term_frame_idx,
                        } => {
                            if let Some(i) = self.long_with_idx(long_term_frame_idx) {
                                self.frames.remove(i);
                            }
                            long_term = Some(long_term_frame_idx);
                        }
                    }
                }
            }
        }
        // After MMCO 5 the picture counts as frame_num 0 with its POC rebased
        // so that the smaller field POC is 0 (§8.2.1, "tempPicOrderCnt").
        let (frame_num, poc) = if mmco5 {
            let base = cur.poc[0].min(cur.poc[1]);
            (0, [cur.poc[0] - base, cur.poc[1] - base])
        } else {
            (curr, cur.poc)
        };
        self.frames.push(RefFrame {
            frame_num,
            long_term_idx: long_term,
            poc,
            pixels,
        });
        // A conforming stream never holds more than Max(max_num_ref_frames,
        // 1) frames here (§8.2.5.1); a broken one would grow the buffer
        // without bound, so the oldest short-term frame (else the lowest
        // long-term index) goes — never the picture just stored.
        while self.frames.len() > cur.max_refs {
            let last = self.frames.len() - 1;
            let victim = self
                .frames
                .iter()
                .enumerate()
                .take(last)
                .filter(|(_, f)| f.long_term_idx.is_none())
                .min_by_key(|(_, f)| Self::pic_num(f, frame_num, max))
                .map(|(i, _)| i)
                .or_else(|| {
                    self.frames
                        .iter()
                        .enumerate()
                        .take(last)
                        .min_by_key(|(_, f)| f.long_term_idx)
                        .map(|(i, _)| i)
                });
            let Some(v) = victim else { break };
            tracing::warn!(
                "H.264: the stream holds more than {} reference frames — dropping the oldest",
                cur.max_refs
            );
            self.frames.remove(v);
        }
        Marked { mmco5 }
    }
}

/// A picture's order counts and the intermediate values the next picture's
/// derivation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Poc {
    pub top: i64,
    pub bottom: i64,
    /// `PicOrderCntMsb` (type 0).
    msb: i64,
    /// `FrameNumOffset` (types 1 and 2).
    frame_num_offset: i64,
}

impl Poc {
    /// The order counts as the 32-bit values VA-API carries. They only order
    /// pictures relative to each other, and a P-only stream decodes without
    /// them (no temporal direct, no implicit weights), so the wrap after
    /// 2^31 (≈ 200 days at 60 fps for type 2) is harmless.
    pub(crate) fn as_i32(&self) -> [i32; 2] {
        [self.top as i32, self.bottom as i32]
    }
}

/// The "previous picture" state of §8.2.1.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PocState {
    /// `prevPicOrderCntMsb` / `prevPicOrderCntLsb` (previous reference picture).
    prev_msb: i64,
    prev_lsb: i64,
    /// `prevFrameNum` / `prevFrameNumOffset` (previous picture of any kind).
    prev_frame_num: u32,
    prev_frame_num_offset: i64,
}

impl PocState {
    fn frame_num_offset(&self, sps: &Sps, idr: bool, frame_num: u32) -> i64 {
        if idr {
            0
        } else if self.prev_frame_num > frame_num {
            self.prev_frame_num_offset + sps.max_frame_num() as i64
        } else {
            self.prev_frame_num_offset
        }
    }

    /// Order counts of the picture `s` (the first slice of it).
    pub(crate) fn compute(&self, sps: &Sps, s: &SliceHeader) -> Poc {
        let reference = s.nal_ref_idc != 0;
        match &sps.poc {
            PocType::Lsb { log2_max_lsb } => {
                let max_lsb = 1i64 << log2_max_lsb;
                let (prev_msb, prev_lsb) = if s.idr {
                    (0, 0)
                } else {
                    (self.prev_msb, self.prev_lsb)
                };
                let lsb = s.pic_order_cnt_lsb as i64;
                // (8-3)
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                Poc {
                    top,
                    bottom: top + s.delta_pic_order_cnt_bottom as i64,
                    msb,
                    frame_num_offset: 0,
                }
            }
            PocType::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offset_for_ref_frame,
                ..
            } => {
                let offset = self.frame_num_offset(sps, s.idr, s.frame_num);
                let cycle = offset_for_ref_frame.len() as i64;
                let mut abs = if cycle != 0 {
                    offset + s.frame_num as i64
                } else {
                    0
                };
                if !reference && abs > 0 {
                    abs -= 1;
                }
                // (8-7)…(8-9)
                let mut expected = 0i64;
                if abs > 0 {
                    let per_cycle: i64 = offset_for_ref_frame.iter().map(|&o| o as i64).sum();
                    let cycles = (abs - 1) / cycle;
                    let in_cycle = ((abs - 1) % cycle) as usize;
                    expected = cycles * per_cycle
                        + offset_for_ref_frame[..=in_cycle]
                            .iter()
                            .map(|&o| o as i64)
                            .sum::<i64>();
                }
                if !reference {
                    expected += *offset_for_non_ref_pic as i64;
                }
                let top = expected + s.delta_pic_order_cnt[0] as i64;
                Poc {
                    top,
                    bottom: top
                        + *offset_for_top_to_bottom_field as i64
                        + s.delta_pic_order_cnt[1] as i64,
                    msb: 0,
                    frame_num_offset: offset,
                }
            }
            PocType::FrameNum => {
                let offset = self.frame_num_offset(sps, s.idr, s.frame_num);
                // (8-12)
                let poc = if s.idr {
                    0
                } else if reference {
                    2 * (offset + s.frame_num as i64)
                } else {
                    2 * (offset + s.frame_num as i64) - 1
                };
                Poc {
                    top: poc,
                    bottom: poc,
                    msb: 0,
                    frame_num_offset: offset,
                }
            }
        }
    }

    /// Record a frame inferred for a frame-number gap: it is "the previous
    /// picture" for types 1/2, and never a reference for type 0's purposes
    /// (§8.2.5.2 leaves its POC unspecified).
    pub(crate) fn note_inferred_frame(&mut self, sps: &Sps, frame_num: u32) {
        self.prev_frame_num_offset = self.frame_num_offset(sps, false, frame_num);
        self.prev_frame_num = frame_num;
    }

    /// Advance past the decoded picture `s` with order counts `poc`.
    pub(crate) fn advance(&mut self, s: &SliceHeader, poc: &Poc, mmco5: bool) {
        if mmco5 {
            // §8.2.1: after MMCO 5 the picture is "frame_num 0", its POC
            // rebased so the smaller field count is 0.
            let base = poc.top.min(poc.bottom);
            self.prev_msb = 0;
            self.prev_lsb = poc.top - base;
            self.prev_frame_num = 0;
            self.prev_frame_num_offset = 0;
            return;
        }
        if s.nal_ref_idc != 0 {
            self.prev_msb = poc.msb;
            self.prev_lsb = s.pic_order_cnt_lsb as i64;
        }
        self.prev_frame_num = s.frame_num;
        self.prev_frame_num_offset = poc.frame_num_offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264dec::slice::tests::main_sets;
    use crate::codec::h264dec::slice::SliceType;

    const MAX: u32 = 16;

    fn p_slice(frame_num: u32, active: u32, mods: Vec<Modification>) -> SliceHeader {
        SliceHeader {
            nal_ref_idc: 1,
            idr: false,
            first_mb_in_slice: 0,
            slice_type: SliceType::P,
            pps_id: 0,
            frame_num,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0, 0],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred: false,
            num_ref_idx_l0_active: active,
            num_ref_idx_l1_active: 0,
            modifications_l0: mods,
            modifications_l1: Vec::new(),
            pred_weights: None,
            marking: Some(Marking::SlidingWindow),
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            header_bits: 0,
        }
    }

    fn cur(frame_num: u32, max_refs: usize) -> Current {
        Current {
            frame_num,
            max_frame_num: MAX,
            poc: [2 * frame_num as i32; 2],
            max_refs,
        }
    }

    /// Store `count` sliding-window reference frames 0..count (surface = n).
    fn filled(count: u32, max_refs: usize) -> Dpb<u32> {
        let mut d = Dpb::default();
        d.mark_and_store(
            &Marking::Idr {
                long_term_reference: false,
            },
            cur(0, max_refs),
            Pixels::Decoded(0),
        );
        for n in 1..count {
            d.mark_and_store(
                &Marking::SlidingWindow,
                cur(n, max_refs),
                Pixels::Decoded(n),
            );
        }
        d
    }

    fn surfaces(d: &Dpb<u32>, list: &[ListEntry]) -> Vec<Option<u32>> {
        list.iter()
            .map(|e| match *e {
                ListEntry::Ref(i) => d.frames()[i].surface().copied(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sliding_window_keeps_the_newest() {
        let d = filled(6, 3);
        let nums: Vec<u32> = d.frames().iter().map(|f| f.frame_num).collect();
        assert_eq!(nums, vec![3, 4, 5]);
    }

    #[test]
    fn p_list_is_descending_pic_num_then_long_term() {
        let mut d = filled(4, 4);
        // Frame 1 → long-term index 0 (MMCO 3 from picture 4: 4 − (2+1) = 1).
        d.mark_and_store(
            &Marking::Adaptive(vec![
                Mmco::MaxLongTermIdx { plus1: 1 },
                Mmco::ShortToLongTerm {
                    difference_minus1: 2,
                    long_term_frame_idx: 0,
                },
                Mmco::ForgetShortTerm {
                    difference_minus1: 3,
                },
            ]),
            cur(4, 4),
            Pixels::Decoded(4),
        );
        // Short-term 2,3,4 (0 forgotten by MMCO 1), long-term 1.
        let list = d.p_list(&p_slice(5, 4, vec![]), MAX).expect("list");
        assert_eq!(
            surfaces(&d, &list),
            vec![Some(4), Some(3), Some(2), Some(1)]
        );
        // Fewer active refs than frames: cut.
        let list = d.p_list(&p_slice(5, 2, vec![]), MAX).expect("list");
        assert_eq!(surfaces(&d, &list), vec![Some(4), Some(3)]);
        // More active refs than frames: padded with "no reference picture".
        let list = d.p_list(&p_slice(5, 6, vec![]), MAX).expect("list");
        assert_eq!(list[4..], [ListEntry::Empty, ListEntry::Empty]);
        assert!(d.decodable(&list));
    }

    #[test]
    fn frame_num_wrap_orders_across_the_wrap() {
        // Frames 14, 15, 0, 1 with MaxFrameNum 16: 1 is newest.
        let mut d: Dpb<u32> = Dpb::default();
        for n in [14, 15, 0, 1] {
            d.mark_and_store(&Marking::SlidingWindow, cur(n, 4), Pixels::Decoded(n));
        }
        let list = d.p_list(&p_slice(2, 4, vec![]), MAX).expect("list");
        assert_eq!(
            surfaces(&d, &list),
            vec![Some(1), Some(0), Some(15), Some(14)]
        );
        // The sliding window drops 14 (smallest FrameNumWrap), not 0.
        d.mark_and_store(&Marking::SlidingWindow, cur(2, 4), Pixels::Decoded(2));
        let nums: Vec<u32> = d.frames().iter().map(|f| f.frame_num).collect();
        assert_eq!(nums, vec![15, 0, 1, 2]);
    }

    #[test]
    fn modification_moves_a_picture_to_the_front() {
        let d = filled(4, 4);
        // Current 4: pred 4 − (1+1) = 2 → frame 2 first, then the rest in
        // initial order without its duplicate.
        let list = d
            .p_list(&p_slice(4, 3, vec![Modification::ShortTermSub(1)]), MAX)
            .expect("list");
        assert_eq!(surfaces(&d, &list), vec![Some(2), Some(3), Some(1)]);
        // Chained: pred 4 − 1 = 3 picks frame 3; then pred 3 + 2 = 5, which is
        // past the current frame_num, so PicNum 5 − 16 = −11: a frame 5 of
        // the previous wrap, which the buffer does not hold → Missing.
        let list = d
            .p_list(
                &p_slice(
                    4,
                    2,
                    vec![Modification::ShortTermSub(0), Modification::ShortTermAdd(1)],
                ),
                MAX,
            )
            .expect("list");
        assert_eq!(list[0], ListEntry::Ref(3));
        assert_eq!(list[1], ListEntry::Missing);
        assert!(!d.decodable(&list));
        // Too many commands.
        assert!(d
            .p_list(
                &p_slice(
                    4,
                    1,
                    vec![Modification::ShortTermSub(0), Modification::ShortTermSub(0)]
                ),
                MAX
            )
            .is_err());
    }

    #[test]
    fn long_term_modification_and_mmco6() {
        let mut d = filled(2, 3);
        // Picture 2 marks itself long-term 0.
        d.mark_and_store(
            &Marking::Adaptive(vec![
                Mmco::MaxLongTermIdx { plus1: 2 },
                Mmco::CurrentToLongTerm {
                    long_term_frame_idx: 0,
                },
            ]),
            cur(2, 3),
            Pixels::Decoded(2),
        );
        assert_eq!(d.frames().last().and_then(|f| f.long_term_idx), Some(0));
        let list = d
            .p_list(&p_slice(3, 2, vec![Modification::LongTerm(0)]), MAX)
            .expect("list");
        assert_eq!(surfaces(&d, &list), vec![Some(2), Some(1)]);
        // A long-term index nobody has → Missing.
        let list = d
            .p_list(&p_slice(3, 1, vec![Modification::LongTerm(1)]), MAX)
            .expect("list");
        assert_eq!(list, vec![ListEntry::Missing]);
        // MMCO 4 with plus1 = 0 drops every long-term frame.
        d.mark_and_store(
            &Marking::Adaptive(vec![Mmco::MaxLongTermIdx { plus1: 0 }]),
            cur(3, 3),
            Pixels::Decoded(3),
        );
        assert!(d.frames().iter().all(|f| f.long_term_idx.is_none()));
    }

    #[test]
    fn idr_long_term_reference_flag() {
        let mut d: Dpb<u32> = filled(3, 4);
        d.mark_and_store(
            &Marking::Idr {
                long_term_reference: true,
            },
            cur(0, 4),
            Pixels::Decoded(9),
        );
        assert_eq!(d.frames().len(), 1);
        assert_eq!(d.frames()[0].long_term_idx, Some(0));
    }

    #[test]
    fn mmco5_rebases_the_picture() {
        let mut d = filled(3, 4);
        let m = d.mark_and_store(
            &Marking::Adaptive(vec![Mmco::ForgetAll]),
            Current {
                frame_num: 7,
                max_frame_num: MAX,
                poc: [14, 15],
                max_refs: 4,
            },
            Pixels::Decoded(7),
        );
        assert!(m.mmco5);
        assert_eq!(d.frames().len(), 1);
        assert_eq!(d.frames()[0].frame_num, 0);
        assert_eq!(d.frames()[0].poc, [0, 1]);
    }

    #[test]
    fn gaps_insert_frames_that_slide_the_window() {
        let mut d = filled(2, 3);
        d.fill_gap(2, 5, MAX, 3, Pixels::Lost);
        let nums: Vec<u32> = d.frames().iter().map(|f| f.frame_num).collect();
        assert_eq!(nums, vec![2, 3, 4]);
        let list = d.p_list(&p_slice(5, 1, vec![]), MAX).expect("list");
        // The newest reference is an inferred (lost) frame: not decodable.
        assert!(!d.decodable(&list));
        let mut d = filled(2, 3);
        d.fill_gap(2, 4, MAX, 3, Pixels::NonExisting);
        let list = d.p_list(&p_slice(4, 1, vec![]), MAX).expect("list");
        assert!(d.decodable(&list), "allowed gaps are not losses");
    }

    #[test]
    fn overfull_buffer_is_trimmed_without_dropping_the_current() {
        let mut d = filled(3, 3);
        // Make every stored frame long-term, then store one more with an
        // MMCO list that frees nothing: the lowest long-term index goes.
        d.mark_and_store(
            &Marking::Adaptive(vec![
                Mmco::MaxLongTermIdx { plus1: 4 },
                Mmco::ShortToLongTerm {
                    difference_minus1: 0,
                    long_term_frame_idx: 2,
                },
                Mmco::ShortToLongTerm {
                    difference_minus1: 1,
                    long_term_frame_idx: 1,
                },
                Mmco::ShortToLongTerm {
                    difference_minus1: 2,
                    long_term_frame_idx: 3,
                },
            ]),
            cur(3, 3),
            Pixels::Decoded(3),
        );
        assert_eq!(d.frames().len(), 3);
        assert_eq!(d.frames().last().map(|f| f.frame_num), Some(3));
        assert!(d.frames().iter().all(|f| f.long_term_idx != Some(1)));
    }

    #[test]
    fn poc_type_2_and_type_0() {
        let (sps, _) = main_sets();
        let mut st = PocState::default();
        let mut idr = p_slice(0, 0, vec![]);
        idr.idr = true;
        let poc = st.compute(&sps, &idr);
        assert_eq!((poc.top, poc.bottom), (0, 0));
        st.advance(&idr, &poc, false);
        let p = p_slice(1, 1, vec![]);
        let poc = st.compute(&sps, &p);
        assert_eq!(poc.top, 2);
        st.advance(&p, &poc, false);
        let mut nonref = p_slice(2, 1, vec![]);
        nonref.nal_ref_idc = 0;
        assert_eq!(st.compute(&sps, &nonref).top, 3);
        // Wrap of frame_num (max 65536 here) advances FrameNumOffset.
        let mut st = PocState {
            prev_frame_num: 65535,
            prev_frame_num_offset: 0,
            ..PocState::default()
        };
        let poc = st.compute(&sps, &p_slice(0, 1, vec![]));
        assert_eq!(poc.top, 2 * 65536);
        st.advance(&p_slice(0, 1, vec![]), &poc, false);
        assert_eq!(st.prev_frame_num_offset, 65536);

        // Type 0 with 4-bit LSBs (MaxPicOrderCntLsb 16): 6, 12, then 2 wraps
        // the MSB up by 16.
        let mut sps0 = sps.clone();
        sps0.poc = PocType::Lsb { log2_max_lsb: 4 };
        let mut st = PocState::default();
        for (frame_num, lsb) in [(1, 6), (2, 12)] {
            let mut s = p_slice(frame_num, 1, vec![]);
            s.pic_order_cnt_lsb = lsb;
            let poc = st.compute(&sps0, &s);
            assert_eq!(poc.top, lsb as i64);
            st.advance(&s, &poc, false);
        }
        let mut b = p_slice(3, 1, vec![]);
        b.pic_order_cnt_lsb = 2;
        b.delta_pic_order_cnt_bottom = 1;
        let pb = st.compute(&sps0, &b);
        assert_eq!((pb.top, pb.bottom), (18, 19));
        // LSB 2 → 15 is more than half the range up: the MSB wraps down.
        let mut back = p_slice(4, 1, vec![]);
        back.pic_order_cnt_lsb = 15;
        let mut st_back = st;
        st_back.advance(&b, &pb, false);
        assert_eq!(st_back.compute(&sps0, &back).top, 15);
        // After MMCO 5 the next picture counts from the rebased POC.
        st.advance(&b, &pb, true);
        let mut c = p_slice(1, 1, vec![]);
        c.pic_order_cnt_lsb = 4;
        assert_eq!(st.compute(&sps0, &c).top, 4);
        // Non-reference pictures do not move the type-0 anchor.
        let mut n = p_slice(2, 1, vec![]);
        n.nal_ref_idc = 0;
        n.pic_order_cnt_lsb = 9;
        let pn = st.compute(&sps0, &n);
        let mut st2 = st;
        st2.advance(&n, &pn, false);
        assert_eq!(st2.prev_lsb, st.prev_lsb);
    }

    #[test]
    fn poc_type_1_cycle() {
        let (mut sps, _) = main_sets();
        sps.poc = PocType::Cycle {
            delta_pic_order_always_zero: true,
            offset_for_non_ref_pic: -1,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: vec![2, 4],
        };
        let st = PocState::default();
        // absFrameNum 3: one full cycle (6) + offsets[0] (2) = 8.
        assert_eq!(st.compute(&sps, &p_slice(3, 1, vec![])).top, 8);
        let mut nonref = p_slice(3, 1, vec![]);
        nonref.nal_ref_idc = 0;
        // absFrameNum 2: offsets[0] + offsets[1] = 6, then −1.
        assert_eq!(st.compute(&sps, &nonref).top, 5);
    }
}
