//! Keyboard identity for the spike: GDK hardware keycode → evdev → DOM code,
//! through the shared generated keymap (`platform-api/src/keymap.rs`, from
//! Chromium's `dom_code_data.inc`); keys it does not carry are dropped, which
//! is the contract's rule for unmapped keys.
//!
//! ## References
//! * Linux `input-event-codes.h` (KEY_ESC = 1 … KEY_KPDOT = 83, KEY_102ND = 86,
//!   KEY_F11 = 87, KEY_F12 = 88): <https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h>
//! * XKB keycodes are evdev + 8 (`xkeyboard-config` evdev keycodes; GDK on
//!   Wayland reports the XKB keycode as `hardware_keycode`):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c>
//! * Chromium `dom_code_data.inc` (the D3 source): <https://chromium.googlesource.com/chromium/src/+/lkgr/ui/events/keycodes/dom/dom_code_data.inc>

use platform_api::keymap::{dom_for, KeyId};

/// XKB keycode offset over evdev.
const XKB_EVDEV_OFFSET: u16 = 8;

/// GDK `hardware_keycode` (XKB) → evdev code.
pub fn evdev_from_gdk(hardware_keycode: u16) -> Option<u16> {
    hardware_keycode.checked_sub(XKB_EVDEV_OFFSET)
}

/// evdev → DOM `KeyboardEvent.code`.
pub fn dom_code(evdev: u16) -> Option<&'static str> {
    KeyId::from_evdev(evdev).map(dom_for)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_maps_through_the_keymap() {
        assert_eq!(evdev_from_gdk(38), Some(30));
        assert_eq!(dom_code(30), Some("KeyA"));
        assert_eq!(dom_code(1), Some("Escape"));
        assert_eq!(dom_code(28), Some("Enter"));
        assert_eq!(dom_code(29), Some("ControlLeft"));
        assert_eq!(dom_code(56), Some("AltLeft"));
        assert_eq!(dom_code(86), Some("IntlBackslash"));
        assert_eq!(dom_code(87), Some("F11"));
        assert_eq!(dom_code(88), Some("F12"));
    }

    #[test]
    fn extended_keys_map_by_evdev_not_by_set1() {
        assert_eq!(dom_code(103), Some("ArrowUp"));
        assert_eq!(dom_code(97), Some("ControlRight"));
        assert_eq!(dom_code(0), None);
        assert_eq!(evdev_from_gdk(3), None);
    }
}
