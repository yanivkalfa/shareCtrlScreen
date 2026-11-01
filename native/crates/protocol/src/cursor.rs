//! The host's real cursor image on the wire (task D9).
//!
//! The pointer *position* stays on the unreliable `cursor` data channel
//! ([`ControlMsg::Cursor`], unchanged, latency first). The *image* is sent as
//! [`ControlMsg::CursorShape`] parts on the reliable, ordered `ctl` channel:
//! an image is only sent when it changes, so losing one would leave the viewer
//! drawing the wrong cursor until the next change, and the `cursor` channel
//! never retransmits.
//!
//! * Pixel format: **premultiplied** BGRA, rows top-down, no padding. Capture
//!   backends hand the host straight alpha (`platform_api::CursorShape`); the
//!   host converts once, so there is exactly one format on the wire and the
//!   viewer uploads it as-is for `ONE, ONE_MINUS_SRC_ALPHA` blending.
//! * Identity: every image has a host-chosen `id`, stable for identical
//!   content. Position messages name the current image in their `shape` field;
//!   `None` there means "no image: draw the built-in arrow" — exactly what an
//!   older host (which never sends images) produces.
//! * Caching: both ends remember the last [`CACHE_SHAPES`] images in insertion
//!   order (oldest evicted first). The host inserts when it sends, the viewer
//!   when an image completes; `ctl` is reliable and ordered, so the two caches
//!   hold the same ids and a cursor seen before is switched to by id alone.
//! * Size: at most [`MAX_SIDE`] px per side (256 × 256 × 4 = 256 KiB). Each
//!   part carries [`CHUNK_BYTES`] of pixels as hex, ~32 KiB of JSON, well
//!   under the 64 KiB SCTP message size a peer that advertises none may assume.
//! * Compatibility: an older viewer fails to parse the unknown `"curshape"`
//!   type and ignores it (the ctl handler drops unparsable messages); the
//!   `shape` id in position messages was already optional and ignored.
//!
//! The viewer treats every part as hostile input: sizes are bounded before
//! anything is allocated, parts must arrive complete and in order, and colour
//! bytes are clamped to their alpha so a malformed image cannot over-brighten
//! what it is blended onto.
//!
//! ## References
//! * RFC 8831 §6.6 (WebRTC data channels): a peer that does not advertise
//!   `a=max-message-size` may be assumed to accept 64 KiB messages; str0m
//!   0.21 `sctp::DEFAULT_REMOTE_MAX_MESSAGE_SIZE`.
//! * Premultiplied ("associated") alpha and why filtering needs it: Porter &
//!   Duff, "Compositing Digital Images", SIGGRAPH 1984; Microsoft,
//!   "Premultiplied alpha" (Direct2D docs, `D2D1_ALPHA_MODE_PREMULTIPLIED`).

use serde::{Deserialize, Serialize};

use crate::control::ControlMsg;

/// Largest image side, in pixels, the host sends and the viewer accepts.
/// Windows' largest accessibility cursor is 256 px; nothing real is bigger,
/// and the cap bounds what a hostile host can make a viewer allocate.
pub const MAX_SIDE: u32 = 256;

/// Pixel bytes per [`CursorShapePart`] (before hex): a 32×32 cursor fits in
/// one part, the 256×256 maximum takes 16.
pub const CHUNK_BYTES: usize = 16 * 1024;

/// How many images each end remembers (see the module docs for why both ends
/// must use the same number and order).
pub const CACHE_SHAPES: usize = 32;

/// One part of a cursor image ([`ControlMsg::CursorShape`]). The image fields
/// repeat in every part so a part can be checked on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShapePart {
    pub id: u32,
    /// Image size in the host's captured-frame pixels.
    pub w: u32,
    pub h: u32,
    /// Hotspot: the image pixel that sits on the pointer position.
    pub hx: u32,
    pub hy: u32,
    /// The captured frame size the image is measured in, so the viewer scales
    /// it with the video (`w / fw` of the picture's width).
    pub fw: u32,
    pub fh: u32,
    /// `0..parts`, sent in order.
    pub part: u32,
    pub parts: u32,
    /// Lower-case hex of this part's premultiplied BGRA bytes.
    pub data: String,
}

/// A complete cursor image: premultiplied BGRA, see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeImage {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hotspot: (u32, u32),
    pub frame: (u32, u32),
    pub bgra: Vec<u8>,
}

impl ShapeImage {
    /// Whether the header is within the wire limits and matches the pixels.
    pub fn is_valid(&self) -> bool {
        header_ok(
            self.width,
            self.height,
            self.hotspot.0,
            self.hotspot.1,
            self.frame,
        ) && self.bgra.len() == image_bytes(self.width, self.height)
    }

    /// The image as `ctl` messages, in send order. Empty for an image outside
    /// the wire limits (the host checks and logs before calling this).
    pub fn to_messages(&self) -> Vec<ControlMsg> {
        if !self.is_valid() {
            return Vec::new();
        }
        let parts = self.bgra.len().div_ceil(CHUNK_BYTES) as u32;
        self.bgra
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(i, chunk)| {
                ControlMsg::CursorShape(CursorShapePart {
                    id: self.id,
                    w: self.width,
                    h: self.height,
                    hx: self.hotspot.0,
                    hy: self.hotspot.1,
                    fw: self.frame.0,
                    fh: self.frame.1,
                    part: i as u32,
                    parts,
                    data: hex::encode(chunk),
                })
            })
            .collect()
    }
}

fn image_bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 4
}

fn header_ok(w: u32, h: u32, hx: u32, hy: u32, frame: (u32, u32)) -> bool {
    (1..=MAX_SIDE).contains(&w)
        && (1..=MAX_SIDE).contains(&h)
        && hx < w
        && hy < h
        && frame.0 > 0
        && frame.1 > 0
}

/// Viewer side: rebuilds images from their parts.
#[derive(Debug, Default)]
pub struct Assembler {
    /// The image being received: its first part (header) and the bytes so far.
    pending: Option<(CursorShapePart, Vec<u8>)>,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one part. Returns the image when this part completes it. A part
    /// that is malformed, out of order or from another image than the one in
    /// progress abandons that image (the host never interleaves two, so this
    /// only happens with a broken or hostile peer).
    pub fn push(&mut self, part: CursorShapePart) -> Option<ShapeImage> {
        let total = image_bytes(part.w, part.h);
        let header = header_ok(part.w, part.h, part.hx, part.hy, (part.fw, part.fh));
        if !header || part.parts as usize != total.div_ceil(CHUNK_BYTES) || part.part >= part.parts
        {
            self.pending = None;
            return None;
        }
        // Exact length check before decoding: a part is a full chunk except
        // the last, which carries the remainder.
        let offset = part.part as usize * CHUNK_BYTES;
        let want = (total - offset).min(CHUNK_BYTES);
        if part.data.len() != want * 2 {
            self.pending = None;
            return None;
        }
        let Ok(bytes) = hex::decode(&part.data) else {
            self.pending = None;
            return None;
        };
        if part.part == 0 {
            let mut buf = Vec::with_capacity(total);
            buf.extend_from_slice(&bytes);
            self.pending = Some((
                CursorShapePart {
                    data: String::new(),
                    ..part
                },
                buf,
            ));
        } else {
            let continues = self
                .pending
                .as_ref()
                .is_some_and(|(head, buf)| same_image(head, &part) && buf.len() == offset);
            if !continues {
                self.pending = None;
                return None;
            }
            if let Some((_, buf)) = self.pending.as_mut() {
                buf.extend_from_slice(&bytes);
            }
        }
        let done = self
            .pending
            .as_ref()
            .is_some_and(|(_, buf)| buf.len() == total);
        if !done {
            return None;
        }
        let (head, mut bgra) = self.pending.take()?;
        // Premultiplied colour never exceeds alpha; clamp so a malformed image
        // cannot add light to what it is blended onto.
        for px in bgra.as_chunks_mut::<4>().0 {
            let a = px[3];
            for c in &mut px[..3] {
                *c = (*c).min(a);
            }
        }
        Some(ShapeImage {
            id: head.id,
            width: head.w,
            height: head.h,
            hotspot: (head.hx, head.hy),
            frame: (head.fw, head.fh),
            bgra,
        })
    }
}

fn same_image(a: &CursorShapePart, b: &CursorShapePart) -> bool {
    (a.id, a.w, a.h, a.hx, a.hy, a.fw, a.fh, a.parts)
        == (b.id, b.w, b.h, b.hx, b.hy, b.fw, b.fh, b.parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: u32, w: u32, h: u32) -> ShapeImage {
        let mut bgra = Vec::with_capacity(image_bytes(w, h));
        for i in 0..w * h {
            let a = (i % 256) as u8;
            bgra.extend_from_slice(&[a / 2, a / 3, a, a]);
        }
        ShapeImage {
            id,
            width: w,
            height: h,
            hotspot: (w / 2, h - 1),
            frame: (1920, 1080),
            bgra,
        }
    }

    fn parts(img: &ShapeImage) -> Vec<CursorShapePart> {
        img.to_messages()
            .into_iter()
            .map(|m| match m {
                ControlMsg::CursorShape(p) => p,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_small_cursor_is_one_part_and_round_trips() {
        let img = image(7, 32, 32);
        let ps = parts(&img);
        assert_eq!(ps.len(), 1);
        let mut a = Assembler::new();
        assert_eq!(a.push(ps[0].clone()), Some(img));
    }

    #[test]
    fn the_largest_cursor_is_split_and_reassembled_through_json() {
        let img = image(9, MAX_SIDE, MAX_SIDE);
        let msgs = img.to_messages();
        assert_eq!(msgs.len(), 16);
        let mut a = Assembler::new();
        let mut out = None;
        for m in msgs {
            let json = serde_json::to_vec(&m).unwrap();
            // Stays under the 64 KiB SCTP message size every peer accepts.
            assert!(json.len() < 64 * 1024, "{} bytes", json.len());
            match serde_json::from_slice::<ControlMsg>(&json).unwrap() {
                ControlMsg::CursorShape(p) => {
                    assert!(out.is_none(), "completed early");
                    out = a.push(p);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(out, Some(img));
    }

    #[test]
    fn wire_shape_is_tagged_curshape() {
        let s = serde_json::to_string(&image(1, 1, 1).to_messages()[0]).unwrap();
        assert!(s.starts_with("{\"t\":\"curshape\""), "{s}");
    }

    #[test]
    fn out_of_order_or_foreign_parts_abandon_the_image() {
        let ps = parts(&image(3, 128, 128)); // 4 parts
        assert_eq!(ps.len(), 4);
        let mut a = Assembler::new();
        assert!(a.push(ps[0].clone()).is_none());
        assert!(a.push(ps[2].clone()).is_none(), "skipped part 1");
        assert!(a.push(ps[3].clone()).is_none(), "image was abandoned");
        // A part of another image in the middle abandons this one too.
        let other = parts(&image(4, 128, 128));
        assert!(a.push(ps[0].clone()).is_none());
        assert!(a.push(other[1].clone()).is_none());
        assert!(a.push(ps[1].clone()).is_none());
        // A clean run still completes afterwards.
        let mut done = None;
        for p in ps {
            done = a.push(p);
        }
        assert!(done.is_some());
    }

    #[test]
    fn hostile_headers_are_refused_before_allocating() {
        let mut a = Assembler::new();
        let good = parts(&image(5, 4, 4)).remove(0);
        for bad in [
            CursorShapePart {
                w: MAX_SIDE + 1,
                ..good.clone()
            },
            CursorShapePart {
                h: 0,
                ..good.clone()
            },
            CursorShapePart {
                hx: 4,
                ..good.clone()
            },
            CursorShapePart {
                fw: 0,
                ..good.clone()
            },
            CursorShapePart {
                parts: 2,
                ..good.clone()
            },
            CursorShapePart {
                part: 1,
                ..good.clone()
            },
            CursorShapePart {
                data: good.data[2..].to_string(),
                ..good.clone()
            },
            CursorShapePart {
                data: "zz".repeat(good.data.len() / 2),
                ..good.clone()
            },
        ] {
            assert!(a.push(bad.clone()).is_none(), "{bad:?}");
        }
        assert!(a.push(good).is_some());
    }

    #[test]
    fn colour_above_alpha_is_clamped() {
        let mut img = image(6, 1, 1);
        img.bgra = vec![200, 10, 255, 100];
        let mut a = Assembler::new();
        let out = a.push(parts(&img).remove(0)).unwrap();
        assert_eq!(out.bgra, vec![100, 10, 100, 100]);
    }

    #[test]
    fn invalid_images_produce_no_messages() {
        let mut img = image(8, 4, 4);
        img.bgra.pop();
        assert!(img.to_messages().is_empty());
    }
}
