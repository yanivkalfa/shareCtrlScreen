//! Keyboard identity for the spike: GDK hardware keycode → evdev → DOM code.
//!
//! The full table is task D3 (generated from Chromium's `dom_code_data.inc`,
//! `platform-api/src/keymap.rs`). Until it lands, the spike maps only the main
//! block, where evdev codes are numerically equal to Set-1 make codes (plan 04
//! §6), through the shipped `input::scancode` table; every other key is dropped,
//! which is the contract's rule for unmapped keys.
//!
//! ## References
//! * Linux `input-event-codes.h` (KEY_ESC = 1 … KEY_KPDOT = 83, KEY_102ND = 86,
//!   KEY_F11 = 87, KEY_F12 = 88): <https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h>
//! * XKB keycodes are evdev + 8 (`xkeyboard-config` evdev keycodes; GDK on
//!   Wayland reports the XKB keycode as `hardware_keycode`):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c>
//! * Chromium `dom_code_data.inc` (the D3 source): <https://chromium.googlesource.com/chromium/src/+/lkgr/ui/events/keycodes/dom/dom_code_data.inc>

/// XKB keycode offset over evdev.
const XKB_EVDEV_OFFSET: u16 = 8;

/// GDK `hardware_keycode` (XKB) → evdev code.
pub fn evdev_from_gdk(hardware_keycode: u16) -> Option<u16> {
    hardware_keycode.checked_sub(XKB_EVDEV_OFFSET)
}

/// evdev → DOM `KeyboardEvent.code`, main block only (see the module doc).
pub fn dom_code(evdev: u16) -> Option<&'static str> {
    match evdev {
        // Set-1 0x01..=0x53 and 0x56..=0x58 are non-extended and equal to evdev
        // (KEY_ESC..KEY_KPDOT, KEY_102ND, KEY_F11, KEY_F12). 84/85 are unused /
        // KEY_ZENKAKUHANKAKU, which Set-1 encodes differently.
        1..=83 | 86..=88 => input::scancode::code_for(evdev, false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_maps_through_set1() {
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
    fn extended_keys_are_dropped_not_mismapped() {
        // KEY_UP = 103 would be Set-1 0x67 (F16-ish) if mapped naively.
        assert_eq!(dom_code(103), None);
        assert_eq!(dom_code(97), None, "KEY_RIGHTCTRL is E0 1D in Set-1");
        assert_eq!(evdev_from_gdk(3), None);
    }
}
