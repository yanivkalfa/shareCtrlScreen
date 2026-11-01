//! The host's real cursor image on the viewer (task D9): which image is
//! current, which the viewer already has, and the viewer's cache of received
//! images. Wire format and limits: [`protocol::cursor`].
//!
//! Host: [`HostCursor`] turns each backend `CursorShape` (straight alpha) into
//! a premultiplied [`ShapeImage`] with a content-derived id, and only produces
//! messages for an id the viewer does not have yet. Every position message
//! names the current id, so switching back to a cursor seen before (arrow →
//! I-beam → arrow) costs nothing on the wire.
//!
//! Viewer: [`ViewerCursors`] reassembles parts and keeps the same
//! insertion-ordered cache as the host, so an id the host names without
//! re-sending is always found.
//!
//! ## References
//! * FNV-1a: Fowler, Noll, Vo, <http://www.isthe.com/chongo/tech/comp/fnv/>
//!   (64-bit offset basis 0xcbf29ce484222325, prime 0x100000001b3).
//! * Straight → premultiplied: `c' = c·a / 255` (Porter & Duff 1984); every
//!   capture backend documents why its `CursorShape` is straight alpha:
//!   DXGI `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR` / `_MASKED_COLOR` /
//!   `_MONOCHROME` (`platform-windows/src/adapters.rs`), CoreGraphics
//!   `kCGImageAlphaPremultipliedFirst` (`platform-macos/src/cursor.rs`),
//!   PipeWire `spa_meta_bitmap` (`platform-linux/src/portal_capture/pixels.rs`),
//!   XFixes `GetCursorImage` premultiplied ARGB (`platform-linux/src/capture/pixels.rs`).

use std::collections::VecDeque;
use std::sync::Arc;

use platform_api::{CursorShape, CursorSprite, Point};
use protocol::cursor::{Assembler, CursorShapePart, ShapeImage, CACHE_SHAPES, MAX_SIDE};
use protocol::ControlMsg;

/// FNV-1a over the image and everything that changes how it is drawn, folded
/// to the `u32` the position message carries. Collisions only matter between
/// the ≤ [`CACHE_SHAPES`] images alive in one session (odds ~ 32² / 2³³).
fn shape_id(img: &ShapeImage) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let header = [
        img.width,
        img.height,
        img.hotspot.0,
        img.hotspot.1,
        img.frame.0,
        img.frame.1,
    ];
    for b in header
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .chain(img.bgra.iter().copied())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h ^ (h >> 32)) as u32
}

/// Straight-alpha BGRA (what capture backends produce) → premultiplied, with
/// rounding. Fully transparent pixels become all-zero, so two images that
/// differ only in invisible colour get the same id.
pub(crate) fn premultiply(straight: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(straight.len());
    for px in straight.as_chunks::<4>().0 {
        let a = u32::from(px[3]);
        let mul = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
        out.extend_from_slice(&[mul(px[0]), mul(px[1]), mul(px[2]), px[3]]);
    }
    out
}

/// Host side: the current image and what the viewer already holds.
#[derive(Debug, Default)]
pub(crate) struct HostCursor {
    /// The backend's last shape. Kept because backends report a shape only when
    /// it changes, but the frame size it is measured against can change on
    /// its own (display mode change) and the id must follow.
    shape: Option<CursorShape>,
    /// Frame size the current id was computed for.
    frame: (u32, u32),
    /// The id position messages name; `None` = the viewer draws its arrow.
    current: Option<u32>,
    /// Ids sent this session, oldest first: a mirror of the viewer's cache
    /// (same capacity, same insertion order, reliable ordered channel).
    sent: VecDeque<u32>,
}

impl HostCursor {
    /// The id to put in position messages.
    pub(crate) fn current(&self) -> Option<u32> {
        self.current
    }

    /// Feed one captured frame's cursor shape (`None` when unchanged) and the
    /// frame's size. Returns the `ctl` messages that deliver a new image, in
    /// order; empty when the viewer already has the current one.
    pub(crate) fn update(
        &mut self,
        shape: Option<&CursorShape>,
        frame: (u32, u32),
    ) -> Vec<ControlMsg> {
        let changed = match shape {
            Some(s) => {
                self.shape = Some(s.clone());
                true
            }
            None => self.shape.is_some() && frame != self.frame,
        };
        if !changed {
            return Vec::new();
        }
        self.frame = frame;
        let Some(img) = self.shape.as_ref().and_then(|s| to_image(s, frame)) else {
            self.current = None;
            return Vec::new();
        };
        self.current = Some(img.id);
        if self.sent.contains(&img.id) {
            return Vec::new();
        }
        if self.sent.len() == CACHE_SHAPES {
            self.sent.pop_front();
        }
        self.sent.push_back(img.id);
        img.to_messages()
    }
}

/// A backend shape as a wire image, or `None` (logged) when it cannot be sent;
/// the viewer then draws its arrow, which beats a wrong or truncated image.
fn to_image(s: &CursorShape, frame: (u32, u32)) -> Option<ShapeImage> {
    if s.width == 0 || s.height == 0 || frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    if s.width > MAX_SIDE || s.height > MAX_SIDE {
        tracing::info!(
            "cursor: {}x{} image is over the {MAX_SIDE}px limit; the viewer draws its arrow",
            s.width,
            s.height
        );
        return None;
    }
    if s.bgra.len() != s.width as usize * s.height as usize * 4 {
        tracing::warn!(
            "cursor: {}x{} image came with {} bytes; not sent",
            s.width,
            s.height,
            s.bgra.len()
        );
        return None;
    }
    // A hotspot outside the image (a backend rounding a scaled hotspot) is
    // pulled onto its nearest edge pixel, so the wire header stays valid.
    let clamp = |v: i32, side: u32| v.clamp(0, side as i32 - 1) as u32;
    let mut img = ShapeImage {
        id: 0,
        width: s.width,
        height: s.height,
        hotspot: (clamp(s.hotspot.x, s.width), clamp(s.hotspot.y, s.height)),
        frame,
        bgra: premultiply(&s.bgra),
    };
    img.id = shape_id(&img);
    Some(img)
}

/// Viewer side: reassembly and the id → image cache.
#[derive(Debug, Default)]
pub(crate) struct ViewerCursors {
    assembler: Assembler,
    /// Oldest first; see [`HostCursor::sent`] for why the order matters.
    cache: VecDeque<(u32, Arc<CursorSprite>)>,
}

impl ViewerCursors {
    /// Feed one received part; returns the id of an image it completed.
    pub(crate) fn push(&mut self, part: CursorShapePart) -> Option<u32> {
        let img = self.assembler.push(part)?;
        let sprite = CursorSprite {
            width: img.width,
            height: img.height,
            hotspot: Point {
                x: img.hotspot.0 as i32,
                y: img.hotspot.1 as i32,
            },
            bgra: img.bgra,
            frame: img.frame,
        };
        if !sprite.is_valid() {
            return None;
        }
        // A re-sent id replaces its entry in place of being duplicated (the
        // host only re-sends after evicting it too, so this is defensive).
        self.cache.retain(|(id, _)| *id != img.id);
        if self.cache.len() == CACHE_SHAPES {
            self.cache.pop_front();
        }
        self.cache.push_back((img.id, Arc::new(sprite)));
        Some(img.id)
    }

    pub(crate) fn get(&self, id: u32) -> Option<Arc<CursorSprite>> {
        self.cache
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(w: u32, h: u32, seed: u8, hot: (i32, i32)) -> CursorShape {
        let mut bgra = Vec::new();
        for i in 0..w * h {
            let v = (i as u8).wrapping_add(seed);
            bgra.extend_from_slice(&[v, v / 2, 255 - v, if i % 3 == 0 { 0 } else { 255 }]);
        }
        CursorShape {
            width: w,
            height: h,
            hotspot: Point { x: hot.0, y: hot.1 },
            bgra,
        }
    }

    fn parts(msgs: Vec<ControlMsg>) -> Vec<CursorShapePart> {
        msgs.into_iter()
            .map(|m| match m {
                ControlMsg::CursorShape(p) => p,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn premultiply_rounds_and_zeroes_invisible_colour() {
        assert_eq!(
            premultiply(&[255, 128, 10, 255, 200, 200, 200, 0, 255, 255, 255, 128]),
            vec![255, 128, 10, 255, 0, 0, 0, 0, 128, 128, 128, 128]
        );
    }

    #[test]
    fn a_shape_is_sent_once_and_then_named_by_id() {
        let mut host = HostCursor::default();
        let arrow = shape(16, 16, 1, (0, 0));
        let beam = shape(8, 16, 2, (4, 8));
        let first = host.update(Some(&arrow), (1920, 1080));
        assert_eq!(first.len(), 1);
        let arrow_id = host.current().unwrap();
        // Same content again (the backend re-reported it): nothing to send.
        assert!(host.update(Some(&arrow), (1920, 1080)).is_empty());
        assert_eq!(host.current(), Some(arrow_id));
        // A new shape is sent; switching back is by id alone.
        assert_eq!(host.update(Some(&beam), (1920, 1080)).len(), 1);
        let beam_id = host.current().unwrap();
        assert_ne!(beam_id, arrow_id);
        assert!(host.update(Some(&arrow), (1920, 1080)).is_empty());
        assert_eq!(host.current(), Some(arrow_id));
        // No shape this frame: unchanged, no messages.
        assert!(host.update(None, (1920, 1080)).is_empty());
        assert_eq!(host.current(), Some(arrow_id));
    }

    #[test]
    fn a_frame_size_change_re_sends_the_image_with_the_new_scale() {
        let mut host = HostCursor::default();
        let arrow = shape(16, 16, 1, (0, 0));
        host.update(Some(&arrow), (1920, 1080));
        let before = host.current();
        let msgs = parts(host.update(None, (3840, 2160)));
        assert_eq!(msgs.len(), 1);
        assert_eq!((msgs[0].fw, msgs[0].fh), (3840, 2160));
        assert_ne!(host.current(), before);
    }

    #[test]
    fn oversized_or_broken_shapes_fall_back_to_the_arrow() {
        let mut host = HostCursor::default();
        host.update(Some(&shape(16, 16, 1, (0, 0))), (100, 100));
        assert!(host.current().is_some());
        assert!(host
            .update(Some(&shape(MAX_SIDE + 1, 4, 1, (0, 0))), (100, 100))
            .is_empty());
        assert_eq!(host.current(), None);
        let mut short = shape(4, 4, 1, (0, 0));
        short.bgra.pop();
        assert!(host.update(Some(&short), (100, 100)).is_empty());
        assert_eq!(host.current(), None);
    }

    #[test]
    fn hotspots_outside_the_image_are_clamped() {
        let mut host = HostCursor::default();
        let p = parts(host.update(Some(&shape(4, 6, 1, (-3, 99))), (10, 10)));
        assert_eq!((p[0].hx, p[0].hy), (0, 5));
    }

    #[test]
    fn host_and_viewer_caches_stay_in_step_past_capacity() {
        let mut host = HostCursor::default();
        let mut viewer = ViewerCursors::default();
        let shapes: Vec<CursorShape> = (0..CACHE_SHAPES as u8 + 5)
            .map(|i| shape(4, 4, i, (1, 1)))
            .collect();
        let mut ids = Vec::new();
        for s in &shapes {
            for p in parts(host.update(Some(s), (64, 64))) {
                viewer.push(p);
            }
            ids.push(host.current().unwrap());
        }
        // Whatever the host names without sending, the viewer has.
        for (i, s) in shapes.iter().enumerate().rev() {
            let msgs = host.update(Some(s), (64, 64));
            for p in parts(msgs) {
                viewer.push(p);
            }
            let id = host.current().unwrap();
            assert_eq!(id, ids[i]);
            let sprite = viewer.get(id).expect("viewer has every id the host names");
            assert_eq!(sprite.bgra, premultiply(&s.bgra));
        }
    }

    #[test]
    fn viewer_builds_a_sprite_with_the_hosts_geometry() {
        let mut host = HostCursor::default();
        let mut viewer = ViewerCursors::default();
        let s = shape(9, 16, 3, (4, 8));
        let mut done = None;
        for p in parts(host.update(Some(&s), (2880, 1800))) {
            done = viewer.push(p);
        }
        let id = done.unwrap();
        assert_eq!(Some(id), host.current());
        let sprite = viewer.get(id).unwrap();
        assert_eq!((sprite.width, sprite.height), (9, 16));
        assert_eq!(sprite.hotspot, Point { x: 4, y: 8 });
        assert_eq!(sprite.frame, (2880, 1800));
        assert!(viewer.get(id.wrapping_add(1)).is_none());
    }
}
