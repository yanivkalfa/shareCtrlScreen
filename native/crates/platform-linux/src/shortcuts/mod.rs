//! Viewer-side capture of desktop shortcuts (Super, Alt+Tab, Alt+Esc …) while
//! the session window is focused, so they act on the remote machine (task L7).
//!
//! The backend follows the window handles the video surface exposes
//! ([`VideoSurface::native_handles`]), not the session type. An app forced onto
//! XWayland (`GDK_BACKEND=x11`) has X11 handles in a Wayland session; an X11
//! grab cannot take the compositor's shortcuts there, so the X11 backend
//! detects XWayland and refuses with the remedy.
//!
//! * **X11** ([`x11`]): an XInput2 keyboard grab on the video window while the
//!   session window has the input focus and the video is shown. While the grab
//!   is held the window manager sees no key at all, and the hook forwards every
//!   key itself.
//! * **Wayland** ([`wayland`]): a `zwp_keyboard_shortcuts_inhibitor_v1` on the
//!   app's toplevel. The compositor then delivers its shortcuts to us as
//!   ordinary keys, which reach GTK and the surface's own key path like any other
//!   key, so this backend never calls `forward` (doing so would send every
//!   shortcut twice).
//!
//! `install` returns `false`, with the reason in the log, whenever capture
//! cannot work here: no native handles, no XInput2, XWayland, the keyboard grabbed by
//! someone else (`AlreadyGrabbed` / `Frozen`), no inhibit manager, or the
//! compositor refusing the inhibitor. The UI then shows the "unsupported here"
//! hint.
//!
//! ## References
//!
//! * `ShortcutHook` / `SurfaceHandles` contracts: `platform-api/src/input.rs`,
//!   `platform-api/src/surface.rs`.
//! * XKB keycodes are evdev codes plus 8 (xkeyboard-config `keycodes/evdev`):
//!   <https://gitlab.freedesktop.org/xkeyboard-config/xkeyboard-config/-/blob/master/keycodes/evdev>
//! * plans/02-LINUX-FINDINGS.md §4 (what X11 and the compositors offer).

mod wayland;
mod x11;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod xvfb_tests;

use std::sync::atomic::AtomicBool;

use platform_api::keymap::{self, KeyId};
use platform_api::{
    KeyForwarder, RawDisplayHandle, RawWindowHandle, ShortcutHook, SurfaceHandles, VideoSurface,
};

/// XKB (and therefore X11) keycodes are the evdev code plus this offset.
const X11_KEYCODE_OFFSET: u16 = 8;

/// The Linux [`ShortcutHook`]. Holds nothing until `install` succeeds.
#[derive(Default)]
pub(crate) struct LinuxShortcutHook {
    active: Option<Active>,
    /// X server to grab on; `None` = `$DISPLAY`, the server GTK connected to.
    #[cfg(test)]
    x11_display: Option<String>,
    /// Treat keys from XTEST devices as real. Only the Xvfb tests need it: XTEST
    /// is the only way to type into Xvfb, and production ignores XTEST input.
    #[cfg(test)]
    accept_virtual_keyboards: bool,
}

enum Active {
    X11(x11::X11Grab),
    Wayland(wayland::Inhibitor),
}

impl LinuxShortcutHook {
    fn try_install(
        &mut self,
        surface: &dyn VideoSurface,
        forward: KeyForwarder,
    ) -> Result<Active, String> {
        let handles = surface.native_handles().ok_or(
            "the video surface does not expose its native window, so there is nothing to \
             capture shortcuts for",
        )?;
        match Backend::of(&handles)? {
            Backend::X11 { video, parent } => {
                #[cfg(test)]
                let options = x11::Options {
                    display: self.x11_display.clone(),
                    accept_virtual_keyboards: self.accept_virtual_keyboards,
                };
                #[cfg(not(test))]
                let options = x11::Options::default();
                x11::X11Grab::install(&options, video, parent, forward).map(Active::X11)
            }
            Backend::Wayland { display, toplevel } => {
                // WHY no forward: with the inhibitor active the compositor sends
                // Super / Alt+Tab to our toplevel as ordinary keys, which the
                // surface's GTK key handler already forwards.
                drop(forward);
                // SAFETY: `SurfaceHandles` are valid for the surface's lifetime
                // (platform-api contract) and the surface outlives the session
                // whose hook thread this is (the engine joins it first).
                unsafe { wayland::Inhibitor::install(display, toplevel) }.map(Active::Wayland)
            }
        }
    }
}

impl ShortcutHook for LinuxShortcutHook {
    fn install(&mut self, surface: &dyn VideoSurface, forward: KeyForwarder) -> bool {
        self.uninstall();
        match self.try_install(surface, forward) {
            Ok(active) => {
                tracing::info!(
                    "shortcut capture on ({})",
                    match active {
                        Active::X11(_) => "X11 keyboard grab while focused",
                        Active::Wayland(_) => "Wayland shortcuts inhibitor",
                    }
                );
                self.active = Some(active);
                true
            }
            Err(reason) => {
                tracing::warn!("shortcut capture is off: {reason}");
                false
            }
        }
    }

    fn run(&mut self, stop: &AtomicBool) {
        match &mut self.active {
            Some(Active::X11(grab)) => grab.run(stop),
            Some(Active::Wayland(inhibitor)) => inhibitor.run(stop),
            None => {}
        }
    }

    fn uninstall(&mut self) {
        match self.active.take() {
            Some(Active::X11(mut grab)) => grab.shutdown(),
            Some(Active::Wayland(mut inhibitor)) => inhibitor.shutdown(),
            None => {}
        }
    }
}

impl Drop for LinuxShortcutHook {
    fn drop(&mut self) {
        self.uninstall();
    }
}

/// Which backend a surface's handles call for, with the values it needs.
#[derive(Debug, PartialEq)]
enum Backend {
    X11 {
        video: u32,
        parent: u32,
    },
    Wayland {
        display: std::ptr::NonNull<std::ffi::c_void>,
        toplevel: std::ptr::NonNull<std::ffi::c_void>,
    },
}

impl Backend {
    fn of(handles: &SurfaceHandles) -> Result<Self, String> {
        match (handles.video, handles.parent, handles.display) {
            (_, RawWindowHandle::Wayland(parent), RawDisplayHandle::Wayland(display)) => {
                Ok(Backend::Wayland {
                    display: display.display,
                    toplevel: parent.surface,
                })
            }
            (video, parent, RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)) => {
                match (x11_window(video), x11_window(parent)) {
                    (Some(video), Some(parent)) => Ok(Backend::X11 { video, parent }),
                    _ => {
                        Err("the video surface reported an X11 display without X11 windows".into())
                    }
                }
            }
            (_, _, display) => Err(format!(
                "this display system is not supported for shortcut capture ({display:?})"
            )),
        }
    }
}

/// The XID inside an X11 window handle; `None` for other handle kinds and for
/// an Xlib handle whose window does not fit the protocol's 29-bit XIDs.
fn x11_window(handle: RawWindowHandle) -> Option<u32> {
    match handle {
        RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok().filter(|&w| w != 0),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

/// DOM `KeyboardEvent.code` of an X11 keycode, through the shared keymap.
/// `None` for keycodes below 8 (unused by XKB) and keys the keymap lacks; the
/// trait drops those.
fn dom_code(x11_keycode: u32) -> Option<&'static str> {
    let evdev = u16::try_from(x11_keycode)
        .ok()?
        .checked_sub(X11_KEYCODE_OFFSET)?;
    KeyId::from_evdev(evdev).map(keymap::dom_for)
}

/// Keys whose key-down was forwarded and whose key-up was not yet.
///
/// Exists for the trait's stuck-key invariant: once a key-down reached the
/// remote, its key-up must follow, even when the real release lands in another
/// window (focus moved, the video was hidden, the hook stopped).
#[derive(Debug, Default)]
struct HeldKeys(Vec<&'static str>);

impl HeldKeys {
    /// Record a forwarded key-down. Auto-repeat downs are tracked once.
    fn press(&mut self, code: &'static str) {
        if !self.0.contains(&code) {
            self.0.push(code);
        }
    }

    /// `true` when `code` was held, i.e. its key-up must be forwarded.
    fn release(&mut self, code: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|&held| held != code);
        self.0.len() != before
    }

    fn contains(&self, code: &str) -> bool {
        self.0.contains(&code)
    }

    /// Everything still held, most recent first (modifiers pressed first are
    /// released last, as a person would).
    fn drain(&mut self) -> Vec<&'static str> {
        let mut held = std::mem::take(&mut self.0);
        held.reverse();
        held
    }
}
