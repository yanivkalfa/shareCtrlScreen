//! Pure geometry for the X11 capture: Damage rectangles → the dirty list the
//! host receives, and X image row sizes. No X11 types, so every rule here is
//! unit-tested without a server.

use platform_api::Rect;

/// Most dirty rectangles reported for one frame. DXGI reports a handful per
/// frame and encoders that use them (ROI, skip-unchanged) want a short list;
/// every rectangle is also one `GetImage` request, so a bounded list bounds
/// the per-frame request count too.
pub(crate) const MAX_DIRTY: usize = 16;

/// Above this many rectangles after touching ones are merged, the damage is
/// scattered over the whole screen (snow, a busy terminal at tiny font size):
/// the pairwise merge below is quadratic, so take the bounding box instead.
const SCATTERED: usize = 64;

/// Damage regions with more pieces than this skip merging entirely. A
/// `FetchRegion` reply is YX-banded, so one ragged shape can arrive as
/// thousands of slivers; merging those one by one would cost more than reading
/// their bounding box.
const MAX_PIECES: usize = 1024;

fn right(r: &Rect) -> i64 {
    r.x as i64 + r.w as i64
}

fn bottom(r: &Rect) -> i64 {
    r.y as i64 + r.h as i64
}

/// The smallest rectangle containing both.
fn union(a: &Rect, b: &Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        w: (right(a).max(right(b)) - x as i64) as u32,
        h: (bottom(a).max(bottom(b)) - y as i64) as u32,
    }
}

/// Overlapping or sharing an edge. Touching rectangles are merged too: their
/// union along a shared edge loses nothing, and it is what turns a banded
/// region (one rectangle per scanline run) back into one rectangle.
fn touches(a: &Rect, b: &Rect) -> bool {
    a.x as i64 <= right(b)
        && b.x as i64 <= right(a)
        && a.y as i64 <= bottom(b)
        && b.y as i64 <= bottom(a)
}

fn bounding_box(rects: &[Rect]) -> Option<Rect> {
    let (first, rest) = rects.split_first()?;
    Some(rest.iter().fold(*first, |acc, r| union(&acc, r)))
}

/// `r` (root-window coordinates) clipped to `area` and expressed relative to
/// its top-left corner; `None` when nothing of `r` lies inside.
pub(crate) fn to_local(r: Rect, area: Rect) -> Option<Rect> {
    let x0 = (r.x as i64).max(area.x as i64);
    let y0 = (r.y as i64).max(area.y as i64);
    let x1 = right(&r).min(right(&area));
    let y1 = bottom(&r).min(bottom(&area));
    (x1 > x0 && y1 > y0).then(|| Rect {
        x: (x0 - area.x as i64) as i32,
        y: (y0 - area.y as i64) as i32,
        w: (x1 - x0) as u32,
        h: (y1 - y0) as u32,
    })
}

/// Merge until no two rectangles touch. The result is pairwise disjoint,
/// which the grab relies on: disjoint rectangles inside the captured area add
/// up to at most its size, so they always fit the shared-memory segment.
fn absorb(mut pending: Vec<Rect>) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::with_capacity(pending.len());
    while let Some(mut r) = pending.pop() {
        // Growing `r` can make it reach rectangles it missed earlier in the
        // pass, so repeat until a whole pass absorbs nothing.
        loop {
            let before = out.len();
            let mut i = 0;
            while i < out.len() {
                if touches(&r, &out[i]) {
                    r = union(&r, &out.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            if out.len() == before {
                break;
            }
        }
        out.push(r);
    }
    out
}

/// Damage rectangles (already clipped to the captured area, empty ones
/// dropped) → at most [`MAX_DIRTY`] pairwise-disjoint rectangles covering all
/// of them. Coverage is never lost; precision is traded for count by merging
/// the pair whose union adds the least area.
pub(crate) fn merge_dirty(rects: Vec<Rect>) -> Vec<Rect> {
    let rects: Vec<Rect> = rects.into_iter().filter(|r| r.area() > 0).collect();
    if rects.len() > MAX_PIECES {
        return bounding_box(&rects).into_iter().collect();
    }
    let mut out = absorb(rects);
    if out.len() > SCATTERED {
        return bounding_box(&out).into_iter().collect();
    }
    while out.len() > MAX_DIRTY {
        let mut best = (0, 1, u64::MAX);
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                // Disjoint inputs: the union's area is at least the sum, so
                // the growth never underflows.
                let growth = union(&out[i], &out[j]).area() - out[i].area() - out[j].area();
                if growth < best.2 {
                    best = (i, j, growth);
                }
            }
        }
        let (i, j, _) = best;
        let merged = union(&out[i], &out[j]);
        // Remove the higher index first so the lower one stays valid.
        out.swap_remove(j);
        out.swap_remove(i);
        out.push(merged);
        // The union may now touch a third rectangle; restore disjointness.
        out = absorb(out);
    }
    out
}

/// Bytes per row of a Z-format X image `width` pixels wide (X11 protocol,
/// "Image formats": each scanline is padded to a multiple of `scanline_pad`
/// bits).
pub(crate) fn row_bytes(width: u32, bits_per_pixel: u8, scanline_pad: u8) -> usize {
    let bits = width as usize * bits_per_pixel as usize;
    let pad = (scanline_pad as usize).max(8);
    bits.div_ceil(pad) * pad / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    fn covers(out: &[Rect], px: (i32, i32)) -> bool {
        out.iter().any(|o| {
            px.0 >= o.x && px.1 >= o.y && (px.0 as i64) < right(o) && (px.1 as i64) < bottom(o)
        })
    }

    fn disjoint(out: &[Rect]) -> bool {
        out.iter()
            .enumerate()
            .all(|(i, a)| out[i + 1..].iter().all(|b| !touches(a, b)))
    }

    #[test]
    fn to_local_clips_and_translates() {
        let monitor = r(1920, 0, 1280, 1024);
        // Entirely on the other monitor.
        assert_eq!(to_local(r(100, 100, 50, 50), monitor), None);
        // Straddling the left edge: only the part on this monitor, relative to it.
        assert_eq!(
            to_local(r(1900, 10, 40, 20), monitor),
            Some(r(0, 10, 20, 20))
        );
        // Past the bottom-right corner.
        assert_eq!(
            to_local(r(3100, 1000, 500, 500), monitor),
            Some(r(1180, 1000, 100, 24))
        );
        // Touching the edge from outside is not inside.
        assert_eq!(to_local(r(1900, 0, 20, 20), monitor), None);
    }

    #[test]
    fn banded_slivers_become_one_rectangle() {
        // A FetchRegion reply for a 10x4 block arrives as one rect per row.
        let slivers = (0..4).map(|y| r(5, 20 + y, 10, 1)).collect();
        assert_eq!(merge_dirty(slivers), vec![r(5, 20, 10, 4)]);
    }

    #[test]
    fn separate_regions_stay_separate() {
        let out = merge_dirty(vec![r(0, 0, 10, 10), r(100, 100, 10, 10)]);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&r(0, 0, 10, 10)) && out.contains(&r(100, 100, 10, 10)));
    }

    #[test]
    fn chains_merge_through_a_grown_rectangle() {
        // a and c only meet once a has absorbed b.
        let out = merge_dirty(vec![r(0, 0, 10, 10), r(30, 0, 10, 10), r(10, 0, 20, 2)]);
        assert_eq!(out, vec![r(0, 0, 40, 10)]);
    }

    #[test]
    fn many_rectangles_are_capped_without_losing_coverage() {
        // A grid of 7x7 separated squares: 49 inputs.
        let input: Vec<Rect> = (0..7)
            .flat_map(|gy| (0..7).map(move |gx| r(gx * 40, gy * 40, 8, 8)))
            .collect();
        let out = merge_dirty(input.clone());
        assert!(out.len() <= MAX_DIRTY, "{} rects", out.len());
        assert!(disjoint(&out));
        for i in &input {
            assert!(covers(&out, (i.x, i.y)) && covers(&out, (i.x + 7, i.y + 7)));
        }
    }

    #[test]
    fn scattered_and_huge_inputs_collapse_to_the_bounding_box() {
        let scattered: Vec<Rect> = (0..100).map(|i| r(i * 20, (i % 7) * 20, 2, 2)).collect();
        assert_eq!(merge_dirty(scattered), vec![r(0, 0, 1982, 122)]);
        let slivers: Vec<Rect> = (0..2000).map(|i| r((i % 3) * 50, i, 1, 1)).collect();
        assert_eq!(merge_dirty(slivers), vec![r(0, 0, 101, 2000)]);
    }

    #[test]
    fn empty_input_and_empty_rects() {
        assert!(merge_dirty(Vec::new()).is_empty());
        assert!(merge_dirty(vec![r(5, 5, 0, 10), r(1, 1, 3, 0)]).is_empty());
    }

    #[test]
    fn row_bytes_follow_scanline_padding() {
        assert_eq!(row_bytes(1920, 32, 32), 7680);
        assert_eq!(row_bytes(1, 32, 32), 4);
        // A server padding scanlines to 64 bits rounds odd widths up.
        assert_eq!(row_bytes(3, 32, 64), 16);
        assert_eq!(row_bytes(4, 32, 64), 16);
        // 24 bpp packed (never accepted for capture, but the arithmetic holds).
        assert_eq!(row_bytes(5, 24, 32), 16);
    }
}
