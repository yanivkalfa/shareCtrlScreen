//! Pure translation from the wire's units to each backend's: DOM codes to evdev
//! and X11 keycodes, mouse buttons, Windows wheel units to detents, and
//! normalized points to pixels. No I/O, so every rule here is unit-tested.

use platform_api::keymap::{self, KeyId};
use platform_api::Rect;
use protocol::Button;

/// One wheel detent in the protocol's (Windows) units, which is also the
/// "value120" unit of libei `scroll_discrete` and of `REL_WHEEL_HI_RES`.
pub(super) const WHEEL_DETENT: i32 = 120;

/// The X11 core protocol numbers keycodes from 8; the X.org evdev and libinput
/// drivers map evdev code `n` to keycode `n + 8` (the generated table's note).
const X11_KEYCODE_OFFSET: u16 = 8;

/// Linux `BTN_LEFT` / `BTN_RIGHT` / `BTN_MIDDLE` (input-event-codes.h).
pub(super) const BTN_LEFT: u16 = 0x110;
pub(super) const BTN_RIGHT: u16 = 0x111;
pub(super) const BTN_MIDDLE: u16 = 0x112;

/// The evdev code of a DOM `KeyboardEvent.code`, if Linux has the key.
pub(super) fn evdev_for(dom: &str) -> Option<u16> {
    keymap::from_dom(dom).and_then(KeyId::to_evdev)
}

/// The X11 keycode of an evdev code. `None` past 255: the core protocol's
/// keycode is one byte, so such keys cannot be sent through XTest.
pub(super) fn x11_keycode(evdev: u16) -> Option<u8> {
    u8::try_from(evdev + X11_KEYCODE_OFFSET).ok()
}

/// The evdev button code (uinput, libei, wlroots) of a protocol button.
pub(super) fn evdev_button(b: Button) -> u16 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Middle => BTN_MIDDLE,
        Button::Right => BTN_RIGHT,
    }
}

/// The X11 core pointer button of a protocol button (1 left, 2 middle, 3 right).
pub(super) fn x11_button(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

/// One wheel message after accumulation, in the two forms backends want.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct WheelSteps {
    /// The raw protocol delta (positive = up / right, 120 per detent), for the
    /// high-resolution forms (libei `scroll_discrete`, `REL_*WHEEL_HI_RES`).
    pub v120_x: i32,
    pub v120_y: i32,
    /// Whole detents (positive = up / right), for the forms that can only
    /// click (X11 buttons 4-7, `REL_WHEEL`, wlroots `axis_discrete`).
    pub notches_x: i32,
    pub notches_y: i32,
}

impl WheelSteps {
    pub(super) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Turns protocol wheel deltas into detents. The protocol promises multiples of
/// 120, but a precise touchpad on the viewer could send less; the remainder is
/// kept so that four 30-unit messages still scroll one detent instead of none.
#[derive(Debug, Default)]
pub(super) struct WheelAccumulator {
    rem_x: i32,
    rem_y: i32,
}

impl WheelAccumulator {
    pub(super) fn add(&mut self, dx: i32, dy: i32) -> WheelSteps {
        let (notches_x, rem_x) = split(self.rem_x.saturating_add(dx));
        let (notches_y, rem_y) = split(self.rem_y.saturating_add(dy));
        self.rem_x = rem_x;
        self.rem_y = rem_y;
        WheelSteps {
            v120_x: dx,
            v120_y: dy,
            notches_x,
            notches_y,
        }
    }

    /// Forget a partial detent (session end: a fresh session must not inherit it).
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Whole detents and the remainder, both carrying the sign of `v` (truncation
/// toward zero, so an up-scroll remainder never turns into a down detent).
fn split(v: i32) -> (i32, i32) {
    (v / WHEEL_DETENT, v % WHEEL_DETENT)
}

/// A normalized point and the display it is relative to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Point {
    pub nx: f64,
    pub ny: f64,
    /// [`InputInjector::set_target_rect`](platform_api::InputInjector::set_target_rect);
    /// `None` = the backend's primary display.
    pub target: Option<Rect>,
}

/// The point with "no target" made explicit: the engine sets no target rect
/// for the primary display, so `None` means the *primary monitor's* rectangle
/// (which on X11 need not start at the root origin), never the whole desktop.
/// Stays `None` only when the backend knows no monitor at all.
pub(super) fn on_primary(p: Point, primary: Option<Rect>) -> Point {
    Point {
        target: p.target.or(primary),
        ..p
    }
}

/// The pixel of `rect` under the normalized point. `[0,1]` covers the rect's
/// pixels; `1.0` (the right/bottom edge) lands on the last pixel, not on the
/// first pixel of the neighbouring monitor, and out-of-range or NaN input is
/// clamped for the same reason.
pub(super) fn pixel_in(rect: Rect, nx: f64, ny: f64) -> (i32, i32) {
    (
        rect.x.saturating_add(axis_pixel(nx, rect.w)),
        rect.y.saturating_add(axis_pixel(ny, rect.h)),
    )
}

fn axis_pixel(n: f64, len: u32) -> i32 {
    let last = len.saturating_sub(1);
    let n = if n.is_nan() { 0.0 } else { n.clamp(0.0, 1.0) };
    // Truncation is the intent: the viewer sends a pixel's left/top edge as
    // `px / len`, so `floor(n * len)` gives that pixel back.
    let px = (n * f64::from(len)).floor() as u32;
    i32::try_from(px.min(last)).unwrap_or(i32::MAX)
}

/// The fraction `[0,1)` of `layout` (the bounding box of every monitor, the
/// space an absolute device covers) at which the point lands. With no known
/// target or layout, the target is taken to be the whole layout, which is
/// exact on a single monitor.
pub(super) fn layout_fraction(p: Point, layout: Option<Rect>) -> (f64, f64) {
    match (p.target, layout) {
        (Some(target), Some(layout)) if layout.w > 0 && layout.h > 0 => {
            let (x, y) = pixel_in(target, p.nx, p.ny);
            // The pixel's centre, so rounding at the far end never moves the
            // pointer onto a neighbouring pixel.
            let fx = (f64::from(x) + 0.5 - f64::from(layout.x)) / f64::from(layout.w);
            let fy = (f64::from(y) + 0.5 - f64::from(layout.y)) / f64::from(layout.h);
            (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0))
        }
        _ => {
            let clamp = |n: f64| if n.is_nan() { 0.0 } else { n.clamp(0.0, 1.0) };
            (clamp(p.nx), clamp(p.ny))
        }
    }
}

/// The smallest rectangle holding every rect, or `None` for none.
pub(super) fn bounding_box(rects: &[Rect]) -> Option<Rect> {
    let first = rects.first()?;
    let (mut x0, mut y0) = (i64::from(first.x), i64::from(first.y));
    let (mut x1, mut y1) = (x0 + i64::from(first.w), y0 + i64::from(first.h));
    for r in &rects[1..] {
        x0 = x0.min(i64::from(r.x));
        y0 = y0.min(i64::from(r.y));
        x1 = x1.max(i64::from(r.x) + i64::from(r.w));
        y1 = y1.max(i64::from(r.y) + i64::from(r.h));
    }
    Some(Rect {
        x: i32::try_from(x0).ok()?,
        y: i32::try_from(y0).ok()?,
        w: u32::try_from(x1 - x0).ok()?,
        h: u32::try_from(y1 - y0).ok()?,
    })
}

/// Where an absolute pointer whose valid area is `regions` (libei device
/// regions, compositor logical pixels) should go. The target's centre picks the
/// region so a point at the target's edge can never slip onto a neighbour; with
/// no target, or one no region contains, the first region stands in for the
/// primary display (Wayland has no primary; EIS implementations list the
/// compositor's outputs in order). `None` when the device has no regions.
pub(super) fn region_position(regions: &[Rect], p: Point) -> Option<(f32, f32)> {
    let contains_centre = |t: Rect| {
        let cx = i64::from(t.x) + i64::from(t.w / 2);
        let cy = i64::from(t.y) + i64::from(t.h / 2);
        move |r: &&Rect| {
            cx >= i64::from(r.x)
                && cx < i64::from(r.x) + i64::from(r.w)
                && cy >= i64::from(r.y)
                && cy < i64::from(r.y) + i64::from(r.h)
        }
    };
    let (area, region) = match p.target {
        Some(t) => match regions.iter().find(contains_centre(t)) {
            Some(r) => (t, *r),
            None => (*regions.first()?, *regions.first()?),
        },
        None => (*regions.first()?, *regions.first()?),
    };
    let (x, y) = pixel_in(area, p.nx, p.ny);
    // Clamp into the region: a target slightly larger than its region (a
    // rounding difference between capture and EIS) must not produce a point
    // outside every region, which EIS silently discards.
    let (x, y) = clamp_into(region, x, y);
    // Logical pixels are small integers; f32 represents them exactly.
    Some((x as f32, y as f32))
}

fn clamp_into(r: Rect, x: i32, y: i32) -> (i32, i32) {
    let last = |origin: i32, len: u32| {
        origin.saturating_add(i32::try_from(len.saturating_sub(1)).unwrap_or(i32::MAX))
    };
    (
        x.clamp(r.x, last(r.x, r.w).max(r.x)),
        y.clamp(r.y, last(r.y, r.h).max(r.y)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    fn at(nx: f64, ny: f64, target: Option<Rect>) -> Point {
        Point { nx, ny, target }
    }

    #[test]
    fn keys_go_through_the_generated_table() {
        assert_eq!(evdev_for("KeyA"), Some(30));
        assert_eq!(evdev_for("Enter"), Some(28));
        assert_eq!(evdev_for("ControlLeft"), Some(29));
        assert_eq!(evdev_for("MetaLeft"), Some(125));
        assert_eq!(evdev_for("NumpadEnter"), Some(96));
        assert_eq!(evdev_for("Nonsense"), None);
        assert_eq!(evdev_for("Pause"), None, "excluded from the table");
    }

    #[test]
    fn x11_keycodes_are_evdev_plus_eight_and_fit_a_byte() {
        assert_eq!(x11_keycode(30), Some(38), "KeyA is keycode 38 under evdev");
        assert_eq!(x11_keycode(1), Some(9), "Escape");
        assert_eq!(x11_keycode(247), Some(255));
        assert_eq!(x11_keycode(248), None);
    }

    #[test]
    fn buttons_map_to_evdev_and_x11() {
        assert_eq!(evdev_button(Button::Left), 0x110);
        assert_eq!(evdev_button(Button::Right), 0x111);
        assert_eq!(evdev_button(Button::Middle), 0x112);
        assert_eq!(x11_button(Button::Left), 1);
        assert_eq!(x11_button(Button::Middle), 2);
        assert_eq!(x11_button(Button::Right), 3);
    }

    #[test]
    fn whole_detents_pass_straight_through() {
        let mut acc = WheelAccumulator::default();
        let s = acc.add(-240, 120);
        assert_eq!(
            s,
            WheelSteps {
                v120_x: -240,
                v120_y: 120,
                notches_x: -2,
                notches_y: 1
            }
        );
    }

    #[test]
    fn partial_detents_accumulate_and_keep_their_sign() {
        let mut acc = WheelAccumulator::default();
        for _ in 0..3 {
            let s = acc.add(0, 30);
            assert_eq!(s.notches_y, 0);
            assert_eq!(s.v120_y, 30, "the hi-res value is never held back");
        }
        assert_eq!(acc.add(0, 30).notches_y, 1);
        // A partial up-scroll then a down detent nets to nothing, not to -1.
        assert_eq!(acc.add(0, 60).notches_y, 0);
        assert_eq!(acc.add(0, -120).notches_y, 0);
        assert_eq!(acc.add(0, -60).notches_y, -1);
    }

    #[test]
    fn reset_drops_a_partial_detent() {
        let mut acc = WheelAccumulator::default();
        acc.add(90, 90);
        acc.reset();
        let s = acc.add(30, 30);
        assert_eq!((s.notches_x, s.notches_y), (0, 0));
    }

    #[test]
    fn huge_wheel_values_do_not_overflow() {
        let mut acc = WheelAccumulator::default();
        acc.add(i32::MAX, i32::MIN);
        let s = acc.add(i32::MAX, i32::MIN);
        assert!(s.notches_x > 0 && s.notches_y < 0);
    }

    #[test]
    fn pixels_cover_the_rect_and_never_leave_it() {
        let r = rect(1920, 0, 1280, 1024);
        assert_eq!(pixel_in(r, 0.0, 0.0), (1920, 0));
        assert_eq!(pixel_in(r, 0.5, 0.5), (2560, 512));
        assert_eq!(
            pixel_in(r, 1.0, 1.0),
            (3199, 1023),
            "last pixel, not the next monitor"
        );
        assert_eq!(pixel_in(r, -3.0, 7.0), (1920, 1023));
        assert_eq!(pixel_in(r, f64::NAN, f64::NAN), (1920, 0));
        assert_eq!(pixel_in(rect(0, 0, 0, 0), 0.5, 0.5), (0, 0));
        // The viewer's `px / w` comes back as the same pixel.
        assert_eq!(
            pixel_in(rect(0, 0, 1280, 720), 639.0 / 1280.0, 0.0),
            (639, 0)
        );
    }

    #[test]
    fn layout_fraction_places_a_monitor_inside_the_layout() {
        // Two 1000-px monitors side by side; the right one is the target.
        let layout = Some(rect(0, 0, 2000, 1000));
        let right = Some(rect(1000, 0, 1000, 1000));
        let (fx, fy) = layout_fraction(at(0.0, 0.0, right), layout);
        assert!((fx - 1000.5 / 2000.0).abs() < 1e-9 && (fy - 0.5 / 1000.0).abs() < 1e-9);
        let (fx, _) = layout_fraction(at(1.0, 0.0, right), layout);
        assert!(fx < 1.0 && fx > 0.999);
        // A layout with a negative origin (monitor left of the primary).
        let (fx, _) = layout_fraction(
            at(0.0, 0.0, Some(rect(0, 0, 1000, 1000))),
            Some(rect(-1000, 0, 2000, 1000)),
        );
        assert!((fx - 1000.5 / 2000.0).abs() < 1e-9);
    }

    #[test]
    fn no_target_means_the_primary_monitor_not_the_root() {
        // RandR primary to the right of a secondary monitor at the origin.
        let primary = rect(1920, 0, 2560, 1440);
        let root = rect(0, 0, 4480, 1440);
        let p = on_primary(at(0.0, 0.0, None), Some(primary));
        assert_eq!(p.target, Some(primary));
        assert_eq!(pixel_in(p.target.unwrap_or(root), p.nx, p.ny), (1920, 0));
        let (fx, _) = layout_fraction(on_primary(at(0.0, 0.5, None), Some(primary)), Some(root));
        assert!(
            (fx - 1920.5 / 4480.0).abs() < 1e-9,
            "absolute devices land on it too"
        );
        // An explicit target wins; no known monitor leaves it unset.
        let other = Some(rect(0, 0, 1920, 1080));
        assert_eq!(on_primary(at(0.5, 0.5, other), Some(primary)).target, other);
        assert_eq!(on_primary(at(0.5, 0.5, None), None).target, None);
    }

    #[test]
    fn layout_fraction_without_geometry_is_the_normalized_point() {
        assert_eq!(layout_fraction(at(0.25, 0.75, None), None), (0.25, 0.75));
        assert_eq!(
            layout_fraction(at(0.25, 0.75, Some(rect(0, 0, 10, 10))), None),
            (0.25, 0.75)
        );
        assert_eq!(layout_fraction(at(2.0, -1.0, None), None), (1.0, 0.0));
    }

    #[test]
    fn bounding_box_spans_every_monitor() {
        assert_eq!(bounding_box(&[]), None);
        assert_eq!(
            bounding_box(&[rect(0, 0, 1920, 1080), rect(-1280, 200, 1280, 1024)]),
            Some(rect(-1280, 0, 3200, 1224))
        );
    }

    #[test]
    fn region_position_uses_the_region_holding_the_target() {
        let regions = [rect(0, 0, 1920, 1080), rect(1920, 0, 1280, 1024)];
        let right = Some(rect(1920, 0, 1280, 1024));
        assert_eq!(
            region_position(&regions, at(0.0, 0.0, right)),
            Some((1920.0, 0.0))
        );
        assert_eq!(
            region_position(&regions, at(1.0, 1.0, right)),
            Some((3199.0, 1023.0))
        );
    }

    #[test]
    fn region_position_clamps_an_oversized_target_into_its_region() {
        let regions = [rect(0, 0, 1920, 1080)];
        let bigger = Some(rect(0, 0, 1922, 1082));
        assert_eq!(
            region_position(&regions, at(1.0, 1.0, bigger)),
            Some((1919.0, 1079.0))
        );
    }

    #[test]
    fn region_position_falls_back_to_the_first_region() {
        let regions = [rect(100, 50, 800, 600), rect(900, 50, 800, 600)];
        assert_eq!(
            region_position(&regions, at(0.5, 0.5, None)),
            Some((500.0, 350.0))
        );
        let elsewhere = Some(rect(5000, 5000, 10, 10));
        assert_eq!(
            region_position(&regions, at(0.5, 0.5, elsewhere)),
            Some((500.0, 350.0))
        );
        assert_eq!(region_position(&[], at(0.5, 0.5, None)), None);
    }
}
