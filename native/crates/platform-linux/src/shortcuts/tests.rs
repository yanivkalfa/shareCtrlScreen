//! Pure-logic tests for the shortcut hook (no display server needed).

use std::any::Any;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::mpsc::Sender;

use platform_api::{
    FileDrop, Presenter, RawDisplayHandle, RawWindowHandle, ShortcutHook, SurfaceError,
    SurfaceHandles, VideoInput, VideoSurface,
};
use raw_window_handle::{
    WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle, XcbWindowHandle,
    XlibDisplayHandle, XlibWindowHandle,
};

use super::wayland::{Inhibition, Verdict};
use super::{dom_code, x11_window, Backend, HeldKeys, LinuxShortcutHook};

/// A surface that only reports handles (the hook needs nothing else).
pub(super) struct TestSurface(pub Option<SurfaceHandles>);

impl VideoSurface for TestSurface {
    fn show(&self) {}
    fn hide(&self) {}
    fn fit(&self) {}
    fn client_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_input_sink(&self, _: Option<Sender<VideoInput>>) {}
    fn set_drop_sink(&self, _: Option<Sender<FileDrop>>) {}
    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Err(SurfaceError::UnsupportedHandle)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn native_handles(&self) -> Option<SurfaceHandles> {
        self.0
    }
}

// SAFETY (for the impls below): the test surface holds only plain handle
// values that no test dereferences outside the thread it made them on.
unsafe impl Send for TestSurface {}
unsafe impl Sync for TestSurface {}

pub(super) fn xcb(video: u32, parent: u32) -> SurfaceHandles {
    let window = |id| {
        RawWindowHandle::Xcb(XcbWindowHandle::new(
            NonZeroU32::new(id).unwrap_or(NonZeroU32::MIN),
        ))
    };
    SurfaceHandles {
        video: window(video),
        parent: window(parent),
        display: RawDisplayHandle::Xcb(XcbDisplayHandle::new(None, 0)),
    }
}

#[test]
fn x11_keycodes_map_through_the_shared_keymap() {
    // XKB keycode = evdev + 8: KEY_LEFTMETA 125, KEY_LEFTALT 56, KEY_TAB 15,
    // KEY_ESC 1, KEY_RIGHTMETA 126.
    assert_eq!(dom_code(133), Some("MetaLeft"));
    assert_eq!(dom_code(134), Some("MetaRight"));
    assert_eq!(dom_code(64), Some("AltLeft"));
    assert_eq!(dom_code(23), Some("Tab"));
    assert_eq!(dom_code(9), Some("Escape"));
    assert_eq!(dom_code(38), Some("KeyA"));
    // Below the XKB offset, and beyond any keycode.
    assert_eq!(dom_code(7), None);
    assert_eq!(dom_code(0), None);
    assert_eq!(dom_code(u32::MAX), None);
}

#[test]
fn held_keys_track_once_and_release_in_reverse() {
    let mut held = HeldKeys::default();
    held.press("AltLeft");
    held.press("Tab");
    held.press("Tab"); // auto-repeat
    assert!(held.contains("Tab"));
    assert!(held.release("Tab"));
    assert!(!held.release("Tab"), "a key-up is forwarded once");
    assert!(!held.release("KeyQ"), "never pressed, nothing to release");
    held.press("ShiftLeft");
    assert_eq!(held.drain(), vec!["ShiftLeft", "AltLeft"]);
    assert!(held.drain().is_empty());
}

#[test]
fn backend_follows_the_handles() {
    assert_eq!(
        Backend::of(&xcb(7, 3)),
        Ok(Backend::X11 {
            video: 7,
            parent: 3
        })
    );

    let xlib = SurfaceHandles {
        video: RawWindowHandle::Xlib(XlibWindowHandle::new(0x0040_0002)),
        parent: RawWindowHandle::Xlib(XlibWindowHandle::new(0x0040_0001)),
        display: RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0)),
    };
    assert_eq!(
        Backend::of(&xlib),
        Ok(Backend::X11 {
            video: 0x0040_0002,
            parent: 0x0040_0001
        })
    );

    let mut a = 1u8;
    let mut b = 2u8;
    let display = NonNull::from(&mut a).cast::<c_void>();
    let toplevel = NonNull::from(&mut b).cast::<c_void>();
    let mut c = 3u8;
    let video = NonNull::from(&mut c).cast::<c_void>();
    let wayland = SurfaceHandles {
        video: RawWindowHandle::Wayland(WaylandWindowHandle::new(video)),
        parent: RawWindowHandle::Wayland(WaylandWindowHandle::new(toplevel)),
        display: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
    };
    // The inhibitor goes on the parent: a sub-surface never has keyboard focus.
    assert_eq!(
        Backend::of(&wayland),
        Ok(Backend::Wayland { display, toplevel })
    );

    // X11 display with a window handle of another kind.
    let mixed = SurfaceHandles {
        parent: RawWindowHandle::Wayland(WaylandWindowHandle::new(toplevel)),
        ..xcb(7, 3)
    };
    assert!(Backend::of(&mixed).is_err());
}

#[test]
fn xlib_window_ids_are_checked() {
    assert_eq!(
        x11_window(RawWindowHandle::Xlib(XlibWindowHandle::new(0))),
        None
    );
    assert_eq!(
        x11_window(RawWindowHandle::Xlib(XlibWindowHandle::new(42))),
        Some(42)
    );
}

#[test]
fn inhibitor_refusal_needs_focus_and_no_grant() {
    let mut s = Inhibition::Pending;
    assert_eq!(
        s.verdict(true),
        Verdict::Waiting,
        "GNOME may still be asking"
    );
    s.on_inactive();
    assert_eq!(
        s.verdict(false),
        Verdict::Waiting,
        "not focused: not a refusal"
    );
    assert_eq!(s.verdict(true), Verdict::Refused);
    s.on_active();
    assert_eq!(s.verdict(true), Verdict::Capturing);
    s.on_inactive();
    // Taken back after being granted: the escape combo, not a refusal.
    assert_eq!(s, Inhibition::Inactive { ever_active: true });
    assert_eq!(s.verdict(true), Verdict::Waiting);
    s.on_inactive();
    assert_eq!(s, Inhibition::Inactive { ever_active: true });
}

#[test]
fn install_without_native_handles_is_refused() {
    let mut hook = LinuxShortcutHook::default();
    assert!(!hook.install(&TestSurface(None), Box::new(|_, _| {})));
    hook.uninstall();
}
