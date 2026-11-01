//! Pixel formats the stream accepts, and the copies out of PipeWire buffers into the
//! tightly packed BGRA the rest of the host consumes (`PixelFormat::Bgra8`).
//!
//! SPA names 32-bit formats by byte order in memory (`BGRx` = bytes B, G, R, x), the
//! GStreamer convention (spa/param/video/raw.h). Pure logic, unit-tested without
//! PipeWire.

use std::fmt;

use pipewire::spa::param::video::VideoFormat;
use platform_api::{CursorShape, Point};

/// Formats offered in `EnumFormat`, most preferred first. BGRx/BGRA are what the
/// encoders take (BGRA → NV12) and copy without a swizzle; the rest cost a per-pixel
/// shuffle but keep compositors that only produce them working.
pub(crate) const PREFERRED: [VideoFormat; 8] = [
    VideoFormat::BGRx,
    VideoFormat::BGRA,
    VideoFormat::RGBx,
    VideoFormat::RGBA,
    VideoFormat::xRGB,
    VideoFormat::ARGB,
    VideoFormat::xBGR,
    VideoFormat::ABGR,
];

/// Where each channel sits inside a 4-byte pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    b: usize,
    g: usize,
    r: usize,
    /// `None` for the `x` (padding) formats.
    a: Option<usize>,
}

impl Layout {
    /// Already in BGRA byte order: rows are copied as they are.
    fn is_bgra(self) -> bool {
        self.b == 0 && self.g == 1 && self.r == 2
    }
}

/// The layout of a 32-bit RGB format, `None` for anything else.
pub(crate) fn layout(format: VideoFormat) -> Option<Layout> {
    let l = |b, g, r, a| Some(Layout { b, g, r, a });
    match format {
        VideoFormat::BGRx => l(0, 1, 2, None),
        VideoFormat::BGRA => l(0, 1, 2, Some(3)),
        VideoFormat::RGBx => l(2, 1, 0, None),
        VideoFormat::RGBA => l(2, 1, 0, Some(3)),
        VideoFormat::xRGB => l(3, 2, 1, None),
        VideoFormat::ARGB => l(3, 2, 1, Some(0)),
        VideoFormat::xBGR => l(1, 2, 3, None),
        VideoFormat::ABGR => l(1, 2, 3, Some(0)),
        _ => None,
    }
}

/// Why a buffer could not be read. Logged once per kind; the frame is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyError {
    /// Bottom-up rows. No screen-cast producer emits them; refusing is safer than
    /// guessing where row 0 starts.
    NegativeStride(i32),
    /// A row is shorter than `width` pixels.
    StrideTooSmall { stride: usize, row: usize },
    /// The rows run past the mapped memory.
    OutOfBounds { needed: usize, available: usize },
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyError::NegativeStride(s) => write!(f, "negative stride {s} (bottom-up rows)"),
            CopyError::StrideTooSmall { stride, row } => {
                write!(f, "stride {stride} is shorter than a {row}-byte row")
            }
            CopyError::OutOfBounds { needed, available } => {
                write!(f, "needs {needed} bytes but only {available} are mapped")
            }
        }
    }
}

/// One plane as a buffer's `spa_chunk` describes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Plane {
    /// `chunk.offset`; spa/buffer/buffer.h: "should be taken modulo the data maxsize".
    pub offset: u32,
    /// `chunk.stride`; 0 = rows are packed (some producers leave it unset).
    pub stride: i32,
}

/// Bytes between row starts and the byte offset of row 0, validated against `mapped`
/// (the whole `maxsize` mapping of the data block).
fn rows(mapped: usize, plane: Plane, (w, h): (u32, u32)) -> Result<(usize, usize), CopyError> {
    let row = w as usize * 4;
    let stride = match plane.stride {
        s if s < 0 => return Err(CopyError::NegativeStride(s)),
        0 => row,
        s => s as usize,
    };
    if stride < row {
        return Err(CopyError::StrideTooSmall { stride, row });
    }
    let offset = if mapped == 0 {
        0
    } else {
        plane.offset as usize % mapped
    };
    let needed = match h as usize {
        0 => 0,
        h => stride * (h - 1) + row,
    };
    let available = mapped - offset;
    if needed > available {
        return Err(CopyError::OutOfBounds { needed, available });
    }
    Ok((stride, offset))
}

/// Copy a `size` picture out of `src` into `dst` as packed BGRA (stride = width × 4).
/// `dst` is reused across frames, so a steady stream allocates nothing. For the `x`
/// formats the alpha byte is undefined, as in the source: captured screens are opaque
/// and the host's BGRA → NV12 path never reads alpha.
pub(crate) fn copy_frame(
    src: &[u8],
    plane: Plane,
    size: (u32, u32),
    layout: Layout,
    dst: &mut Vec<u8>,
) -> Result<(), CopyError> {
    let (stride, offset) = rows(src.len(), plane, size)?;
    let row = size.0 as usize * 4;
    dst.clear();
    dst.reserve(row * size.1 as usize);
    for y in 0..size.1 as usize {
        let start = offset + y * stride;
        let line = &src[start..start + row];
        if layout.is_bgra() {
            dst.extend_from_slice(line);
        } else {
            for px in line.as_chunks::<4>().0 {
                let a = layout.a.map_or(0xff, |i| px[i]);
                dst.extend_from_slice(&[px[layout.b], px[layout.g], px[layout.r], a]);
            }
        }
    }
    Ok(())
}

/// Largest cursor bitmap accepted. Far above any real cursor (the meta size we
/// negotiate caps it lower still); only a corrupt header reaches it.
pub(crate) const MAX_CURSOR_SIDE: u32 = 1024;

/// Convert a `spa_meta_bitmap` cursor image to the trait's straight-alpha BGRA.
///
/// Mutter (`COGL_PIXEL_FORMAT_RGBA_8888_PRE`) and KWin
/// (`QImage::Format_RGBA8888_Premultiplied`) write premultiplied alpha, so the colour is
/// divided back out; without that every anti-aliased cursor edge would be drawn dark on
/// the viewer. A format without alpha is opaque. `None` for an unknown format, an empty
/// or oversized image, or rows that do not fit `data`.
pub(crate) fn cursor_shape(
    format: VideoFormat,
    (w, h): (u32, u32),
    stride: i32,
    data: &[u8],
    hotspot: Point,
) -> Option<CursorShape> {
    let layout = layout(format)?;
    if w == 0 || h == 0 || w > MAX_CURSOR_SIDE || h > MAX_CURSOR_SIDE {
        return None;
    }
    let plane = Plane { offset: 0, stride };
    let (stride, offset) = rows(data.len(), plane, (w, h)).ok()?;
    let mut bgra = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let start = offset + y * stride;
        for px in data[start..start + w as usize * 4].as_chunks::<4>().0 {
            let Some(ai) = layout.a else {
                bgra.extend_from_slice(&[px[layout.b], px[layout.g], px[layout.r], 0xff]);
                continue;
            };
            let a = px[ai];
            let straight = |c: u8| -> u8 {
                if a == 0 {
                    0
                } else {
                    // Rounded division; a premultiplied channel never exceeds alpha,
                    // but a producer that broke that rule must not wrap around.
                    ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
                }
            };
            bgra.extend_from_slice(&[
                straight(px[layout.b]),
                straight(px[layout.g]),
                straight(px[layout.r]),
                a,
            ]);
        }
    }
    let clamp = |v: i32, max: u32| v.clamp(0, max as i32 - 1);
    Some(CursorShape {
        width: w,
        height: h,
        hotspot: Point {
            x: clamp(hotspot.x, w),
            y: clamp(hotspot.y, h),
        },
        bgra,
    })
}

/// Whether a straight-alpha BGRA image shows anything. A fully transparent sprite is
/// treated as a hidden pointer, so the viewer draws nothing rather than an invisible
/// shape it might replace with a default arrow.
pub(crate) fn is_visible(shape: &CursorShape) -> bool {
    shape.bgra.as_chunks::<4>().0.iter().any(|px| px[3] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra() -> Layout {
        layout(VideoFormat::BGRx).unwrap()
    }

    #[test]
    fn every_offered_format_has_a_layout_and_bgr_comes_first() {
        for f in PREFERRED {
            assert!(layout(f).is_some(), "{f:?}");
        }
        assert!(layout(PREFERRED[0]).unwrap().is_bgra());
        assert!(layout(PREFERRED[1]).unwrap().is_bgra());
        assert_eq!(layout(VideoFormat::NV12), None);
        assert_eq!(layout(VideoFormat::RGB), None);
    }

    #[test]
    fn padded_rows_are_packed() {
        // 2×2, stride 12 (one padding pixel per row), offset 4 into the mapping.
        let mut src = vec![0xEEu8; 4 + 12 * 2];
        src[4..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[16..24].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        let mut dst = Vec::new();
        let plane = Plane {
            offset: 4,
            stride: 12,
        };
        copy_frame(&src, plane, (2, 2), bgra(), &mut dst).unwrap();
        assert_eq!(dst, (1..=16).collect::<Vec<u8>>());
    }

    #[test]
    fn zero_stride_means_packed_and_offset_wraps_modulo_maxsize() {
        let src: Vec<u8> = (0..16).collect();
        let mut dst = vec![0xAA; 3];
        let plane = Plane {
            offset: 16,
            stride: 0,
        };
        copy_frame(&src, plane, (2, 2), bgra(), &mut dst).unwrap();
        assert_eq!(dst, src);
    }

    #[test]
    fn rgb_orders_are_swizzled_to_bgra() {
        let px = [10u8, 20, 30, 40];
        let cases = [
            (VideoFormat::RGBA, [30, 20, 10, 40]),
            (VideoFormat::RGBx, [30, 20, 10, 0xff]),
            (VideoFormat::ARGB, [40, 30, 20, 10]),
            (VideoFormat::xRGB, [40, 30, 20, 0xff]),
            (VideoFormat::ABGR, [20, 30, 40, 10]),
            (VideoFormat::xBGR, [20, 30, 40, 0xff]),
            (VideoFormat::BGRA, [10, 20, 30, 40]),
        ];
        for (format, want) in cases {
            let mut dst = Vec::new();
            let plane = Plane {
                offset: 0,
                stride: 4,
            };
            copy_frame(&px, plane, (1, 1), layout(format).unwrap(), &mut dst).unwrap();
            assert_eq!(dst, want, "{format:?}");
        }
    }

    #[test]
    fn malformed_chunks_are_refused_not_read_past() {
        let src = vec![0u8; 100];
        let mut dst = Vec::new();
        let mut copy = |offset, stride, size| {
            copy_frame(&src, Plane { offset, stride }, size, bgra(), &mut dst)
        };
        assert_eq!(copy(0, -40, (10, 2)), Err(CopyError::NegativeStride(-40)));
        assert_eq!(
            copy(0, 8, (10, 2)),
            Err(CopyError::StrideTooSmall { stride: 8, row: 40 })
        );
        // Two 40-byte rows at stride 48 need 88 bytes; from offset 20 only 80 remain.
        assert_eq!(
            copy(20, 48, (10, 2)),
            Err(CopyError::OutOfBounds {
                needed: 88,
                available: 80
            })
        );
        assert!(copy(12, 48, (10, 2)).is_ok());
        assert!(copy(0, 0, (0, 0)).is_ok());
    }

    #[test]
    fn premultiplied_cursor_becomes_straight_bgra() {
        // RGBA premultiplied: opaque red, half-transparent white, fully transparent.
        let data = [255, 0, 0, 255, 128, 128, 128, 128, 0, 0, 0, 0];
        let shape =
            cursor_shape(VideoFormat::RGBA, (3, 1), 12, &data, Point { x: 1, y: 0 }).unwrap();
        assert_eq!(
            shape.bgra,
            vec![0, 0, 255, 255, 255, 255, 255, 128, 0, 0, 0, 0]
        );
        assert_eq!(shape.hotspot, Point { x: 1, y: 0 });
        assert!(is_visible(&shape));
    }

    #[test]
    fn a_fully_transparent_sprite_is_invisible() {
        let shape = cursor_shape(VideoFormat::RGBA, (1, 1), 4, &[0; 4], Point::default()).unwrap();
        assert!(!is_visible(&shape));
    }

    #[test]
    fn bad_cursor_bitmaps_are_ignored_and_hotspots_kept_inside() {
        let data = vec![0xffu8; 64];
        let hot = Point::default();
        assert!(cursor_shape(VideoFormat::NV12, (2, 2), 8, &data, hot).is_none());
        assert!(cursor_shape(VideoFormat::RGBA, (0, 2), 8, &data, hot).is_none());
        assert!(cursor_shape(VideoFormat::RGBA, (4, 5), 16, &data, hot).is_none());
        assert!(cursor_shape(VideoFormat::RGBA, (2000, 1), 0, &data, hot).is_none());
        let far = Point { x: 99, y: -3 };
        let s = cursor_shape(VideoFormat::BGRx, (2, 2), 8, &data, far).unwrap();
        assert_eq!(s.hotspot, Point { x: 1, y: 0 });
        assert_eq!(s.bgra, vec![0xff; 16]);
    }
}
