//! Key identity on the video surface: GDK `hardware_keycode` → DOM
//! `KeyboardEvent.code`, through the shared generated keymap.
//!
//! GDK reports the XKB keycode on both backends: on X11 it is the X keycode,
//! on Wayland the `wl_keyboard.key` evdev code plus 8 (GDK adds the offset so
//! that XKB keymaps apply). With the evdev XKB rules every current X server
//! and compositor uses, the XKB keycode is therefore evdev + 8, and the
//! physical key is found without looking at the layout — which is exactly the
//! layout independence the protocol's DOM codes want.
//!
//! ## References
//! * Linux `include/uapi/linux/input-event-codes.h` (evdev `KEY_*`).
//! * xkeyboard-config `keycodes/evdev` (`<AE01> = 10` for `KEY_1 = 2`, i.e.
//!   evdev + 8).
//! * GDK 3 Wayland keyboard: `deliver_key_event` sets `hardware_keycode =
//!   key + 8` — <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c>
//! * `platform_api::keymap` (generated from Chromium `dom_code_data.inc`).

use platform_api::keymap::{dom_for, KeyId};

/// XKB keycode offset over evdev.
const XKB_EVDEV_OFFSET: u16 = 8;

/// DOM code for a GDK `hardware_keycode`, or `None` for keys the protocol
/// does not carry (they are dropped, per the `VideoInput::Key` contract).
pub(crate) fn dom_code(hardware_keycode: u16) -> Option<&'static str> {
    let evdev = hardware_keycode.checked_sub(XKB_EVDEV_OFFSET)?;
    KeyId::from_evdev(evdev).map(dom_for)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_and_extended_keys_map_by_position() {
        // KEY_A = 30, KEY_ESC = 1, KEY_LEFTCTRL = 29.
        assert_eq!(dom_code(38), Some("KeyA"));
        assert_eq!(dom_code(9), Some("Escape"));
        assert_eq!(dom_code(37), Some("ControlLeft"));
        // The keys the spike's Set-1 shortcut could not map: KEY_UP = 103,
        // KEY_RIGHTCTRL = 97, KEY_RIGHTALT (AltGr) = 100, KEY_LEFTMETA = 125.
        assert_eq!(dom_code(111), Some("ArrowUp"));
        assert_eq!(dom_code(105), Some("ControlRight"));
        assert_eq!(dom_code(108), Some("AltRight"));
        assert_eq!(dom_code(133), Some("MetaLeft"));
    }

    #[test]
    fn codes_below_the_offset_or_unknown_are_dropped() {
        assert_eq!(dom_code(0), None);
        assert_eq!(dom_code(7), None);
        // KEY_RESERVED (0) + 8.
        assert_eq!(dom_code(8), None);
        assert_eq!(dom_code(u16::MAX), None);
    }
}
