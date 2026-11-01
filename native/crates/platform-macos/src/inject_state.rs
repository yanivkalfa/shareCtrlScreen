//! The pure half of the CGEvent injector (`injector.rs` posts the events):
//! where a normalized point lands on the desktop, which modifier flags each
//! event carries, how the protocol's wheel units become scroll pixels, how
//! presses become single and double clicks, and which window a point hits.
//! No OS calls, so every rule here is unit-tested.
//!
//! ## Coordinates: capture pixels → injection points
//!
//! CoreGraphics events are positioned in the global display space in
//! **points**: origin at the top-left of the main display, y down, the other
//! displays placed around it as `CGDisplayBounds` reports. ScreenCaptureKit
//! delivers the shared display in **pixels**, `scale` × its point size on a
//! Retina panel. The wire carries positions normalized over the captured
//! picture, so pixel `(px, py)` of a `W × H` capture arrives as
//! `(px / W, py / H)` and lands at `target.x + px / W · target.w`,
//! `target.y + py / H · target.h` points. The backing scale cancels out, so
//! the injector never needs it. `target` is the shared display's bounds in
//! points, which is what `DisplayInfo` reports on macOS (`displays.rs`) and
//! therefore what the engine passes to `set_target_rect`.
//!
//! ## References
//! * `CGEventCreateMouseEvent` (position in global display coordinates):
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/init(mouseeventsource:mousetype:mousecursorposition:mousebutton:)>
//! * `CGDisplayBounds` (global display space, points):
//!   <https://developer.apple.com/documentation/coregraphics/cgdisplaybounds(_:)>
//! * `CGEventField.mouseEventClickState` (the click count apps read for
//!   double-clicks): <https://developer.apple.com/documentation/coregraphics/cgeventfield/mouseeventclickstate>
//! * `CGEventSourceSetPixelsPerLine`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventsource/pixelsperline>
//! * `CGWindowListCopyWindowInfo` and the window dictionary keys
//!   (`kCGWindowLayer`, `kCGWindowAlpha`, `kCGWindowBounds`):
//!   <https://developer.apple.com/documentation/coregraphics/cgwindowlistcopywindowinfo(_:_:)>
//! * Win32 `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL` (the wire's wheel units
//!   and signs): <https://learn.microsoft.com/windows/win32/api/winuser/ns-winuser-mouseinput>
//! * Win32 `SM_CXDOUBLECLK` (the double-click rectangle, 4 px by default):
//!   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getsystemmetrics>

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use platform_api::Rect;
use protocol::Button;

use crate::convert::RectF;
use crate::keymap::{self, CAPS_LOCK, FLAG_CAPS_LOCK, MODIFIERS};

/// Written to `kCGEventSourceUserData` of every injected event so the
/// viewer's own event tap (task M6) recognizes and ignores our synthetic
/// input instead of forwarding it back. Same value as the Windows
/// `input::INJECT_SENTINEL` ("SCRS").
pub(crate) const INJECT_SENTINEL: i64 = 0x5343_5253;

/// Every `CGEventFlags` bit the injector decides: the modifier flags, their
/// side-specific device bits and Caps Lock. All other bits of an event are
/// left as CoreGraphics created them.
pub(crate) const OWNED_FLAGS: u64 = {
    let mut mask = FLAG_CAPS_LOCK;
    let mut i = 0;
    while i < MODIFIERS.len() {
        mask |= MODIFIERS[i].flag | MODIFIERS[i].device_bit;
        i += 1;
    }
    mask
};

// ---- coordinates -------------------------------------------------------------

/// A target rectangle (points on macOS, see the module docs) as floats.
pub(crate) fn rect_f(r: Rect) -> RectF {
    RectF {
        x: f64::from(r.x),
        y: f64::from(r.y),
        w: f64::from(r.w),
        h: f64::from(r.h),
    }
}

/// The global point a normalized `(nx, ny)` names inside `target`.
///
/// `nx = px / W` addresses the left edge of a pixel, so `x + nx · w` is the
/// exact inverse; the result is clamped to the last point inside the
/// display, because `nx = 1.0` would otherwise name the first point of the
/// display to the right (or no display at all, which macOS clamps to some
/// other edge). Values outside `[0, 1]` or not finite are clamped too: the
/// viewer may send a position from just outside the picture during a drag.
pub(crate) fn global_point(target: RectF, nx: f64, ny: f64) -> (f64, f64) {
    fn axis(origin: f64, len: f64, n: f64) -> f64 {
        let n = if n.is_finite() {
            n.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let last = origin + (len - 1.0).max(0.0);
        (origin + n * len).min(last)
    }
    (axis(target.x, target.w, nx), axis(target.y, target.h, ny))
}

// ---- keyboard ----------------------------------------------------------------

/// The CoreGraphics event type a key change is posted as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    Down,
    Up,
    /// Modifier keys and Caps Lock: real keyboards produce `flagsChanged`,
    /// never key-down/up, and AppKit only reads modifiers from it.
    FlagsChanged {
        down: bool,
    },
}

/// One keyboard event to post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyEvent {
    /// `kVK_*` code.
    pub key_code: u16,
    pub kind: KeyKind,
    /// A key-down for a key that is already down (the viewer's key repeat):
    /// sets `kCGKeyboardEventAutorepeat`, as the hardware repeat does.
    pub autorepeat: bool,
    /// The [`OWNED_FLAGS`] bits as they are **after** this event, which is
    /// how macOS reports them (a Shift-down already carries Shift).
    pub flags: u64,
}

/// Held keys and the Caps Lock state of the injected keyboard.
///
/// Why the flags are tracked here and set on every event: a posted modifier
/// key-down does not make CoreGraphics add that modifier to later posted
/// events, so without explicit flags Cmd+V would arrive as a plain "v".
/// Chromium's remoting host (`input_injector_mac.cc`) does the same.
#[derive(Debug, Default)]
pub(crate) struct Keyboard {
    held: BTreeSet<u16>,
    caps_lock: bool,
}

impl Keyboard {
    /// `caps_lock`: the Mac's own lock state when the session starts, so the
    /// remote user's first letters have the case the Mac shows.
    pub fn new(caps_lock: bool) -> Self {
        Self {
            held: BTreeSet::new(),
            caps_lock,
        }
    }

    /// The [`OWNED_FLAGS`] bits for the current state.
    pub fn flags(&self) -> u64 {
        let modifiers = MODIFIERS
            .iter()
            .filter(|m| self.held.contains(&m.key_code))
            .fold(0, |acc, m| acc | m.flag | m.device_bit);
        if self.caps_lock {
            modifiers | FLAG_CAPS_LOCK
        } else {
            modifiers
        }
    }

    /// The event for a press or release of `key_code`, or `None` when
    /// nothing must be posted.
    ///
    /// * Caps Lock: macOS keyboards report the lock toggling, not the key,
    ///   so each press toggles the lock (as on the Windows host) and its
    ///   release posts nothing. macOS has no Num Lock: the DOM `NumLock` code
    ///   is the keypad Clear key in the generated table and is an ordinary key.
    /// * A repeated press of a held modifier posts nothing (keyboards do not
    ///   repeat modifiers); of any other key, an autorepeat key-down.
    /// * A release is always posted, held or not: a key-up the app did not
    ///   expect is harmless, a missing one leaves a key stuck.
    pub fn key(&mut self, key_code: u16, down: bool) -> Option<KeyEvent> {
        if key_code == CAPS_LOCK {
            if !down {
                return None;
            }
            self.caps_lock = !self.caps_lock;
            return Some(KeyEvent {
                key_code,
                kind: KeyKind::FlagsChanged {
                    down: self.caps_lock,
                },
                autorepeat: false,
                flags: self.flags(),
            });
        }
        let is_modifier = keymap::modifier(key_code).is_some();
        let repeat = if down {
            !self.held.insert(key_code)
        } else {
            self.held.remove(&key_code);
            false
        };
        if repeat && is_modifier {
            return None;
        }
        let kind = match (is_modifier, down) {
            (true, down) => KeyKind::FlagsChanged { down },
            (false, true) => KeyKind::Down,
            (false, false) => KeyKind::Up,
        };
        Some(KeyEvent {
            key_code,
            kind,
            autorepeat: repeat,
            flags: self.flags(),
        })
    }

    /// Releases for every held key: ordinary keys first, then modifiers, so
    /// no key is released into a different chord than it was pressed in.
    /// The Caps Lock state is kept, as a real keyboard's lock is.
    pub fn release_all(&mut self) -> Vec<KeyEvent> {
        let (modifiers, others): (Vec<u16>, Vec<u16>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|&k| keymap::modifier(k).is_some());
        // Re-insert so each release below carries the flags of what is still
        // held after it.
        self.held = modifiers.iter().copied().collect();
        let mut events = Vec::with_capacity(others.len() + modifiers.len());
        for key_code in others.into_iter().chain(modifiers) {
            if let Some(event) = self.key(key_code, false) {
                events.push(event);
            }
        }
        events
    }
}

// ---- mouse -------------------------------------------------------------------

/// The CoreGraphics family a mouse event belongs to: `Left*`, `Right*` or
/// `Other*` (`CGEventType`), with `CGMouseButton` 0, 1 or 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CgButton {
    Left,
    Right,
    Center,
}

impl From<Button> for CgButton {
    fn from(b: Button) -> Self {
        match b {
            Button::Left => CgButton::Left,
            Button::Right => CgButton::Right,
            Button::Middle => CgButton::Center,
        }
    }
}

/// How a pointer move is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveKind {
    Moved,
    /// A button is held: macOS delivers drags only as `*MouseDragged`; a plain
    /// `mouseMoved` while a button is down does not drag anything.
    Dragged(CgButton),
}

/// Distance (points) within which a second press still counts as the same
/// click run. macOS has no public setting for it; the Windows default
/// double-click rectangle is 4 px, and the viewer reproduces the user's own
/// double-click, so only a little coordinate rounding has to be absorbed.
const CLICK_SLOP: f64 = 4.0;

/// The last press, for counting double and triple clicks.
#[derive(Debug, Clone, Copy)]
struct LastPress {
    button: CgButton,
    at: Instant,
    x: f64,
    y: f64,
    count: i64,
}

/// Held buttons, click counting and the last injected position.
///
/// Why the click count: CoreGraphics creates every mouse-down with a click
/// state of 1, and apps read `kCGMouseEventClickState` (not timing) to tell a
/// double-click; without it a remote double-click opens nothing.
#[derive(Debug, Default)]
pub(crate) struct Mouse {
    held: [bool; 3],
    last_press: Option<LastPress>,
    position: Option<(f64, f64)>,
}

fn index(b: CgButton) -> usize {
    match b {
        CgButton::Left => 0,
        CgButton::Right => 1,
        CgButton::Center => 2,
    }
}

impl Mouse {
    /// The type of a move posted now. With several buttons held, the
    /// lowest-numbered one names the drag, as the HID system does.
    pub fn move_kind(&self) -> MoveKind {
        [CgButton::Left, CgButton::Right, CgButton::Center]
            .into_iter()
            .find(|&b| self.held[index(b)])
            .map_or(MoveKind::Moved, MoveKind::Dragged)
    }

    /// Records a pointer move to `(x, y)` and returns the motion since the
    /// previous one for `kCGMouseEventDeltaX/Y`, which CoreGraphics leaves at
    /// zero and which apps that track relative motion (3D views, games) read.
    pub fn moved_to(&mut self, x: f64, y: f64) -> (i64, i64) {
        let delta = self.position.map_or((0, 0), |(px, py)| {
            ((x - px).round() as i64, (y - py).round() as i64)
        });
        self.position = Some((x, y));
        delta
    }

    /// The last position this injector moved the pointer to.
    pub fn position(&self) -> Option<(f64, f64)> {
        self.position
    }

    /// Records a press at `(x, y)` and returns its click count: one more than
    /// the previous press when it is the same button, within `interval`
    /// (the user's double-click speed) and within [`CLICK_SLOP`], else 1.
    pub fn press(&mut self, b: CgButton, at: Instant, x: f64, y: f64, interval: Duration) -> i64 {
        self.held[index(b)] = true;
        let count = match self.last_press {
            Some(last)
                if last.button == b
                    && at.saturating_duration_since(last.at) <= interval
                    && (x - last.x).abs() <= CLICK_SLOP
                    && (y - last.y).abs() <= CLICK_SLOP =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_press = Some(LastPress {
            button: b,
            at,
            x,
            y,
            count,
        });
        count
    }

    /// Records a release and returns its click count, which macOS reports
    /// as that of the press it ends.
    pub fn release(&mut self, b: CgButton) -> i64 {
        self.held[index(b)] = false;
        self.last_press
            .filter(|last| last.button == b)
            .map_or(1, |last| last.count)
    }

    /// Forgets every held button (they are being released) and the click
    /// run, so the next session's first click is a single click.
    pub fn release_all(&mut self) -> Vec<CgButton> {
        let held = [CgButton::Left, CgButton::Right, CgButton::Center]
            .into_iter()
            .filter(|&b| self.held[index(b)])
            .collect();
        self.held = [false; 3];
        self.last_press = None;
        held
    }
}

// ---- wheel -------------------------------------------------------------------

/// Wire units per wheel notch (Win32 `WHEEL_DELTA`).
const UNITS_PER_NOTCH: i64 = 120;
/// Lines per notch, the Windows default (`SPI_GETWHEELSCROLLLINES`), so a
/// notch scrolls a Mac app as far as it scrolls a Windows one.
const LINES_PER_NOTCH: i64 = 3;
/// Pixels per notch: the viewer (`view.rs`) turns 40 trackpad pixels into one
/// notch, so the host turns a notch back into 40 pixels and a Mac-to-Mac swipe
/// scrolls the remote as far as a local one.
const PIXELS_PER_NOTCH: i64 = 40;
/// Wire units per scroll pixel. Exact (asserted below), so the remainder can
/// be kept in whole wire units and nothing is lost to float rounding.
const UNITS_PER_PIXEL: i64 = UNITS_PER_NOTCH / PIXELS_PER_NOTCH;
const _: () = assert!(UNITS_PER_NOTCH % PIXELS_PER_NOTCH == 0);
/// Set on the event source with `CGEventSourceSetPixelsPerLine`, so that a
/// pixel-unit event also reports [`LINES_PER_NOTCH`] lines per notch to apps
/// that read line deltas.
pub(crate) const PIXELS_PER_LINE: f64 = PIXELS_PER_NOTCH as f64 / LINES_PER_NOTCH as f64;

/// Turns wire wheel units into whole scroll pixels, keeping the remainder.
///
/// Pixel units, not lines: the wire carries fractions of a notch (a trackpad
/// viewer sends a few units at a time) and line-unit events are whole lines,
/// which would drop slow scrolling entirely.
#[derive(Debug, Default)]
pub(crate) struct Wheel {
    /// Wire units not yet sent, always within `±(UNITS_PER_PIXEL - 1)`.
    x: i64,
    y: i64,
}

impl Wheel {
    /// `(vertical, horizontal)` pixels for `CGEventCreateScrollWheelEvent`'s
    /// `wheel1` and `wheel2`. Signs: the wire's `+dy` is Win32's wheel away
    /// from the user (scroll up) and `+wheel1` is CoreGraphics' scroll up, so
    /// it passes through; the wire's `+dx` is Win32's tilt right but
    /// `+wheel2` scrolls left, so it is negated.
    pub fn pixels(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        fn take(acc: &mut i64, units: i64) -> i32 {
            *acc += units;
            // Truncates toward zero, so the remainder keeps its sign.
            let whole = *acc / UNITS_PER_PIXEL;
            *acc -= whole * UNITS_PER_PIXEL;
            // |whole| ≤ (|i32::MIN| + UNITS_PER_PIXEL) / UNITS_PER_PIXEL, which
            // fits an i32.
            whole as i32
        }
        let vertical = take(&mut self.y, i64::from(dy));
        let horizontal = take(&mut self.x, -i64::from(dx));
        (vertical, horizontal)
    }
}

// ---- window hit-testing --------------------------------------------------------

/// `kCGNormalWindowLevel`: document and app windows. Menus, the menu bar,
/// the Dock, notifications and overlay windows (screenshot tools, the
/// cursor's own layers) sit above it and are never a paste target.
const NORMAL_WINDOW_LAYER: i64 = 0;

/// The fields of one `CGWindowListCopyWindowInfo` entry the hit test needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowInfo {
    pub pid: i32,
    pub layer: i64,
    pub alpha: f64,
    /// Global display space, points.
    pub bounds: RectF,
}

/// The owner of the front-most normal window containing `(x, y)`.
/// `windows` must be in the window server's front-to-back order, which is
/// how `CGWindowListCopyWindowInfo` returns them.
pub(crate) fn window_at(
    windows: impl IntoIterator<Item = WindowInfo>,
    x: f64,
    y: f64,
) -> Option<i32> {
    windows
        .into_iter()
        .find(|w| {
            // A fully transparent window is invisible to the user and is not
            // what they dropped on.
            w.layer == NORMAL_WINDOW_LAYER
                && w.alpha > 0.0
                && x >= w.bounds.x
                && x < w.bounds.x + w.bounds.w
                && y >= w.bounds.y
                && y < w.bounds.y + w.bounds.h
        })
        .map(|w| w.pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::keymap::from_dom;

    fn mac(dom: &str) -> u16 {
        from_dom(dom)
            .and_then(|k| k.to_mac())
            .unwrap_or_else(|| panic!("{dom} has no macOS key"))
    }

    const SHIFT: u64 = 0x0002_0000;
    const COMMAND: u64 = 0x0010_0000;

    // ---- coordinates

    #[test]
    fn a_normalized_point_maps_into_the_target_in_points() {
        // A Retina display right of a 1440-point main display: 1512×982
        // points (3024×1964 pixels captured). The scale never enters.
        let t = rect_f(Rect {
            x: 1440,
            y: -100,
            w: 1512,
            h: 982,
        });
        assert_eq!(global_point(t, 0.0, 0.0), (1440.0, -100.0));
        assert_eq!(global_point(t, 0.5, 0.5), (1440.0 + 756.0, -100.0 + 491.0));
        // Pixel 1512 of 3024 is the middle point.
        assert_eq!(global_point(t, 1512.0 / 3024.0, 0.0).0, 1440.0 + 756.0);
    }

    #[test]
    fn the_far_edge_stays_on_the_display() {
        let t = rect_f(Rect {
            x: 0,
            y: 0,
            w: 1440,
            h: 900,
        });
        assert_eq!(global_point(t, 1.0, 1.0), (1439.0, 899.0));
        assert_eq!(global_point(t, 1.7, -0.3), (1439.0, 0.0));
        assert_eq!(global_point(t, f64::NAN, f64::INFINITY), (0.0, 0.0));
    }

    // ---- keyboard

    #[test]
    fn owned_flags_cover_every_modifier_and_caps_lock() {
        assert_eq!(OWNED_FLAGS & 0x001F_0000, 0x001F_0000);
        assert_eq!(OWNED_FLAGS & 0x207F, 0x207F);
        // Not ours: NumericPad, SecondaryFn, NonCoalesced.
        assert_eq!(OWNED_FLAGS & (0x0020_0000 | 0x0080_0000 | 0x100), 0);
    }

    #[test]
    fn cmd_v_carries_command_on_the_v() {
        let mut k = Keyboard::new(false);
        let cmd = k.key(mac("MetaLeft"), true).unwrap();
        assert_eq!(cmd.kind, KeyKind::FlagsChanged { down: true });
        assert_eq!(cmd.flags, COMMAND | 0x08);
        let v = k.key(mac("KeyV"), true).unwrap();
        assert_eq!(v.kind, KeyKind::Down);
        assert_eq!(v.flags, COMMAND | 0x08);
        assert_eq!(k.key(mac("KeyV"), false).unwrap().kind, KeyKind::Up);
        let up = k.key(mac("MetaLeft"), false).unwrap();
        assert_eq!(up.kind, KeyKind::FlagsChanged { down: false });
        assert_eq!(up.flags, 0);
    }

    #[test]
    fn both_shifts_keep_shift_until_the_last_is_released() {
        let mut k = Keyboard::new(false);
        k.key(mac("ShiftLeft"), true);
        k.key(mac("ShiftRight"), true);
        assert_eq!(k.flags(), SHIFT | 0x02 | 0x04);
        let first = k.key(mac("ShiftRight"), false).unwrap();
        assert_eq!(first.flags, SHIFT | 0x02);
        let last = k.key(mac("ShiftLeft"), false).unwrap();
        assert_eq!(last.flags, 0);
    }

    #[test]
    fn repeats_autorepeat_keys_but_not_modifiers() {
        let mut k = Keyboard::new(false);
        assert!(!k.key(mac("KeyA"), true).unwrap().autorepeat);
        assert!(k.key(mac("KeyA"), true).unwrap().autorepeat);
        k.key(mac("ShiftLeft"), true);
        assert_eq!(k.key(mac("ShiftLeft"), true), None);
    }

    #[test]
    fn a_release_is_posted_even_for_a_key_never_pressed() {
        let mut k = Keyboard::new(false);
        assert_eq!(k.key(mac("KeyQ"), false).unwrap().kind, KeyKind::Up);
        assert_eq!(
            k.key(mac("AltLeft"), false).unwrap().kind,
            KeyKind::FlagsChanged { down: false }
        );
    }

    #[test]
    fn caps_lock_toggles_on_each_press_and_its_release_posts_nothing() {
        let mut k = Keyboard::new(false);
        let on = k.key(CAPS_LOCK, true).unwrap();
        assert_eq!(on.kind, KeyKind::FlagsChanged { down: true });
        assert_eq!(on.flags, FLAG_CAPS_LOCK);
        assert_eq!(k.key(CAPS_LOCK, false), None);
        assert_eq!(k.key(mac("KeyA"), true).unwrap().flags, FLAG_CAPS_LOCK);
        let off = k.key(CAPS_LOCK, true).unwrap();
        assert_eq!(off.kind, KeyKind::FlagsChanged { down: false });
        assert_eq!(off.flags, 0);
    }

    #[test]
    fn caps_lock_starts_in_the_macs_state_and_survives_release_all() {
        let mut k = Keyboard::new(true);
        assert_eq!(k.flags(), FLAG_CAPS_LOCK);
        k.key(mac("ShiftLeft"), true);
        k.release_all();
        assert_eq!(k.flags(), FLAG_CAPS_LOCK);
    }

    #[test]
    fn numlock_is_the_keypad_clear_key_and_an_ordinary_key() {
        let clear = mac("NumLock");
        assert_eq!(clear, 0x47); // kVK_ANSI_KeypadClear
        let mut k = Keyboard::new(false);
        assert_eq!(k.key(clear, true).unwrap().kind, KeyKind::Down);
        assert_eq!(k.key(clear, false).unwrap().kind, KeyKind::Up);
        assert_eq!(k.flags(), 0);
    }

    #[test]
    fn release_all_releases_keys_before_modifiers_and_clears_flags() {
        let mut k = Keyboard::new(false);
        k.key(mac("MetaLeft"), true);
        k.key(mac("ShiftLeft"), true);
        k.key(mac("KeyZ"), true);
        let events = k.release_all();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].key_code, mac("KeyZ"));
        assert_eq!(events[0].kind, KeyKind::Up);
        // Z is released while Cmd+Shift are still down, as it was pressed.
        assert_eq!(events[0].flags, COMMAND | SHIFT | 0x08 | 0x02);
        assert!(events[1..]
            .iter()
            .all(|e| e.kind == KeyKind::FlagsChanged { down: false }));
        assert_eq!(events[2].flags, 0);
        assert_eq!(k.flags(), 0);
        assert!(k.release_all().is_empty());
    }

    // ---- mouse

    #[test]
    fn moves_with_a_button_held_are_drags_of_that_button() {
        let now = Instant::now();
        let ms = Duration::from_millis(500);
        let mut m = Mouse::default();
        assert_eq!(m.move_kind(), MoveKind::Moved);
        m.press(CgButton::Right, now, 0.0, 0.0, ms);
        assert_eq!(m.move_kind(), MoveKind::Dragged(CgButton::Right));
        m.press(CgButton::Left, now, 0.0, 0.0, ms);
        assert_eq!(m.move_kind(), MoveKind::Dragged(CgButton::Left));
        m.release(CgButton::Left);
        m.release(CgButton::Right);
        assert_eq!(m.move_kind(), MoveKind::Moved);
    }

    #[test]
    fn quick_presses_in_place_count_up_and_releases_repeat_the_count() {
        let t0 = Instant::now();
        let interval = Duration::from_millis(500);
        let mut m = Mouse::default();
        assert_eq!(m.press(CgButton::Left, t0, 10.0, 10.0, interval), 1);
        assert_eq!(m.release(CgButton::Left), 1);
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(m.press(CgButton::Left, t1, 12.0, 9.0, interval), 2);
        assert_eq!(m.release(CgButton::Left), 2);
        let t2 = t1 + Duration::from_millis(200);
        assert_eq!(m.press(CgButton::Left, t2, 12.0, 9.0, interval), 3);
    }

    #[test]
    fn a_slow_far_or_other_button_press_starts_a_new_run() {
        let t0 = Instant::now();
        let interval = Duration::from_millis(500);
        let mut m = Mouse::default();
        m.press(CgButton::Left, t0, 10.0, 10.0, interval);
        let late = t0 + Duration::from_millis(501);
        assert_eq!(m.press(CgButton::Left, late, 10.0, 10.0, interval), 1);
        let soon = late + Duration::from_millis(100);
        assert_eq!(m.press(CgButton::Left, soon, 20.0, 10.0, interval), 1);
        assert_eq!(m.press(CgButton::Right, soon, 20.0, 10.0, interval), 1);
        // The left release still reports a single click.
        assert_eq!(m.release(CgButton::Left), 1);
    }

    #[test]
    fn release_all_returns_held_buttons_and_forgets_the_run() {
        let t0 = Instant::now();
        let interval = Duration::from_millis(500);
        let mut m = Mouse::default();
        m.press(CgButton::Left, t0, 0.0, 0.0, interval);
        m.press(CgButton::Center, t0, 0.0, 0.0, interval);
        assert_eq!(m.release_all(), vec![CgButton::Left, CgButton::Center]);
        assert_eq!(m.move_kind(), MoveKind::Moved);
        assert_eq!(m.press(CgButton::Left, t0, 0.0, 0.0, interval), 1);
    }

    #[test]
    fn deltas_follow_the_injected_positions() {
        let mut m = Mouse::default();
        assert_eq!(m.moved_to(100.0, 100.0), (0, 0));
        assert_eq!(m.moved_to(103.4, 90.0), (3, -10));
        assert_eq!(m.position(), Some((103.4, 90.0)));
    }

    #[test]
    fn protocol_buttons_map_to_coregraphics_families() {
        assert_eq!(CgButton::from(Button::Left), CgButton::Left);
        assert_eq!(CgButton::from(Button::Right), CgButton::Right);
        assert_eq!(CgButton::from(Button::Middle), CgButton::Center);
    }

    // ---- wheel

    #[test]
    fn a_notch_is_forty_pixels_and_three_lines() {
        let mut w = Wheel::default();
        assert_eq!(w.pixels(0, 120), (40, 0));
        assert_eq!(w.pixels(0, -240), (-80, 0));
        assert_eq!(PIXELS_PER_NOTCH as f64 / PIXELS_PER_LINE, 3.0);
    }

    #[test]
    fn horizontal_is_negated_and_vertical_passes_through() {
        let mut w = Wheel::default();
        // Win32 tilt right (+dx) → CoreGraphics wheel2 negative.
        assert_eq!(w.pixels(120, 0), (0, -40));
        assert_eq!(w.pixels(-120, 0), (0, 40));
    }

    #[test]
    fn fractions_of_a_pixel_accumulate() {
        let mut w = Wheel::default();
        // 1 unit = 1/3 pixel.
        assert_eq!(w.pixels(0, 1), (0, 0));
        assert_eq!(w.pixels(0, 1), (0, 0));
        assert_eq!(w.pixels(0, 1), (1, 0));
        // The remainder keeps its sign: -2 units is not yet a pixel up.
        assert_eq!(w.pixels(0, -2), (0, 0));
        assert_eq!(w.pixels(0, -1), (-1, 0));
    }

    #[test]
    fn extreme_deltas_do_not_overflow() {
        let mut w = Wheel::default();
        assert_eq!(w.pixels(0, i32::MAX), (i32::MAX / 3, 0));
        assert_eq!(
            w.pixels(i32::MIN, i32::MIN),
            (i32::MIN / 3, -(i32::MIN / 3))
        );
    }

    // ---- windows

    fn window(pid: i32, layer: i64, alpha: f64, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            pid,
            layer,
            alpha,
            bounds: RectF { x, y, w, h },
        }
    }

    #[test]
    fn the_front_most_normal_window_under_the_point_wins() {
        let list = [
            window(1, 25, 1.0, 0.0, 0.0, 2000.0, 25.0),  // menu bar
            window(2, 0, 0.0, 0.0, 0.0, 2000.0, 2000.0), // invisible overlay
            window(3, 0, 1.0, 100.0, 100.0, 400.0, 300.0),
            window(4, 0, 1.0, 0.0, 0.0, 1440.0, 900.0),
        ];
        assert_eq!(window_at(list, 200.0, 200.0), Some(3));
        assert_eq!(window_at(list, 50.0, 10.0), Some(4));
        assert_eq!(window_at(list, 500.0, 400.0), Some(4));
        // Right/bottom edges are outside, as in CGRectContainsPoint.
        assert_eq!(window_at(list, 1440.0, 10.0), None);
        assert_eq!(window_at(list, -1.0, 10.0), None);
    }

    #[test]
    fn windows_on_a_display_left_of_main_have_negative_coordinates() {
        let list = [window(7, 0, 1.0, -1920.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(window_at(list, -10.0, 500.0), Some(7));
    }
}
