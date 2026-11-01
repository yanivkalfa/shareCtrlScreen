//! Pure geometry of the Wayland video subsurface: logical → physical size
//! under a fractional scale, the viewport crop that reveals the web toolbar,
//! and pointer-axis units. No Wayland or GTK calls, so every rule is
//! unit-tested.
//!
//! ## References
//! * wp_fractional_scale_v1 — scale is `n / 120`; buffers are
//!   `round(logical × scale)`, rounded halfway away from zero:
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/fractional-scale/fractional-scale-v1.xml>
//! * wp_viewport — source rectangle in buffer coordinates (with buffer_scale
//!   1), `out_of_buffer` if it leaves the buffer; destination in surface-local
//!   units: <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/stable/viewporter/viewporter.xml>
//! * wl_surface.set_buffer_scale (buffer size must be a multiple of the scale,
//!   `invalid_size`), wl_pointer.axis / axis_discrete / axis_value120
//!   (positive vertical = down, one detent = 120), wl_pointer button codes
//!   (`BTN_*` from `linux/input-event-codes.h`):
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>

use surface_wgpu::input::WheelAccumulator;
use surface_wgpu::reveal::BAR_CSS_PX;

/// wp_fractional_scale_v1 denominator.
pub(crate) const SCALE_DENOMINATOR: u32 = 120;

/// Everything that decides how the video surfaces are laid out. One instance
/// lives behind the surface lock (see `super::Shared`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// Top-left of the content area in the GTK toplevel's surface (logical px).
    pub(crate) origin: (i32, i32),
    /// Content-area size in logical px (what GTK allocated to the web content).
    pub(crate) logical: (u32, u32),
    /// Preferred scale × 120 (fractional-scale event, else GDK's integer scale).
    pub(crate) scale120: u32,
    /// The top bar strip is cut out of the video (hover reveal active).
    pub(crate) revealed: bool,
    /// Size of the buffer the presenter last committed, if any. The viewport
    /// source rectangle is computed from THIS, never from the target size: the
    /// compositor validates the source against the attached buffer and a miss
    /// is a fatal protocol error (and GDK `_exit(1)`s on display errors).
    pub(crate) attached: Option<(u32, u32)>,
    /// Whether the video has a `wp_viewport` (always in the app window; the
    /// fallback window works without one, at integer scales only).
    pub(crate) viewport: bool,
}

impl Geometry {
    pub(crate) fn new(viewport: bool) -> Self {
        Self {
            origin: (0, 0),
            logical: (1, 1),
            scale120: SCALE_DENOMINATOR,
            revealed: false,
            attached: None,
            viewport,
        }
    }
}

/// The video surface's viewport and position inside its parent area surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VideoLayout {
    /// `wp_viewport.set_source(x, y, w, h)` in buffer px; `None` = unset.
    pub(crate) source: Option<(f64, f64, f64, f64)>,
    /// `wp_viewport.set_destination(w, h)` in logical px (always ≥ 1).
    pub(crate) destination: (i32, i32),
    /// `wl_subsurface.set_position(0, y)` of the video inside the area.
    pub(crate) offset_y: i32,
}

/// `round(len × scale120 / 120)`, halfway away from zero (all inputs ≥ 0).
pub(crate) fn scale_len(len: u32, scale120: u32) -> u32 {
    let scaled = (u64::from(len) * u64::from(scale120) + u64::from(SCALE_DENOMINATOR / 2))
        / u64::from(SCALE_DENOMINATOR);
    scaled.min(u64::from(u32::MAX)) as u32
}

impl Geometry {
    pub(crate) fn scale(&self) -> f64 {
        f64::from(self.scale120) / f64::from(SCALE_DENOMINATOR)
    }

    /// The integer `wl_surface` buffer scale used without a viewport.
    pub(crate) fn int_scale(&self) -> u32 {
        (self.scale120 / SCALE_DENOMINATOR).max(1)
    }

    /// The buffer size the presenter renders at (what `client_size` reports).
    pub(crate) fn physical(&self) -> (u32, u32) {
        if self.viewport {
            (
                scale_len(self.logical.0, self.scale120).max(1),
                scale_len(self.logical.1, self.scale120).max(1),
            )
        } else {
            // Without a viewport the surface is buffer / buffer_scale, so the
            // buffer must be an exact multiple of the integer scale.
            let s = self.int_scale();
            (
                self.logical.0.max(1).saturating_mul(s),
                self.logical.1.max(1).saturating_mul(s),
            )
        }
    }

    /// Logical px currently cut from the top. Never the whole height: a zero
    /// destination height is a `bad_value` protocol error.
    pub(crate) fn bar_logical(&self) -> u32 {
        if self.revealed {
            (BAR_CSS_PX as u32).min(self.logical.1.saturating_sub(1))
        } else {
            0
        }
    }

    /// Viewport + position that show the full-size picture minus the top
    /// strip, with every remaining pixel exactly where it is when unrevealed:
    /// the crop moves the surface down by the strip and drops the same rows
    /// from the buffer, so the picture itself never moves (the Windows
    /// `SetWindowRgn` behaviour, which keeps `client_size` constant too).
    pub(crate) fn video_layout(&self) -> VideoLayout {
        let bar = self.bar_logical();
        let (lw, lh) = (self.logical.0.max(1), self.logical.1.max(1));
        let destination = (clamp_i32(lw), clamp_i32((lh - bar).max(1)));
        let source = match (bar, self.attached) {
            (0, _) | (_, None) => None,
            (bar, Some((bw, bh))) => {
                // Work in wl_fixed units (1/256 px): y is rounded DOWN and the
                // height is whatever remains, so y + height == bh exactly and
                // the rectangle can never leave the buffer (out_of_buffer).
                // Multiples of 1/256 are exact in f64, and wl_fixed conversion
                // multiplies by 256, so these values reach the wire unchanged.
                let y256 = (u64::from(bar) * u64::from(bh) * 256) / u64::from(lh);
                let h256 = (u64::from(bh) * 256).saturating_sub(y256);
                Some((0.0, y256 as f64 / 256.0, f64::from(bw), h256 as f64 / 256.0))
            }
        };
        VideoLayout {
            source,
            destination,
            offset_y: clamp_i32(bar),
        }
    }
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// The `wl_surface.set_buffer_scale` for a buffer of `size` without a
/// viewport: the geometry's integer scale when the buffer is a multiple of it
/// (the protocol's `invalid_size` rule), else 1. A buffer rendered just
/// before a scale change then shows at the wrong size for one frame instead
/// of killing the connection (GDK exits the app on any protocol error).
pub(crate) fn buffer_scale_for(size: (u32, u32), int_scale: u32) -> i32 {
    let s = int_scale.max(1);
    if size.0.is_multiple_of(s) && size.1.is_multiple_of(s) {
        i32::try_from(s).unwrap_or(1)
    } else {
        1
    }
}

/// Linux evdev button → DOM `MouseEvent.button`. Other buttons are dropped,
/// as on Windows (which only forwards left/middle/right).
pub(crate) fn dom_button(evdev: u32) -> Option<u8> {
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

/// Continuous scroll distance that counts as one notch. Weston and wlroots
/// report a wheel detent as 15 px (libinput's default) and older Weston as 10;
/// 15 makes a touchpad feel like a wheel on the host.
const CONTINUOUS_PX_PER_NOTCH: f64 = 15.0;

/// Wheel motion collected over one `wl_pointer.frame`, converted to the
/// protocol's Windows units (±120 per notch, positive `dy` = away from user).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct AxisFrame {
    /// High-resolution detents (axis_value120, or axis_discrete × 120).
    v120: (i32, i32),
    /// Continuous scroll distance (touchpads), in surface-local px.
    continuous: (f64, f64),
    /// Whether any detent arrived this frame (then the continuous value is
    /// the same motion restated and must not be counted twice).
    has_detents: (bool, bool),
    /// Sub-notch remainder of continuous scrolling, carried across frames.
    remainder: WheelAccumulator,
}

impl AxisFrame {
    /// `horizontal` = wl_pointer axis 1.
    pub(crate) fn detents(&mut self, horizontal: bool, v120: i32) {
        if horizontal {
            self.v120.0 = self.v120.0.saturating_add(v120);
            self.has_detents.0 = true;
        } else {
            self.v120.1 = self.v120.1.saturating_add(v120);
            self.has_detents.1 = true;
        }
    }

    pub(crate) fn continuous(&mut self, horizontal: bool, px: f64) {
        if horizontal {
            self.continuous.0 += px;
        } else {
            self.continuous.1 += px;
        }
    }

    /// End of a pointer frame: the `(dx, dy)` to send, or `None` for none.
    pub(crate) fn flush(&mut self) -> Option<(i32, i32)> {
        // 120 / 15 = 8 exactly: one multiplication, no rounding of its own.
        let units = |px: f64| px * (120.0 / CONTINUOUS_PX_PER_NOTCH);
        let (cx, cy) = self.remainder.add(
            if self.has_detents.0 {
                0.0
            } else {
                units(self.continuous.0)
            },
            if self.has_detents.1 {
                0.0
            } else {
                units(self.continuous.1)
            },
        );
        let x = if self.has_detents.0 { self.v120.0 } else { cx };
        let y = if self.has_detents.1 { self.v120.1 } else { cy };
        let remainder = self.remainder;
        *self = Self {
            remainder,
            ..Self::default()
        };
        // Wayland: +vertical = content moves up (scroll down, towards the
        // user); Windows: +dy = wheel away from the user. Horizontal agrees
        // (+ = right).
        (x != 0 || y != 0).then(|| (x, y.saturating_neg()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(logical: (u32, u32), scale120: u32) -> Geometry {
        Geometry {
            logical,
            scale120,
            ..Geometry::new(true)
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
    fn without_a_viewport_the_buffer_is_an_integer_multiple() {
        let g = Geometry {
            logical: (1001, 777),
            scale120: 240,
            ..Geometry::new(false)
        };
        assert_eq!(g.int_scale(), 2);
        assert_eq!(g.physical(), (2002, 1554));
        // A fractional preference is never applied without a viewport.
        let f = Geometry { scale120: 150, ..g };
        assert_eq!(f.int_scale(), 1);
        assert_eq!(f.physical(), (1001, 777));
        assert_eq!(buffer_scale_for((2002, 1554), 2), 2);
        // A stale odd-sized buffer falls back to scale 1, never invalid_size.
        assert_eq!(buffer_scale_for((1001, 777), 2), 1);
        assert_eq!(buffer_scale_for((10, 10), 0), 1);
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
        let bar = BAR_CSS_PX as u32;
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
            let Some((x, y, w, h)) = l.source else {
                panic!("revealed with a buffer must crop");
            };
            assert_eq!(x, 0.0);
            assert_eq!(w, f64::from(buf.0));
            assert!(y >= 0.0 && y + h <= f64::from(buf.1), "{y}+{h} > {}", buf.1);
            assert_eq!(y + h, f64::from(buf.1));
            assert_eq!(l.offset_y, bar as i32);
            assert_eq!(l.destination, (logical.0 as i32, (logical.1 - bar) as i32));
            // The cropped rows map onto the surface rows they replaced: buffer
            // row `y` lands at logical `offset_y`, the same place it was drawn.
            let row_scale = f64::from(buf.1) / f64::from(logical.1);
            assert!((y / row_scale - f64::from(l.offset_y)).abs() < 1.0 / 128.0);
        }
    }

    #[test]
    fn reveal_never_produces_an_empty_destination() {
        let g = Geometry {
            revealed: true,
            attached: Some((30, 30)),
            ..geo((30, 30), 120)
        };
        assert!(g.video_layout().destination.1 >= 1);
    }

    #[test]
    fn wheel_units_and_signs() {
        let mut w = AxisFrame::default();
        // One detent down (towards the user) on Wayland = −120 on Windows; the
        // continuous value restating it is not counted again.
        w.detents(false, 120);
        w.continuous(false, 15.0);
        assert_eq!(w.flush(), Some((0, -120)));
        assert_eq!(w.flush(), None);
        // Touchpad: 7.5 px = half a notch per frame, nothing lost.
        w.continuous(true, 7.5);
        assert_eq!(w.flush(), Some((60, 0)));
        // Sub-unit motion is carried, not lost: 1/16 px = 0.5 units per frame
        // (dyadic values, so the sums are exact).
        w.continuous(true, 0.03125);
        w.continuous(true, 0.03125);
        assert_eq!(w.flush(), None, "0.5 units stay as remainder");
        w.continuous(true, 0.0625);
        assert_eq!(w.flush(), Some((1, 0)));
    }

    #[test]
    fn buttons() {
        assert_eq!(dom_button(0x110), Some(0));
        assert_eq!(dom_button(0x112), Some(1));
        assert_eq!(dom_button(0x111), Some(2));
        assert_eq!(dom_button(0x113), None);
    }
}
