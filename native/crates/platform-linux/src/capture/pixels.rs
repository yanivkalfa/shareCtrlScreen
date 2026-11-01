//! Pure pixel conversions for the X11 capture: X images (B,G,R,x in memory) →
//! the trait's straight-alpha BGRA, and XFixes cursor images → `CursorShape`
//! pixels. Unit-tested without a server.

use platform_api::Rect;

/// Copy a `r.w × r.h` X image (rows `src_stride` bytes apart, 4 bytes per
/// pixel) into `dst` (rows `dst_stride` bytes apart) at `(r.x, r.y)`, setting
/// every alpha byte to 255.
///
/// Why force alpha: a depth-24 visual has no alpha channel, and the X server
/// leaves the fourth byte of each pixel undefined (zero on Xorg). The trait's
/// `Bgra8` carries real alpha, and a consumer that honours it (a presenter
/// blending the frame, a PNG dump in a test) would show a transparent screen.
///
/// Returns `false`, copying nothing, when either buffer is too small for the
/// rectangle; the caller treats that as a malformed reply.
pub(crate) fn copy_opaque(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    r: Rect,
) -> bool {
    if r.x < 0 || r.y < 0 {
        return false;
    }
    if r.w == 0 || r.h == 0 {
        return true;
    }
    let (x, y, w, h) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
    let row = w * 4;
    let src_need = src_stride * (h - 1) + row;
    let dst_need = dst_stride * (y + h - 1) + x * 4 + row;
    if src_stride < row || dst_stride < (x + w) * 4 || src.len() < src_need || dst.len() < dst_need
    {
        return false;
    }
    for i in 0..h {
        let s = &src[i * src_stride..i * src_stride + row];
        let d0 = (y + i) * dst_stride + x * 4;
        let d = &mut dst[d0..d0 + row];
        d.copy_from_slice(s);
        for px in d.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    true
}

/// An XFixes cursor image → straight-alpha BGRA bytes.
///
/// XFixes delivers one CARD32 per pixel, alpha in the top byte then red, green
/// and blue, **premultiplied** by alpha (fixesproto, `GetCursorImage`).
/// `CursorShape` is straight alpha; without un-premultiplying, every
/// anti-aliased edge would be drawn too dark by a viewer blending with
/// straight alpha. `None` when the pixel count does not match the size.
pub(crate) fn cursor_bgra(argb: &[u32], width: u16, height: u16) -> Option<Vec<u8>> {
    if argb.len() != width as usize * height as usize {
        return None;
    }
    let mut out = Vec::with_capacity(argb.len() * 4);
    for &p in argb {
        let a = (p >> 24) as u8;
        let unmul = |c: u32| -> u8 {
            let c = c & 0xff;
            match a {
                0 => 0,
                255 => c as u8,
                _ => ((c * 255 + a as u32 / 2) / a as u32).min(255) as u8,
            }
        };
        out.extend_from_slice(&[unmul(p), unmul(p >> 8), unmul(p >> 16), a]);
    }
    Some(out)
}

/// Whether a BGRA cursor image is fully transparent. Applications hide the
/// pointer (video players, games, typing in some editors) by setting such a
/// cursor; X11 has no "hidden" flag to query, so this is how it is detected.
pub(crate) fn is_blank(bgra: &[u8]) -> bool {
    bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_places_rows_at_the_offset_and_forces_alpha() {
        // A 2x2 source with an 8-byte row padding: B,G,R,x pixels.
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, 99, 99, 99, 99, //
            7, 8, 9, 0, 10, 11, 12, 0,
        ];
        let mut dst = vec![0u8; 4 * 3 * 4]; // 4x3 picture, stride 16
        assert!(copy_opaque(
            &src,
            16,
            &mut dst,
            16,
            Rect {
                x: 1,
                y: 1,
                w: 2,
                h: 2
            }
        ));
        assert_eq!(
            &dst[16..32],
            &[0, 0, 0, 0, 1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0]
        );
        assert_eq!(
            &dst[32..48],
            &[0, 0, 0, 0, 7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0]
        );
        // Row 0 untouched; padding bytes of the source never copied.
        assert!(dst[..16].iter().all(|&b| b == 0));
    }

    #[test]
    fn copy_refuses_short_buffers_and_out_of_range_rects() {
        let src = [0u8; 16];
        let mut dst = vec![0u8; 16];
        // Source too short for two rows of two pixels.
        assert!(!copy_opaque(
            &src[..12],
            8,
            &mut dst,
            8,
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2
            }
        ));
        // Rectangle sticking out of the destination.
        assert!(!copy_opaque(
            &src,
            8,
            &mut dst,
            8,
            Rect {
                x: 1,
                y: 0,
                w: 2,
                h: 2
            }
        ));
        // Source stride smaller than a row.
        assert!(!copy_opaque(
            &src,
            4,
            &mut dst,
            8,
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2
            }
        ));
        assert!(dst.iter().all(|&b| b == 0));
        // Empty rectangles are a no-op, not an error.
        assert!(copy_opaque(
            &src,
            8,
            &mut dst,
            8,
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 2
            }
        ));
    }

    #[test]
    fn cursor_pixels_are_unpremultiplied_bgra() {
        let px = [
            0xff_10_20_30, // opaque: unchanged, reordered to B,G,R,A
            0x80_40_20_00, // half alpha: channels doubled back
            0x00_00_00_00, // transparent
        ];
        let out = cursor_bgra(&px, 3, 1).expect("size matches");
        assert_eq!(&out[0..4], &[0x30, 0x20, 0x10, 0xff]);
        assert_eq!(&out[4..8], &[0x00, 0x40, 0x80, 0x80]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0]);
        assert!(!is_blank(&out));
        assert_eq!(cursor_bgra(&px, 2, 2), None, "4 pixels claimed, 3 sent");
    }

    #[test]
    fn blank_cursor_is_detected() {
        let out = cursor_bgra(&[0; 4], 2, 2).expect("size matches");
        assert!(is_blank(&out));
    }
}
