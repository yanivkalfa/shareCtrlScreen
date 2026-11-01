//! Letterbox placement and pointer normalization — the two halves of one
//! mapping that must agree exactly, or clicks land offset by the black bars
//! (the "clicks 1 cm off" bug the Windows surface fixed in `render/src/window.rs`).
//!
//! All sizes are physical pixels of the **full** surface (what
//! [`platform_api::VideoSurface::client_size`] reports), never the area left
//! visible while the toolbar is revealed: the reveal crops the picture, it
//! does not move or rescale it.

/// An axis-aligned rectangle in pixels; may be fractional and may extend past
/// the target (the GPU clips).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The largest `video`-aspect rectangle that fits inside `target`, centered,
/// with black bars on the remaining sides. The whole video texture is mapped
/// into it, so the picture is scaled exactly once and never distorted.
///
/// A zero-sized target or video yields an empty rectangle (nothing to draw).
pub fn letterbox(target: (u32, u32), video: (u32, u32)) -> RectF {
    let (tw, th) = (f64::from(target.0), f64::from(target.1));
    let (vw, vh) = (f64::from(video.0), f64::from(video.1));
    if tw <= 0.0 || th <= 0.0 || vw <= 0.0 || vh <= 0.0 {
        return RectF {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
    }
    let scale = (tw / vw).min(th / vh);
    let (w, h) = (vw * scale, vh * scale);
    RectF {
        x: ((tw - w) * 0.5).max(0.0),
        y: ((th - h) * 0.5).max(0.0),
        w,
        h,
    }
}

/// Map a pointer position on the surface (physical pixels, top-left origin)
/// to `[0,1]` over the letterboxed video, clamping points on the black bars to
/// the nearest video edge (the same behaviour as the Windows surface).
///
/// Before the first frame is presented the video size is unknown (`None` or a
/// zero dimension); the whole surface is then used, which is what the host
/// sees anyway until the stream starts.
pub fn normalize(px: f64, py: f64, surface: (u32, u32), video: Option<(u32, u32)>) -> (f64, f64) {
    let full = RectF {
        x: 0.0,
        y: 0.0,
        w: f64::from(surface.0.max(1)),
        h: f64::from(surface.1.max(1)),
    };
    let rect = match video {
        Some((vw, vh)) if vw > 0 && vh > 0 => {
            let r = letterbox((surface.0.max(1), surface.1.max(1)), (vw, vh));
            if r.w > 0.0 && r.h > 0.0 {
                r
            } else {
                full
            }
        }
        _ => full,
    };
    (
        ((px - rect.x) / rect.w).clamp(0.0, 1.0),
        ((py - rect.y) / rect.h).clamp(0.0, 1.0),
    )
}

/// Size of the client-side cursor sprite in points (logical pixels); scaled by
/// the backing factor so it looks the same on Retina and non-Retina screens.
/// Matches the Windows sprite's 18 × 26 px at 100 % scaling.
pub const CURSOR_SIZE_PT: (f64, f64) = (18.0, 26.0);

/// Where the cursor sprite goes: its top-left corner is the hotspot (the
/// pointer tip), placed at the normalized position over the video rectangle.
pub fn cursor_rect(video_rect: RectF, cursor: (f64, f64), scale: f64) -> RectF {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    RectF {
        x: video_rect.x + cursor.0.clamp(0.0, 1.0) * video_rect.w,
        y: video_rect.y + cursor.1.clamp(0.0, 1.0) * video_rect.h,
        w: CURSOR_SIZE_PT.0 * scale,
        h: CURSOR_SIZE_PT.1 * scale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_wide_target_gets_pillarbox_bars() {
        // A 2:1 target is wider than 16:9, so the bars go left and right.
        let r = letterbox((2000, 1000), (1920, 1080));
        assert!((r.h - 1000.0).abs() < 1e-9);
        assert!((r.w - 1000.0 * 16.0 / 9.0).abs() < 1e-9);
        assert!((r.x - (2000.0 - r.w) / 2.0).abs() < 1e-9);
        assert_eq!(r.y, 0.0);
    }

    #[test]
    fn letterbox_tall_target_gets_top_and_bottom_bars() {
        let r = letterbox((1000, 1000), (1920, 1080));
        assert!((r.w - 1000.0).abs() < 1e-9);
        assert!((r.h - 562.5).abs() < 1e-9);
        assert_eq!(r.x, 0.0);
        assert!((r.y - 218.75).abs() < 1e-9);
    }

    #[test]
    fn letterbox_exact_fit_is_identity() {
        let r = letterbox((1920, 1080), (1920, 1080));
        assert_eq!(
            r,
            RectF {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0
            }
        );
    }

    #[test]
    fn letterbox_degenerate_sizes_draw_nothing() {
        assert_eq!(letterbox((0, 100), (10, 10)).w, 0.0);
        assert_eq!(letterbox((100, 100), (0, 10)).h, 0.0);
    }

    #[test]
    fn normalize_maps_over_the_letterbox_not_the_surface() {
        // 1000×1000 surface, 16:9 video → video occupies y ∈ [218.75, 781.25].
        let video = Some((1920, 1080));
        let (nx, ny) = normalize(500.0, 500.0, (1000, 1000), video);
        assert!((nx - 0.5).abs() < 1e-9 && (ny - 0.5).abs() < 1e-9);
        let (_, top) = normalize(10.0, 218.75, (1000, 1000), video);
        assert!(top.abs() < 1e-9);
        let (_, bottom) = normalize(10.0, 781.25, (1000, 1000), video);
        assert!((bottom - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normalize_clamps_clicks_on_the_bars() {
        let video = Some((1920, 1080));
        assert_eq!(normalize(-5.0, 0.0, (1000, 1000), video), (0.0, 0.0));
        assert_eq!(normalize(2000.0, 999.0, (1000, 1000), video), (1.0, 1.0));
    }

    #[test]
    fn normalize_without_a_frame_uses_the_whole_surface() {
        assert_eq!(normalize(250.0, 750.0, (1000, 1000), None), (0.25, 0.75));
        assert_eq!(
            normalize(250.0, 750.0, (1000, 1000), Some((0, 0))),
            (0.25, 0.75)
        );
    }

    #[test]
    fn normalize_is_the_inverse_of_letterbox_placement() {
        for &(surface, video) in &[
            ((1280u32, 720u32), (1920u32, 1080u32)),
            ((800, 1200), (2560, 1440)),
            ((3024, 1890), (1366, 768)),
        ] {
            let r = letterbox(surface, video);
            for &(fx, fy) in &[(0.0, 0.0), (0.25, 0.75), (1.0, 1.0), (0.5, 0.1)] {
                let (nx, ny) = normalize(r.x + fx * r.w, r.y + fy * r.h, surface, Some(video));
                assert!((nx - fx).abs() < 1e-9, "{surface:?} {video:?} x");
                assert!((ny - fy).abs() < 1e-9, "{surface:?} {video:?} y");
            }
        }
    }

    #[test]
    fn cursor_sprite_scales_with_backing_factor_and_sits_on_the_hotspot() {
        let video = RectF {
            x: 100.0,
            y: 50.0,
            w: 400.0,
            h: 200.0,
        };
        let c = cursor_rect(video, (0.5, 0.5), 2.0);
        assert_eq!((c.x, c.y), (300.0, 150.0));
        assert_eq!((c.w, c.h), (36.0, 52.0));
        let bad_scale = cursor_rect(video, (0.0, 0.0), f64::NAN);
        assert_eq!((bad_scale.w, bad_scale.h), CURSOR_SIZE_PT);
    }
}
