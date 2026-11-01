//! `InputInjector` on macOS: the viewer's input posted as CoreGraphics events
//! (plan task M5, `plans/01-MACOS-FINDINGS.md` §5).
//!
//! Every event is created from the injector's own event source, tagged with
//! [`INJECT_SENTINEL`] in `kCGEventSourceUserData`, given the modifier flags
//! the injector tracks (`inject_state.rs` explains why each is needed) and
//! posted to `kCGHIDEventTap`, where it enters the system as if it came from
//! the hardware: the cursor moves, the key window receives the keys, and
//! the system's own shortcuts (Cmd+Tab, Mission Control) work on the host.
//!
//! ## Permission
//!
//! Posting events needs the Accessibility grant (`permissions.rs`). Without
//! it macOS discards every posted event **silently**, so [`CgInjector::new`]
//! refuses with `InputError::PermissionDenied` and the engine runs the
//! session view-only and tells the user why, instead of a session in which
//! nothing the remote user does has any effect. A grant revoked mid-session
//! is noticed within [`TRUST_RECHECK`] and logged with the same advice; the
//! trait has no channel to report it to the UI.
//!
//! ## References
//! * `CGEventPost`, `CGEventTapLocation.cghidEventTap`:
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/post(tap:)>
//! * `CGEventCreateMouseEvent`:
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/init(mouseeventsource:mousetype:mousecursorposition:mousebutton:)>
//! * `CGEventCreateKeyboardEvent` ("all keystrokes needed to generate a
//!   character must be entered, including SHIFT, CONTROL, OPTION, and
//!   COMMAND"):
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/init(keyboardeventsource:virtualkey:keydown:)>
//! * `CGEventCreateScrollWheelEvent2`, `CGScrollEventUnit.pixel`:
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/init(scrollwheelevent2source:units:wheelcount:wheel1:wheel2:wheel3:)>
//! * `CGEventSetFlags`: <https://developer.apple.com/documentation/coregraphics/cgevent/flags>
//! * `CGEventSetIntegerValueField`, `kCGEventSourceUserData`,
//!   `kCGMouseEventClickState`, `kCGKeyboardEventAutorepeat`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventfield>
//! * `CGEventSourceCreate`, `CGEventSourceStateID`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventsource/init(stateid:)>
//! * `CGEventSourceSetLocalEventsSuppressionInterval` (0.25 s by default):
//!   <https://developer.apple.com/documentation/coregraphics/cgeventsource/localeventssuppressioninterval>
//! * `CGEventSourceFlagsState`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventsource/flagsstate(_:)>
//! * `CGEventCreate(NULL)` for the current cursor location:
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/init(source:)>
//! * `NSEvent.doubleClickInterval`:
//!   <https://developer.apple.com/documentation/appkit/nsevent/doubleclickinterval>
//! * Chromium remoting `remoting/host/input_injector_mac.cc` (BSD-3-Clause):
//!   the reference design for explicit flags on every event and a zero
//!   suppression interval.

use std::time::{Duration, Instant};

use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMainDisplayID, CGMouseButton, CGScrollEventUnit,
};
use platform_api::keymap::from_dom;
use platform_api::{InputError, InputInjector, OsPermission, Rect};
use protocol::{Button, InputMsg};

use crate::convert::RectF;
use crate::inject_state::{
    global_point, rect_f, CgButton, KeyEvent, KeyKind, Keyboard, Mouse, MoveKind, Wheel,
    INJECT_SENTINEL, OWNED_FLAGS, PIXELS_PER_LINE,
};
use crate::keymap::FLAG_CAPS_LOCK;
use crate::{displays, focus, permissions};

/// How often `dispatch` re-reads the Accessibility grant. A revoked grant
/// makes macOS drop our events without telling us; checking on every event
/// would add a TCC query to each of up to 60 moves a second.
const TRUST_RECHECK: Duration = Duration::from_secs(2);

const ACCESSIBILITY_ADVICE: &str = "turn on ShareCtrlScreen in System Settings > Privacy & \
    Security > Accessibility, then start the session again";

pub(crate) struct CgInjector {
    source: CFRetained<CGEventSource>,
    /// The shared display in global points; `None` = the main display, read
    /// at each use so a resolution change mid-session is followed.
    target: Option<RectF>,
    keyboard: Keyboard,
    mouse: Mouse,
    wheel: Wheel,
    trusted: bool,
    trust_checked: Instant,
    /// One warning per failure kind, not one per mouse move.
    create_failed_logged: bool,
}

impl CgInjector {
    /// Call on the thread that dispatches (the host transport thread).
    pub(crate) fn new() -> Result<Self, InputError> {
        if !permissions::is_granted(OsPermission::InputInjection) {
            return Err(InputError::PermissionDenied(format!(
                "macOS does not allow this app to control the computer; {ACCESSIBILITY_ADVICE}"
            )));
        }
        // Combined-session state, as Chromium's remoting host uses: events
        // created from it start from the session's real state, and our
        // explicit flags then replace only the bits we own.
        let source =
            CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok_or_else(|| {
                InputError::Unavailable(
                    "CoreGraphics refused to create an input event source; sign out and back \
                     in, or restart the Mac"
                        .into(),
                )
            })?;
        // By default every posted event mutes the local mouse and keyboard
        // for 0.25 s; with a stream of remote moves the person at the Mac
        // could not use their own mouse at all. AnyDesk lets both act.
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        // Also stamped on each event (see `post`); on the source it covers
        // any field CoreGraphics copies from it.
        CGEventSource::set_user_data(Some(&source), INJECT_SENTINEL);
        // Relates the pixel deltas we post to the line deltas apps that read
        // lines see (3 lines per notch, `inject_state.rs`).
        CGEventSource::set_pixels_per_line(Some(&source), PIXELS_PER_LINE);
        let caps_lock = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).0
            & FLAG_CAPS_LOCK
            != 0;
        Ok(Self {
            source,
            target: None,
            keyboard: Keyboard::new(caps_lock),
            mouse: Mouse::default(),
            wheel: Wheel::default(),
            trusted: true,
            trust_checked: Instant::now(),
            create_failed_logged: false,
        })
    }

    fn target(&self) -> RectF {
        self.target
            .unwrap_or_else(|| displays::bounds(CGMainDisplayID()))
    }

    /// Logs (once per change) when the Accessibility grant goes away or
    /// comes back, because macOS gives no other sign that it now discards
    /// everything we post.
    fn check_trust(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.trust_checked) < TRUST_RECHECK {
            return;
        }
        self.trust_checked = now;
        let trusted = permissions::is_granted(OsPermission::InputInjection);
        if trusted != self.trusted {
            if trusted {
                tracing::info!("Accessibility granted again: remote input reaches this Mac");
            } else {
                tracing::warn!(
                    "Accessibility was revoked: macOS now discards remote input; \
                     {ACCESSIBILITY_ADVICE}"
                );
            }
            self.trusted = trusted;
        }
    }

    /// Tags, flags and posts `event`. `None` (CoreGraphics could not create
    /// it, which only happens when the window server is unreachable) is
    /// logged once and dropped: there is nothing to retry with.
    fn post(&mut self, event: Option<CFRetained<CGEvent>>, flags: u64) {
        let Some(event) = event else {
            if !std::mem::replace(&mut self.create_failed_logged, true) {
                tracing::warn!("CoreGraphics could not create an input event; input is dropped");
            }
            return;
        };
        let e = Some(&*event);
        CGEvent::set_integer_value_field(e, CGEventField::EventSourceUserData, INJECT_SENTINEL);
        // Replace only the bits we track; leave the rest (NonCoalesced,
        // NumericPad, SecondaryFn…) as CoreGraphics set them for this event.
        let kept = CGEvent::flags(e).0 & !OWNED_FLAGS;
        CGEvent::set_flags(e, CGEventFlags(kept | flags));
        CGEvent::post(CGEventTapLocation::HIDEventTap, e);
    }

    fn mouse_event(
        &self,
        ty: CGEventType,
        at: (f64, f64),
        b: CgButton,
    ) -> Option<CFRetained<CGEvent>> {
        let button = match b {
            CgButton::Left => CGMouseButton::Left,
            CgButton::Right => CGMouseButton::Right,
            CgButton::Center => CGMouseButton::Center,
        };
        CGEvent::new_mouse_event(Some(&self.source), ty, CGPoint { x: at.0, y: at.1 }, button)
    }

    /// Moves the pointer to the normalized point (a drag if a button is held)
    /// and returns where it went.
    fn move_to(&mut self, nx: f64, ny: f64) -> (f64, f64) {
        let at = global_point(self.target(), nx, ny);
        let (ty, button) = match self.mouse.move_kind() {
            MoveKind::Moved => (CGEventType::MouseMoved, CgButton::Left),
            MoveKind::Dragged(CgButton::Left) => (CGEventType::LeftMouseDragged, CgButton::Left),
            MoveKind::Dragged(CgButton::Right) => (CGEventType::RightMouseDragged, CgButton::Right),
            MoveKind::Dragged(CgButton::Center) => {
                (CGEventType::OtherMouseDragged, CgButton::Center)
            }
        };
        let (dx, dy) = self.mouse.moved_to(at.0, at.1);
        let event = self.mouse_event(ty, at, button);
        if let Some(e) = event.as_deref() {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaX, dx);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaY, dy);
        }
        self.post(event, self.keyboard.flags());
        at
    }

    fn post_button(&mut self, b: CgButton, down: bool, at: (f64, f64), clicks: i64) {
        let ty = match (b, down) {
            (CgButton::Left, true) => CGEventType::LeftMouseDown,
            (CgButton::Left, false) => CGEventType::LeftMouseUp,
            (CgButton::Right, true) => CGEventType::RightMouseDown,
            (CgButton::Right, false) => CGEventType::RightMouseUp,
            (CgButton::Center, true) => CGEventType::OtherMouseDown,
            (CgButton::Center, false) => CGEventType::OtherMouseUp,
        };
        let event = self.mouse_event(ty, at, b);
        if let Some(e) = event.as_deref() {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, clicks);
        }
        self.post(event, self.keyboard.flags());
    }

    fn button(&mut self, b: Button, down: bool, nx: f64, ny: f64) {
        let b = CgButton::from(b);
        // Move first: the preceding move may have been dropped on the
        // unreliable channel, and a release must end a drag where it was
        // aimed (the move is a drag while the button is still held).
        let at = self.move_to(nx, ny);
        let clicks = if down {
            let interval = Duration::from_secs_f64(NSEvent::doubleClickInterval().max(0.0));
            self.mouse.press(b, Instant::now(), at.0, at.1, interval)
        } else {
            self.mouse.release(b)
        };
        self.post_button(b, down, at, clicks);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        let (vertical, horizontal) = self.wheel.pixels(dx, dy);
        if vertical == 0 && horizontal == 0 {
            return;
        }
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source),
            CGScrollEventUnit::Pixel,
            2,
            vertical,
            horizontal,
            0,
        );
        self.post(event, self.keyboard.flags());
    }

    fn post_key(&mut self, k: KeyEvent) {
        let down = match k.kind {
            KeyKind::Down => true,
            KeyKind::Up => false,
            KeyKind::FlagsChanged { down } => down,
        };
        let event = CGEvent::new_keyboard_event(Some(&self.source), k.key_code, down);
        if let Some(e) = event.as_deref() {
            if let KeyKind::FlagsChanged { .. } = k.kind {
                // Keyboards report modifiers and Caps Lock as flagsChanged;
                // a key-down for kVK_Command would reach apps as a
                // keystroke instead of a modifier change.
                CGEvent::set_type(Some(e), CGEventType::FlagsChanged);
            }
            if k.autorepeat {
                CGEvent::set_integer_value_field(Some(e), CGEventField::KeyboardEventAutorepeat, 1);
            }
        }
        self.post(event, k.flags);
    }

    fn key(&mut self, code: &str, down: bool) {
        // Codes macOS has no key for (PrintScreen, F21–F24…) or that the
        // protocol excludes are ignored, as on the Windows host.
        let Some(key_code) = from_dom(code).and_then(|k| k.to_mac()) else {
            return;
        };
        if let Some(event) = self.keyboard.key(key_code, down) {
            self.post_key(event);
        }
    }

    /// Where the pointer is now: button releases go there, so releasing
    /// does not warp a cursor the person at the Mac has moved since.
    fn cursor_position(&self) -> Option<(f64, f64)> {
        let probe = CGEvent::new(None)?;
        let p = CGEvent::location(Some(&probe));
        Some((p.x, p.y))
    }
}

impl InputInjector for CgInjector {
    /// On macOS `rect` is in global **points**: it comes from `DisplayInfo`,
    /// which `displays.rs` reports in points (see `inject_state.rs`).
    fn set_target_rect(&mut self, rect: Option<Rect>) {
        self.target = rect.filter(|r| r.w > 0 && r.h > 0).map(rect_f);
    }

    fn dispatch(&mut self, msg: &InputMsg) {
        self.check_trust();
        match msg {
            InputMsg::Move { x, y } => {
                self.move_to(*x, *y);
            }
            InputMsg::ButtonDown { b, x, y } => self.button(*b, true, *x, *y),
            InputMsg::ButtonUp { b, x, y } => self.button(*b, false, *x, *y),
            InputMsg::Wheel { dx, dy } => self.wheel(*dx, *dy),
            InputMsg::KeyDown { code } => self.key(code, true),
            InputMsg::KeyUp { code } => self.key(code, false),
        }
    }

    fn release_all(&mut self) {
        for event in self.keyboard.release_all() {
            self.post_key(event);
        }
        let buttons = self.mouse.release_all();
        if buttons.is_empty() {
            return;
        }
        let Some(at) = self.cursor_position().or(self.mouse.position()) else {
            return;
        };
        for b in buttons {
            self.post_button(b, false, at, 1);
        }
    }

    fn focus_window_at(&mut self, nx: f64, ny: f64) -> bool {
        let (x, y) = global_point(self.target(), nx, ny);
        focus::focus_window_at(x, y)
    }

    fn paste_chord(&self) -> &'static [&'static str] {
        &["MetaLeft", "KeyV"]
    }
}
