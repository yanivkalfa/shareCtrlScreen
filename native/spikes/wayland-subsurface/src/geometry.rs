//! Pure geometry of the video subsurface: logical → physical size under a
//! fractional scale, the hover-reveal crop, letterbox input normalization and
//! wheel units. No Wayland or GTK calls here, so every rule is unit-tested.
//!
//! ## References
//! * wp_fractional_scale_v1 — scale is `n / 120`; buffers are
//!   `round(logical × scale)`, rounded halfway away from zero:
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/fractional-scale/fractional-scale-v1.xml>
//! * wp_viewport — source rectangle in buffer coordinates (with buffer_scale 1),
//!   `out_of_buffer` if it leaves the buffer; destination in surface-local units:
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/stable/viewporter/viewporter.xml>
//! * wl_pointer.axis / axis_value120 — positive vertical = down, one detent =
//!   120 (`wayland.xml`, wl_pointer v8):
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * The Windows rules being reproduced: `native/crates/render/src/window.rs`
//!   (`normalize_client`, hover reveal) and the `VideoSurface` invariants in
//!   `native/crates/platform-api/src/surface.rs`.

/// Session bar height in CSS px (the web `.view-bar`). WebKitGTK CSS px are GTK
/// logical px (no page zoom in the app), which are Wayland surface-local units.
pub const BAR_CSS_PX: u32 = 40;
/// The pointer within this many **physical** px of the top edge reveals the bar
/// (the `VideoSurface` contract's "top 2 px").
pub const REVEAL_EDGE_PHYS_PX: f64 = 2.0;
/// Going deeper than this re-hides the bar. Interpreted in CSS px so the
/// hysteresis band (56 − 40 = 16 px) survives high scales; see the spike report
/// for the Windows discrepancy (it compares physical px there).
pub const UNREVEAL_DEPTH_CSS_PX: f64 = 56.0;
/// wp_fractional_scale_v1 denominator.
pub const SCALE_DENOMINATOR: u32 = 120;

/// Everything that decides how the video surfaces are laid out. One instance
/// lives behind the surface lock (see `surface::Shared`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Top-left of the content area in the GTK toplevel's surface (logical px).
    pub origin: (i32, i32),
    /// Content-area size in logical px (what GTK allocated to the web content).
    pub logical: (u32, u32),
    /// Preferred scale × 120 (fractional-scale event, else GDK's integer scale).
    pub scale120: u32,
    /// The top bar strip is cut out of the video (hover reveal active).
    pub revealed: bool,
    /// Size of the buffer the presenter last committed, if any. The viewport
    /// source rectangle is computed from THIS, never from the target size: the
    /// compositor validates the source against the attached buffer and a miss
    /// is a fatal protocol error (and GDK `_exit(1)`s on display errors).
    pub attached: Option<(u32, u32)>,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            origin: (0, 0),
            logical: (1, 1),
            scale120: SCALE_DENOMINATOR,
            revealed: false,
            attached: None,
        }
    }
}

/// The video surface's viewport and position inside its parent area surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoLayout {
    /// `wp_viewport.set_source(x, y, w, h)` in buffer px; `None` = unset.
    pub source: Option<(f64, f64, f64, f64)>,
    /// `wp_viewport.set_destination(w, h)` in logical px (always ≥ 1).
    pub destination: (i32, i32),
    /// `wl_subsurface.set_position(0, y)` of the video inside the area.
    pub offset_y: i32,
}

/// `round(len × scale120 / 120)`, halfway away from zero (all inputs ≥ 0).
pub fn scale_len(len: u32, scale120: u32) -> u32 {
    let scaled =
        (len as u64 * scale120 as u64 + (SCALE_DENOMINATOR as u64 / 2)) / SCALE_DENOMINATOR as u64;
    scaled.min(u32::MAX as u64) as u32
}

impl Geometry {
    pub fn scale(&self) -> f64 {
        self.scale120 as f64 / SCALE_DENOMINATOR as f64
    }

    /// The buffer size the presenter renders at (what `client_size` reports).
    pub fn physical(&self) -> (u32, u32) {
        (
            scale_len(self.logical.0, self.scale120).max(1),
            scale_len(self.logical.1, self.scale120).max(1),
        )
    }

    /// Logical px currently cut from the top. Never the whole height: a zero
    /// destination height is a `bad_value` protocol error.
    pub fn bar_logical(&self) -> u32 {
        if self.revealed {
            BAR_CSS_PX.min(self.logical.1.saturating_sub(1))
        } else {
            0
        }
    }

    /// Viewport + position that show the full-size picture minus the top strip,
    /// with every remaining pixel exactly where it is when unrevealed: the crop
    /// moves the surface down by the strip and drops the same rows from the
    /// buffer, so the picture itself never moves (the Windows `SetWindowRgn`
    /// behaviour, which keeps `client_size` constant too).
    pub fn video_layout(&self) -> VideoLayout {
        let bar = self.bar_logical();
        let (lw, lh) = (self.logical.0.max(1), self.logical.1.max(1));
        let destination = (lw as i32, (lh - bar).max(1) as i32);
        let source = match (bar, self.attached) {
            (0, _) | (_, None) => None,
            (bar, Some((bw, bh))) => {
                // Work in wl_fixed units (1/256 px): y is rounded DOWN and the
                // height is whatever remains, so y + height == bh exactly and the
                // rectangle can never leave the buffer (out_of_buffer). Multiples
                // of 1/256 are exact in f64, and wl_fixed conversion multiplies by
                // 256, so these values reach the wire unchanged.
                let y256 = (bar as u64 * bh as u64 * 256) / lh as u64;
                let h256 = (bh as u64 * 256).saturating_sub(y256);
                Some((0.0, y256 as f64 / 256.0, bw as f64, h256 as f64 / 256.0))
            }
        };
        VideoLayout {
            source,
            destination,
            offset_y: bar as i32,
        }
    }

    /// Hover-reveal transition for a pointer at `y_full` logical px from the top
    /// of the FULL content rect. `Some(new_state)` when it changes.
    pub fn reveal_transition(&self, y_full: f64) -> Option<bool> {
        if !self.revealed && y_full * self.scale() <= REVEAL_EDGE_PHYS_PX {
            Some(true)
        } else if self.revealed && y_full > UNREVEAL_DEPTH_CSS_PX {
            Some(false)
        } else {
            None
        }
    }
}

/// Map a position in the full content rect (logical px) to `[0,1]` over the
/// letterboxed video rectangle, the presenter's exact letterbox math (scale to
/// fit, centre). `video` = the last presented frame size; unknown (0) → the
/// whole rect. The math is scale-invariant, so logical px give the same answer
/// as physical px.
pub fn normalize(rect: (u32, u32), video: (u32, u32), x: f64, y: f64) -> (f64, f64) {
    let (cw, ch) = (rect.0.max(1) as f64, rect.1.max(1) as f64);
    let (vw, vh) = (video.0 as f64, video.1 as f64);
    if vw <= 0.0 || vh <= 0.0 {
        return ((x / cw).clamp(0.0, 1.0), (y / ch).clamp(0.0, 1.0));
    }
    let (ox, oy, dw, dh) = letterbox(cw, ch, vw, vh);
    (
        ((x - ox) / dw).clamp(0.0, 1.0),
        ((y - oy) / dh).clamp(0.0, 1.0),
    )
}

/// `(x, y, w, h)` of a `vw×vh` picture fitted and centred in `cw×ch`.
pub fn letterbox(cw: f64, ch: f64, vw: f64, vh: f64) -> (f64, f64, f64, f64) {
    let scale = (cw / vw).min(ch / vh);
    let (dw, dh) = (vw * scale, vh * scale);
    ((cw - dw) * 0.5, (ch - dh) * 0.5, dw, dh)
}

/// Linux evdev button → DOM `MouseEvent.button`. Other buttons are dropped,
/// as on Windows (which only forwards left/middle/right).
pub fn dom_button(evdev: u32) -> Option<u8> {
    // linux/input-event-codes.h
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    match evdev {
        BTN_LEFT => Some(0),
        BTN_MIDDLE => Some(1),
        BTN_RIGHT => Some(2),
        _ => None,
    }
}

/// Wheel motion collected over one `wl_pointer.frame`, converted to the
/// protocol's Windows units (±120 per notch, positive `dy` = away from user).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct WheelAccumulator {
    /// High-resolution detents (axis_value120, or axis_discrete × 120).
    v120: (i32, i32),
    /// Continuous scroll distance (touchpads), in surface-local px.
    continuous: (f64, f64),
    /// Whether any detent arrived this frame (then the continuous value is the
    /// same motion restated and must not be counted twice).
    has_detents: (bool, bool),
    /// Sub-notch remainder of continuous scrolling, carried across frames.
    remainder: (f64, f64),
}

/// Continuous scroll distance that counts as one notch. Weston and wlroots
/// report a wheel detent as 15 px (libinput's default) and older Weston as 10;
/// 15 makes a touchpad feel like a wheel on the host.
const CONTINUOUS_PX_PER_NOTCH: f64 = 15.0;

impl WheelAccumulator {
    /// `horizontal` = wl_pointer axis 1.
    pub fn detents(&mut self, horizontal: bool, v120: i32) {
        if horizontal {
            self.v120.0 += v120;
            self.has_detents.0 = true;
        } else {
            self.v120.1 += v120;
            self.has_detents.1 = true;
        }
    }

    pub fn continuous(&mut self, horizontal: bool, px: f64) {
        if horizontal {
            self.continuous.0 += px;
        } else {
            self.continuous.1 += px;
        }
    }

    /// End of a pointer frame: the `(dx, dy)` to send, or `None` for no motion.
    pub fn flush(&mut self) -> Option<(i32, i32)> {
        let axis = |detents: bool, v120: i32, px: f64, rem: &mut f64| -> i32 {
            if detents {
                *rem = 0.0;
                v120
            } else {
                let notches = px / CONTINUOUS_PX_PER_NOTCH * 120.0 + *rem;
                let whole = notches.trunc();
                *rem = notches - whole;
                whole as i32
            }
        };
        let (mut rem_x, mut rem_y) = self.remainder;
        let x = axis(
            self.has_detents.0,
            self.v120.0,
            self.continuous.0,
            &mut rem_x,
        );
        let y = axis(
            self.has_detents.1,
            self.v120.1,
            self.continuous.1,
            &mut rem_y,
        );
        *self = Self {
            remainder: (rem_x, rem_y),
            ..Self::default()
        };
        // Wayland: +vertical = content moves up (scroll down, towards the user);
        // Windows: +dy = wheel away from the user. Horizontal agrees (+ = right).
        (x != 0 || y != 0).then_some((x, -y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(logical: (u32, u32), scale120: u32) -> Geometry {
        Geometry {
            logical,
            scale120,
            ..Geometry::default()
        }
    }

    #[test]
    fn physical_size_rounds_half_away_from_zero() {
        assert_eq!(geo((1280, 720), 120).physical(), (1280, 720));
        assert_eq!(geo((1280, 720), 180).physical(), (1920, 1080));
        // 125%: 1001 × 1.25 = 1251.25 → 1251; 1002 × 1.25 = 1252.5 → 1253.
        assert_eq!(geo((1001, 1002), 150).physical(), (1251, 1253));
        assert_eq!(geo((0, 0), 240).physical(), (1, 1));
    }

    #[test]
    fn unrevealed_layout_is_full_and_uncropped() {
        let g = Geometry {
            attached: Some((1920, 1080)),
            ..geo((1280, 720), 180)
        };
        let l = g.video_layout();
        assert_eq!(l.source, None);
        assert_eq!(l.destination, (1280, 720));
        assert_eq!(l.offset_y, 0);
    }

    #[test]
    fn revealed_crop_keeps_pixels_in_place_and_inside_the_buffer() {
        for &(logical, scale, buf) in &[
            ((1280u32, 720u32), 180u32, (1920u32, 1080u32)),
            ((1001, 777), 150, (1251, 971)),
            // A stale (pre-resize) buffer: still inside it.
            ((1400, 900), 120, (1280, 720)),
        ] {
            let g = Geometry {
                revealed: true,
                attached: Some(buf),
                ..geo(logical, scale)
            };
            let l = g.video_layout();
            let (x, y, w, h) = l.source.expect("revealed with a buffer crops");
            assert_eq!(x, 0.0);
            assert_eq!(w, buf.0 as f64);
            assert!(y >= 0.0 && y + h <= buf.1 as f64, "{y}+{h} > {}", buf.1);
            assert_eq!(y + h, buf.1 as f64);
            assert_eq!(l.offset_y, BAR_CSS_PX as i32);
            assert_eq!(
                l.destination,
                (logical.0 as i32, (logical.1 - BAR_CSS_PX) as i32)
            );
            // The cropped rows map onto the surface rows they replaced: buffer
            // row `y` lands at logical `offset_y`, the same place it was drawn.
            let row_scale = buf.1 as f64 / logical.1 as f64;
            assert!((y / row_scale - l.offset_y as f64).abs() < 1.0 / 128.0);
        }
    }

    #[test]
    fn reveal_never_produces_an_empty_destination() {
        let g = Geometry {
            revealed: true,
            attached: Some((30, 30)),
            ..geo((30, 30), 120)
        };
        let l = g.video_layout();
        assert!(l.destination.1 >= 1);
    }

    #[test]
    fn reveal_hysteresis() {
        let mut g = geo((1280, 720), 240);
        assert_eq!(
            g.reveal_transition(1.5),
            None,
            "1.5 logical = 3 phys at 200%"
        );
        assert_eq!(g.reveal_transition(1.0), Some(true));
        g.revealed = true;
        assert_eq!(g.reveal_transition(45.0), None, "inside the band");
        assert_eq!(g.reveal_transition(57.0), Some(false));
    }

    #[test]
    fn normalize_matches_letterbox() {
        // 16:9 video in a 4:3 rect: bars top and bottom.
        let (rect, video) = ((800, 600), (1600, 900));
        assert_eq!(normalize(rect, video, 400.0, 300.0), (0.5, 0.5));
        assert_eq!(normalize(rect, video, 0.0, 75.0), (0.0, 0.0));
        assert_eq!(normalize(rect, video, 800.0, 525.0), (1.0, 1.0));
        // Clicks on the black bars clamp.
        assert_eq!(normalize(rect, video, 400.0, 10.0).1, 0.0);
        // Unknown video size → whole rect.
        assert_eq!(normalize(rect, (0, 0), 200.0, 150.0), (0.25, 0.25));
    }

    #[test]
    fn wheel_units_and_signs() {
        let mut w = WheelAccumulator::default();
        // One detent down (towards the user) on Wayland = −120 on Windows.
        w.detents(false, 120);
        w.continuous(false, 15.0);
        assert_eq!(w.flush(), Some((0, -120)));
        assert_eq!(w.flush(), None);
        // Touchpad: 7.5 px twice = one notch, remainder carried.
        w.continuous(true, 7.5);
        assert_eq!(w.flush(), Some((60, 0)));
        w.continuous(true, 7.5);
        assert_eq!(w.flush(), Some((60, 0)));
    }

    #[test]
    fn buttons() {
        assert_eq!(dom_button(0x110), Some(0));
        assert_eq!(dom_button(0x112), Some(1));
        assert_eq!(dom_button(0x111), Some(2));
        assert_eq!(dom_button(0x113), None);
    }
}
