//! macOS key events → DOM `KeyboardEvent.code`.
//!
//! The key-code table itself is the shared generated one
//! ([`platform_api::keymap`], from Chromium's `dom_code_data.inc`); this module
//! adds only what is macOS-specific about reading key events: modifier keys
//! arrive as `flagsChanged`, whose press/release must be read from the
//! side-specific modifier bits, and Caps Lock reports the lock toggling.
//!
//! ## References
//! * `NSEvent.keyCode`, `NSEvent.modifierFlags`:
//!   <https://developer.apple.com/documentation/appkit/nsevent>
//! * `HIToolbox/Events.h` (Carbon), "Virtual keycodes" — the `kVK_*` values.
//! * `IOKit/hidsystem/IOLLEvent.h` — the `NX_DEVICE*KEYMASK` bits.

use platform_api::keymap::{dom_for, KeyId};

/// DOM code for a macOS virtual key code (`NSEvent.keyCode`), or `None` for
/// keys the protocol does not carry (Fn, media keys, unassigned codes).
pub fn dom_code(key_code: u16) -> Option<&'static str> {
    KeyId::from_mac(key_code).map(dom_for)
}

/// How a `flagsChanged` event for `key_code` maps to a press or release.
///
/// AppKit reports modifier keys only through `flagsChanged`, with the new
/// modifier state in `modifierFlags`. The device-independent bits (Shift,
/// Control…) cannot tell left from right, so this uses the device-dependent
/// bits in the low word (IOKit `NX_DEVICE*KEYMASK`, `IOLLEvent.h`): with both
/// Shift keys held, releasing one still leaves the shared Shift flag set.
pub fn modifier_is_down(key_code: u16, modifier_flags: usize) -> Option<bool> {
    // IOKit/hidsystem/IOLLEvent.h
    const NX_DEVICELCTLKEYMASK: usize = 0x0000_0001;
    const NX_DEVICELSHIFTKEYMASK: usize = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: usize = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: usize = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: usize = 0x0000_0010;
    const NX_DEVICELALTKEYMASK: usize = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: usize = 0x0000_0040;
    const NX_DEVICERCTLKEYMASK: usize = 0x0000_2000;
    let mask = match key_code {
        0x3B => NX_DEVICELCTLKEYMASK,
        0x3E => NX_DEVICERCTLKEYMASK,
        0x38 => NX_DEVICELSHIFTKEYMASK,
        0x3C => NX_DEVICERSHIFTKEYMASK,
        0x37 => NX_DEVICELCMDKEYMASK,
        0x36 => NX_DEVICERCMDKEYMASK,
        0x3A => NX_DEVICELALTKEYMASK,
        0x3D => NX_DEVICERALTKEYMASK,
        _ => return None,
    };
    Some(modifier_flags & mask != 0)
}

/// `kVK_CapsLock`. A `flagsChanged` for it reports the **lock** toggling, not
/// the key going down and up, so the surface forwards each one as a tap.
pub const CAPS_LOCK: u16 = 0x39;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_the_surface_relies_on_map_through_the_generated_table() {
        assert_eq!(dom_code(0x00), Some("KeyA"));
        assert_eq!(dom_code(0x12), Some("Digit1"));
        assert_eq!(dom_code(0x24), Some("Enter"));
        assert_eq!(dom_code(0x33), Some("Backspace"));
        assert_eq!(dom_code(0x7E), Some("ArrowUp"));
        assert_eq!(dom_code(CAPS_LOCK), Some("CapsLock"));
        // Every modifier `modifier_is_down` knows must have a DOM code, or its
        // presses would be dropped.
        for (key_code, dom) in [
            (0x3B, "ControlLeft"),
            (0x3E, "ControlRight"),
            (0x38, "ShiftLeft"),
            (0x3C, "ShiftRight"),
            (0x37, "MetaLeft"),
            (0x36, "MetaRight"),
            (0x3A, "AltLeft"),
            (0x3D, "AltRight"),
        ] {
            assert_eq!(dom_code(key_code), Some(dom), "{key_code:#04x}");
            assert!(modifier_is_down(key_code, 0).is_some());
        }
    }

    #[test]
    fn fn_and_unassigned_codes_are_dropped() {
        assert_eq!(dom_code(0x3F), None); // kVK_Function
        assert_eq!(dom_code(u16::MAX), None);
    }

    #[test]
    fn modifiers_use_the_side_specific_bits() {
        // Both Shift keys down (0x02 | 0x04), then the right one released.
        assert_eq!(modifier_is_down(0x38, 0x0002_0006), Some(true));
        assert_eq!(modifier_is_down(0x3C, 0x0002_0002), Some(false));
        assert_eq!(modifier_is_down(0x38, 0x0002_0002), Some(true));
        assert_eq!(modifier_is_down(0x37, 0x0010_0008), Some(true));
        assert_eq!(modifier_is_down(0x37, 0x0000_0000), Some(false));
        assert_eq!(modifier_is_down(0x3E, 0x0004_2000), Some(true));
        assert_eq!(modifier_is_down(0x00, 0xFFFF_FFFF), None);
        assert_eq!(modifier_is_down(CAPS_LOCK, 0x0001_0000), None);
    }
}
