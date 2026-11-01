//! Damage (changed regions) of captured frames, from `SPA_META_VideoDamage`.
//!
//! Pure logic, unit-tested without PipeWire.

use platform_api::Rect;

/// More rectangles than this collapse into their bounding box. The compositor's
/// damage array is itself bounded (the meta size we negotiate holds
/// [`super::params::DAMAGE_REGIONS`] entries), and an encoder gains nothing from a
/// confetti of tiny rects.
pub(crate) const MAX_RECTS: usize = 16;

/// What changed in a frame since the previous one the consumer took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Damage {
    /// Assume every pixel changed (no damage metadata, or it covered the frame).
    Full,
    /// Only these rectangles changed: non-empty, inside the frame, none contained in
    /// another, at most [`MAX_RECTS`].
    Rects(Vec<Rect>),
}

impl Damage {
    /// From the compositor's regions (`x`, `y`, `w`, `h` in frame pixels). Clamped to the
    /// frame because a region can outlive a resize by a buffer. No usable region means
    /// the compositor told us nothing, which must be read as "everything": a damaged
    /// pixel left out would never be re-sent.
    pub(crate) fn from_regions(
        regions: impl IntoIterator<Item = (i32, i32, u32, u32)>,
        frame: (u32, u32),
    ) -> Damage {
        let rects: Vec<Rect> = regions
            .into_iter()
            .filter_map(|r| clamp(r, frame))
            .collect();
        if rects.is_empty() {
            return Damage::Full;
        }
        normalize(rects, frame)
    }

    /// The damage of a frame that replaced an older, never-consumed one. The older
    /// frame's pixels are gone but its changes are in the newer picture too, so they
    /// must still be reported: newest-wins delivery drops frames, never damage.
    pub(crate) fn union(self, newer: Damage, frame: (u32, u32)) -> Damage {
        match (self, newer) {
            (Damage::Rects(mut a), Damage::Rects(b)) => {
                a.extend(b);
                normalize(a, frame)
            }
            _ => Damage::Full,
        }
    }

    /// The trait's encoding: an empty list means "assume everything changed".
    pub(crate) fn into_dirty(self) -> Vec<Rect> {
        match self {
            Damage::Full => Vec::new(),
            Damage::Rects(r) => r,
        }
    }
}

fn clamp((x, y, w, h): (i32, i32, u32, u32), (fw, fh): (u32, u32)) -> Option<Rect> {
    // i64 so a region at i32::MAX with a large width cannot overflow.
    let x0 = i64::from(x).clamp(0, i64::from(fw));
    let y0 = i64::from(y).clamp(0, i64::from(fh));
    let x1 = (i64::from(x) + i64::from(w)).clamp(0, i64::from(fw));
    let y1 = (i64::from(y) + i64::from(h)).clamp(0, i64::from(fh));
    // Clamped into 0..=frame size, and frame sizes are negotiated video sizes
    // (at most `params::MAX_SIDE`), so the casts below are lossless.
    (x1 > x0 && y1 > y0).then(|| Rect {
        x: x0 as i32,
        y: y0 as i32,
        w: (x1 - x0) as u32,
        h: (y1 - y0) as u32,
    })
}

fn contains(outer: &Rect, inner: &Rect) -> bool {
    let (ox1, oy1) = (
        i64::from(outer.x) + i64::from(outer.w),
        i64::from(outer.y) + i64::from(outer.h),
    );
    let (ix1, iy1) = (
        i64::from(inner.x) + i64::from(inner.w),
        i64::from(inner.y) + i64::from(inner.h),
    );
    outer.x <= inner.x && outer.y <= inner.y && ox1 >= ix1 && oy1 >= iy1
}

fn bounding_box(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = rects
        .iter()
        .map(|r| i64::from(r.x) + i64::from(r.w))
        .max()
        .unwrap_or(0);
    let y1 = rects
        .iter()
        .map(|r| i64::from(r.y) + i64::from(r.h))
        .max()
        .unwrap_or(0);
    Rect {
        x: x0,
        y: y0,
        w: (x1 - i64::from(x0)) as u32,
        h: (y1 - i64::from(y0)) as u32,
    }
}

/// Drop duplicates and rects inside other rects, collapse an overlong list into its
/// bounding box, and report a frame-covering result as [`Damage::Full`]. Input rects
/// are already clamped and non-empty.
fn normalize(mut rects: Vec<Rect>, frame: (u32, u32)) -> Damage {
    // Largest first, so every rect is checked against all bigger candidates.
    rects.sort_by_key(|r| std::cmp::Reverse(r.area()));
    let mut kept: Vec<Rect> = Vec::with_capacity(rects.len());
    for r in rects {
        if !kept.iter().any(|k| contains(k, &r)) {
            kept.push(r);
        }
    }
    if kept.len() > MAX_RECTS {
        kept = vec![bounding_box(&kept)];
    }
    let whole = Rect {
        x: 0,
        y: 0,
        w: frame.0,
        h: frame.1,
    };
    if kept.iter().any(|r| contains(r, &whole)) {
        return Damage::Full;
    }
    Damage::Rects(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (u32, u32) = (1920, 1080);

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn regions_are_clamped_to_the_frame_and_empty_ones_dropped() {
        let d = Damage::from_regions(
            [
                (-10, -10, 20, 20),
                (1900, 1070, 100, 100),
                (5, 5, 0, 10),
                (3000, 3000, 5, 5),
            ],
            FRAME,
        );
        assert_eq!(
            d,
            Damage::Rects(vec![rect(1900, 1070, 20, 10), rect(0, 0, 10, 10)])
        );
    }

    #[test]
    fn no_usable_region_means_everything_changed() {
        assert_eq!(Damage::from_regions([], FRAME), Damage::Full);
        assert_eq!(
            Damage::from_regions([(0, 0, 0, 0), (5000, 0, 1, 1)], FRAME),
            Damage::Full
        );
        assert_eq!(Damage::Full.into_dirty(), Vec::<Rect>::new());
    }

    #[test]
    fn a_region_covering_the_frame_is_full() {
        assert_eq!(
            Damage::from_regions([(0, 0, 1920, 1080), (10, 10, 5, 5)], FRAME),
            Damage::Full
        );
        assert_eq!(
            Damage::from_regions([(-5, -5, 4000, 4000)], FRAME),
            Damage::Full
        );
    }

    #[test]
    fn contained_and_duplicate_rects_are_dropped() {
        let d = Damage::from_regions(
            [
                (0, 0, 100, 100),
                (10, 10, 20, 20),
                (0, 0, 100, 100),
                (200, 0, 1, 1),
            ],
            FRAME,
        );
        assert_eq!(
            d,
            Damage::Rects(vec![rect(0, 0, 100, 100), rect(200, 0, 1, 1)])
        );
    }

    #[test]
    fn too_many_rects_collapse_into_their_bounding_box() {
        let many = (0..(MAX_RECTS as i32 + 1)).map(|i| (i * 10, i * 5, 2, 2));
        let d = Damage::from_regions(many, FRAME);
        let n = MAX_RECTS as i32;
        assert_eq!(
            d,
            Damage::Rects(vec![rect(0, 0, (n * 10 + 2) as u32, (n * 5 + 2) as u32)])
        );
    }

    #[test]
    fn union_keeps_the_damage_of_a_superseded_frame() {
        let old = Damage::from_regions([(0, 0, 10, 10)], FRAME);
        let new = Damage::from_regions([(100, 100, 10, 10)], FRAME);
        assert_eq!(
            old.union(new, FRAME),
            Damage::Rects(vec![rect(0, 0, 10, 10), rect(100, 100, 10, 10)])
        );
        let rects = Damage::from_regions([(0, 0, 10, 10)], FRAME);
        assert_eq!(rects.clone().union(Damage::Full, FRAME), Damage::Full);
        assert_eq!(Damage::Full.union(rects, FRAME), Damage::Full);
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let d = Damage::from_regions([(i32::MAX, i32::MAX, u32::MAX, u32::MAX)], FRAME);
        assert_eq!(d, Damage::Full);
        let d = Damage::from_regions([(i32::MIN, 0, u32::MAX, 1)], FRAME);
        assert_eq!(d, Damage::Rects(vec![rect(0, 0, 1920, 1)]));
    }
}
