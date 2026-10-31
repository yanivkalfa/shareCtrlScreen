//! Input injection (host) and OS-shortcut capture (viewer). Contract derived
//! from `input::Injector` and `input::keyhook`.

use std::sync::atomic::AtomicBool;

use protocol::InputMsg;

use crate::frame::Rect;
use crate::surface::VideoSurface;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// macOS Accessibility not granted, Wayland portal refused, no uinput access.
    #[error("input injection permission denied: {0}")]
    PermissionDenied(String),
    #[error("input injection unavailable: {0}")]
    Unavailable(String),
}

/// Injects the viewer's input into the host desktop.
///
/// Invariants:
/// * Coordinates in [`InputMsg`] are normalized over the captured display; the
///   backend maps them into [`set_target_rect`](Self::set_target_rect) (or the
///   primary display when `None`).
/// * A button press first moves to its coordinates (the preceding move may have
///   been dropped on the unreliable channel).
/// * Track every held key/button; [`release_all`](Self::release_all) emits real
///   release events for them (called on control revocation and session end).
/// * Tag injected events so the local [`ShortcutHook`] ignores them.
/// * Backends that must re-attach to the current input desktop before injecting
///   (Windows secure desktop) do it inside `dispatch`.
pub trait InputInjector {
    /// Display rectangle (virtual-desktop pixels) that `[0,1]` maps onto.
    fn set_target_rect(&mut self, rect: Option<Rect>);
    fn dispatch(&mut self, msg: &InputMsg);
    fn release_all(&mut self);
    /// Bring the top-level window under the normalized point to the foreground
    /// **without clicking** (a synthetic click could press a button there).
    /// `false` when impossible (no window, or the OS forbids it — Wayland).
    fn focus_window_at(&mut self, nx: f64, ny: f64) -> bool;
    /// DOM codes of the OS paste chord, pressed in order and released in reverse.
    fn paste_chord(&self) -> &'static [&'static str] {
        &["ControlLeft", "KeyV"]
    }
}

/// Receives `(dom_code, is_down)` for each captured shortcut key. Called on the
/// hook thread, possibly inside an OS callback with a hard time limit — must not
/// block (Windows removes low-level hooks that take > ~300 ms).
pub type KeyForwarder = Box<dyn Fn(&str, bool) + Send>;

/// Viewer-side capture of OS-reserved shortcuts (Win/Super/Cmd, Alt+Tab,
/// Cmd+Tab) while the session window is focused, so they act on the host.
///
/// Created, installed, run and uninstalled on one dedicated thread.
/// Invariants: never swallow keys when the session window is not focused;
/// once a key-down was forwarded, forward its key-up unconditionally; ignore
/// events produced by our own injector; never block the OS input path.
pub trait ShortcutHook {
    /// `forward(dom_code, is_down)` must not block. Returns `false` when this OS
    /// / session cannot capture shortcuts (the UI then shows the hint).
    fn install(&mut self, surface: &dyn VideoSurface, forward: KeyForwarder) -> bool;
    /// Pump OS events until `stop` is set.
    fn run(&mut self, stop: &AtomicBool);
    fn uninstall(&mut self);
}
