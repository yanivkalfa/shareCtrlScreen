//! Pure conversions between ScreenCaptureKit / AppKit / CoreGraphics values and
//! the `platform-api` types. Kept free of OS calls so every rule here is
//! unit-tested on every CI job, not only on a Mac.

use platform_api::{Point, Rect};

use crate::slot::StopReason;

/// A CoreGraphics rectangle (`CGRect`) without the OS type, in whatever unit
/// the caller says (points or pixels).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// `SCStreamErrorUserDeclined`: the user refused (or revoked) Screen Recording.
pub(crate) const SC_ERROR_USER_DECLINED: isize = -3801;

/// The size ScreenCaptureKit should deliver: the display's native pixel size,
/// or — when the viewer asked for less — the largest same-aspect size that fits
/// `requested`. Never upscales (extra pixels cost bitrate and add no detail),
/// always even (H.264/HEVC chroma is subsampled 2×2 and hardware encoders
/// reject odd sizes) and at least 2×2.
///
/// The source aspect is preserved because the viewer letterboxes to the
/// frame's ratio; any other shape would be shown with bars *and* stretched.
/// Same rule as the Windows host's `fit_within`, so both OSes encode the same
/// size for the same viewer window.
pub(crate) fn capture_size(native: (u32, u32), requested: Option<(u32, u32)>) -> (u32, u32) {
    let even = |v: u32| (v & !1).max(2);
    let native_even = (even(native.0), even(native.1));
    let Some(want) = requested else {
        return native_even;
    };
    if want.0 == 0 || want.1 == 0 || native.0 == 0 || native.1 == 0 {
        return native_even;
    }
    let s = (want.0 as f64 / native.0 as f64)
        .min(want.1 as f64 / native.1 as f64)
        .min(1.0);
    (
        even((native.0 as f64 * s).round() as u32),
        even((native.1 as f64 * s).round() as u32),
    )
}

/// Convert ScreenCaptureKit dirty rects (pixels of the delivered surface, per
/// the `SCStreamFrameInfoDirtyRects` header) to integer rects inside a
/// `frame`-sized picture.
///
/// Rounded *outward* (floor the origin, ceil the far edge): a damaged pixel
/// left out of the region would never be re-sent, a pixel too many costs
/// nothing. Clamped to the frame because a rect from the previous
/// configuration can outlive a resize by a frame; empty results are dropped.
pub(crate) fn dirty_rects(rects: &[RectF], frame: (u32, u32)) -> Vec<Rect> {
    let (fw, fh) = (frame.0 as f64, frame.1 as f64);
    rects
        .iter()
        .filter(|r| r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite())
        .filter_map(|r| {
            let x0 = r.x.floor().clamp(0.0, fw);
            let y0 = r.y.floor().clamp(0.0, fh);
            let x1 = (r.x + r.w).ceil().clamp(0.0, fw);
            let y1 = (r.y + r.h).ceil().clamp(0.0, fh);
            (x1 > x0 && y1 > y0).then_some(Rect {
                x: x0 as i32,
                y: y0 as i32,
                w: (x1 - x0) as u32,
                h: (y1 - y0) as u32,
            })
        })
        .collect()
}

/// Map the AppKit mouse location to captured-frame pixels.
///
/// * `mouse` — `NSEvent.mouseLocation`: global points, origin at the
///   **bottom**-left of the main display, y up.
/// * `main_height` — height in points of the main display (the one whose
///   bottom-left is the AppKit origin), used to flip into CoreGraphics space.
/// * `display` — `CGDisplayBounds` of the captured display: global points,
///   origin at the **top**-left of the main display, y down.
/// * `frame` — the delivered picture size in pixels (the display may be
///   captured smaller than native when the viewer asked for less).
///
/// Returns the pixel position and whether the pointer is on this display.
/// Positions off the display are still reported (clamped by the host), so the
/// viewer's sprite tracks to the edge instead of freezing mid-screen.
pub(crate) fn cursor_in_frame(
    mouse: (f64, f64),
    main_height: f64,
    display: RectF,
    frame: (u32, u32),
) -> (Point, bool) {
    let gx = mouse.0;
    let gy = main_height - mouse.1;
    let (lx, ly) = (gx - display.x, gy - display.y);
    let inside = lx >= 0.0 && ly >= 0.0 && lx < display.w && ly < display.h;
    if display.w <= 0.0 || display.h <= 0.0 {
        return (Point::default(), false);
    }
    let sx = frame.0 as f64 / display.w;
    let sy = frame.1 as f64 / display.h;
    (
        Point {
            x: (lx * sx).floor() as i32,
            y: (ly * sy).floor() as i32,
        },
        inside,
    )
}

/// Pixel size to render a cursor image of `points` at `scale` frame pixels per
/// point. At least 1×1; at most 256×256 (macOS's own accessibility cursor
/// size limit is far below this, so only a corrupt size hits the cap).
pub(crate) fn cursor_pixels(points: (f64, f64), scale: f64) -> (u32, u32) {
    let px = |v: f64| {
        let p = (v * scale).round();
        if p.is_finite() {
            p.clamp(1.0, 256.0) as u32
        } else {
            1
        }
    };
    (px(points.0), px(points.1))
}

/// An `NSCursor.hotSpot` (points, top-left origin of the image) in pixels of
/// the rendered `size`, kept inside the image.
pub(crate) fn hotspot_pixels(hot: (f64, f64), scale: f64, size: (u32, u32)) -> Point {
    let px = |v: f64, max: u32| {
        let p = (v * scale).floor();
        if p.is_finite() {
            p.clamp(0.0, max.saturating_sub(1) as f64) as i32
        } else {
            0
        }
    };
    Point {
        x: px(hot.0, size.0),
        y: px(hot.1, size.1),
    }
}

/// In place: premultiplied BGRA (what a CoreGraphics bitmap context with
/// `PremultipliedFirst | ByteOrder32Little` holds) → straight-alpha BGRA (what
/// `CursorShape` carries). Without this, every anti-aliased cursor edge would
/// be drawn too dark by a viewer that blends with straight alpha.
pub(crate) fn unpremultiply_bgra(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        match a {
            0 => p[..3].fill(0),
            255 => {}
            _ => {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
    }
}

/// Classify a stopped / failed stream. `UserDeclined` is the explicit refusal
/// code; a stream that stops for any other reason while the permission
/// preflight also reports "not granted" is a revocation too. Everything else
/// (display sleep, mode change, `SystemStoppedStream`) is recoverable by
/// recreating the stream.
pub(crate) fn classify_stop(code: isize, sck_domain: bool, preflight_granted: bool) -> StopReason {
    if (sck_domain && code == SC_ERROR_USER_DECLINED) || !preflight_granted {
        StopReason::PermissionDenied
    } else {
        StopReason::Lost
    }
}

/// Wait before the `failures`-th consecutive stream rebuild: 250 ms doubling to
/// 4 s. Fast enough that a woken display is back within a few seconds, slow
/// enough that a display asleep all night costs ~1 attempt per 4 s.
pub(crate) fn retry_delay(failures: u32) -> std::time::Duration {
    const FIRST_MS: u64 = 250;
    const MAX_MS: u64 = 4_000;
    let doublings = failures.saturating_sub(1).min(16);
    std::time::Duration::from_millis((FIRST_MS << doublings).min(MAX_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backs_off_to_a_cap() {
        let ms = |n| retry_delay(n).as_millis();
        assert_eq!(ms(0), 250);
        assert_eq!(ms(1), 250);
        assert_eq!(ms(2), 500);
        assert_eq!(ms(4), 2000);
        assert_eq!(ms(5), 4000);
        assert_eq!(ms(u32::MAX), 4000);
    }

    #[test]
    fn capture_size_is_native_unless_the_viewer_wants_less() {
        assert_eq!(capture_size((3840, 2160), None), (3840, 2160));
        assert_eq!(capture_size((3840, 2160), Some((1920, 1200))), (1920, 1080));
        // Never upscale.
        assert_eq!(capture_size((1440, 900), Some((3000, 3000))), (1440, 900));
        // Zero means "no request".
        assert_eq!(capture_size((2880, 1800), Some((0, 500))), (2880, 1800));
        // Odd native sizes are evened (some scaled Retina modes are odd).
        assert_eq!(capture_size((1367, 769), None), (1366, 768));
        // Tiny requests never reach 0.
        assert_eq!(capture_size((2880, 1800), Some((1, 1))), (2, 2));
    }

    #[test]
    fn dirty_rects_round_outward_and_clamp() {
        let r = dirty_rects(
            &[
                RectF {
                    x: 10.4,
                    y: 20.6,
                    w: 5.2,
                    h: 1.0,
                },
                // Hangs off the bottom-right corner.
                RectF {
                    x: 1900.0,
                    y: 1070.0,
                    w: 100.0,
                    h: 100.0,
                },
                // Entirely outside / empty / garbage.
                RectF {
                    x: 5000.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                },
                RectF {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 10.0,
                },
                RectF {
                    x: f64::NAN,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                },
                // Negative origin.
                RectF {
                    x: -3.0,
                    y: -3.0,
                    w: 6.0,
                    h: 6.0,
                },
            ],
            (1920, 1080),
        );
        assert_eq!(
            r,
            vec![
                Rect {
                    x: 10,
                    y: 20,
                    w: 6,
                    h: 2
                },
                Rect {
                    x: 1900,
                    y: 1070,
                    w: 20,
                    h: 10
                },
                Rect {
                    x: 0,
                    y: 0,
                    w: 3,
                    h: 3
                },
            ]
        );
    }

    #[test]
    fn cursor_maps_from_appkit_space_to_frame_pixels() {
        let main = RectF {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        // Retina main display captured at native 2x.
        let (p, inside) = cursor_in_frame((100.0, 800.0), 900.0, main, (2880, 1800));
        assert_eq!(p, Point { x: 200, y: 200 });
        assert!(inside);
        // Same display captured at half size for a small viewer window.
        let (p, _) = cursor_in_frame((100.0, 800.0), 900.0, main, (1440, 900));
        assert_eq!(p, Point { x: 100, y: 100 });

        // A second display to the right, top-aligned, 1920x1080 points at 1x.
        let right = RectF {
            x: 1440.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        // AppKit y = 900 - 50 → CG y = 50.
        let (p, inside) = cursor_in_frame((1500.0, 850.0), 900.0, right, (1920, 1080));
        assert_eq!(p, Point { x: 60, y: 50 });
        assert!(inside);
        // Pointer on the main display, seen from the right display.
        let (p, inside) = cursor_in_frame((100.0, 800.0), 900.0, right, (1920, 1080));
        assert!(!inside);
        assert!(p.x < 0);
        // A display placed above the main one has negative CG y.
        let above = RectF {
            x: 0.0,
            y: -1080.0,
            w: 1920.0,
            h: 1080.0,
        };
        let (p, inside) = cursor_in_frame((10.0, 1000.0), 900.0, above, (1920, 1080));
        assert_eq!(p, Point { x: 10, y: 980 });
        assert!(inside);
        // Degenerate bounds never divide by zero.
        assert_eq!(
            cursor_in_frame((1.0, 1.0), 900.0, RectF::default(), (10, 10)),
            (Point::default(), false)
        );
    }

    #[test]
    fn cursor_sizes_and_hotspots_scale_and_stay_in_bounds() {
        assert_eq!(cursor_pixels((17.0, 23.0), 2.0), (34, 46));
        assert_eq!(cursor_pixels((0.0, 0.2), 1.0), (1, 1));
        assert_eq!(cursor_pixels((1e9, f64::NAN), 1.0), (256, 1));
        assert_eq!(
            hotspot_pixels((4.5, 4.5), 2.0, (34, 46)),
            Point { x: 9, y: 9 }
        );
        assert_eq!(
            hotspot_pixels((100.0, -1.0), 2.0, (34, 46)),
            Point { x: 33, y: 0 }
        );
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        let mut px = vec![
            0, 0, 0, 255, // opaque black stays
            200, 100, 50, 255, // opaque colour stays
            64, 32, 0, 128, // half-transparent: colour doubled back
            9, 9, 9, 0, // fully transparent: colour cleared
            200, 200, 200, 100, // over-bright (corrupt) premultiplied: saturates
        ];
        unpremultiply_bgra(&mut px);
        assert_eq!(
            px,
            vec![0, 0, 0, 255, 200, 100, 50, 255, 128, 64, 0, 128, 0, 0, 0, 0, 255, 255, 255, 100]
        );
    }

    #[test]
    fn stops_are_classified() {
        assert_eq!(
            classify_stop(SC_ERROR_USER_DECLINED, true, true),
            StopReason::PermissionDenied
        );
        // Same code from another domain is not a refusal.
        assert_eq!(
            classify_stop(SC_ERROR_USER_DECLINED, false, true),
            StopReason::Lost
        );
        // SystemStoppedStream (display sleep) while still granted → recover.
        assert_eq!(classify_stop(-3821, true, true), StopReason::Lost);
        // Any stop while the grant is gone → permission.
        assert_eq!(
            classify_stop(-3821, true, false),
            StopReason::PermissionDenied
        );
    }
}
