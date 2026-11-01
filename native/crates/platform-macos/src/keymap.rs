//! macOS key events ↔ DOM `KeyboardEvent.code`.
//!
//! The key-code table itself is the shared generated one
//! ([`platform_api::keymap`], from Chromium's `dom_code_data.inc`); this module
//! adds only what is macOS-specific about key events, read by the surface and
//! written by the injector: modifier keys arrive as `flagsChanged`, whose
//! press/release lives in the side-specific modifier bits, and Caps Lock
//! reports the lock toggling.
//!
//! ## References
//! * `NSEvent.keyCode`, `NSEvent.modifierFlags`:
//!   <https://developer.apple.com/documentation/appkit/nsevent>
//! * `CGEventFlags`: <https://developer.apple.com/documentation/coregraphics/cgeventflags>
//! * `HIToolbox/Events.h` (Carbon), "Virtual keycodes" — the `kVK_*` values.
//! * `IOKit/hidsystem/IOLLEvent.h` — the `NX_DEVICE*KEYMASK` bits.

use platform_api::keymap::{dom_for, KeyId};

/// DOM code for a macOS virtual key code (`NSEvent.keyCode`), or `None` for
/// keys the protocol does not carry (Fn, media keys, unassigned codes).
pub fn dom_code(key_code: u16) -> Option<&'static str> {
    KeyId::from_mac(key_code).map(dom_for)
}

/// One modifier key, as macOS describes it in an event's modifier flags.
///
/// `flag` is the device-independent bit set while either side is held; it is
/// the same bit in `NSEvent.modifierFlags` and in `CGEventFlags`
/// (`kCGEventFlagMask*`). `device_bit` is the side-specific bit in the low
/// word (IOKit `NX_DEVICE*KEYMASK`, `IOLLEvent.h`), the only way to tell left
/// from right: with both Shift keys held, releasing one still leaves the
/// shared Shift flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Modifier {
    /// `kVK_*` code of the key (`HIToolbox/Events.h`).
    pub key_code: u16,
    pub flag: u64,
    pub device_bit: u64,
}

// CGEventTypes.h `kCGEventFlagMask*` (= NSEvent.h `NSEventModifierFlag*`).
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

/// `kCGEventFlagMaskAlphaShift` (= `NSEventModifierFlagCapsLock`): the Caps
/// Lock is on.
pub(crate) const FLAG_CAPS_LOCK: u64 = 0x0001_0000;

/// The eight modifier keys. Their `kVK_*` codes are pinned to the generated
/// table by `keys_the_surface_relies_on_map_through_the_generated_table`;
/// the device bits are `NX_DEVICE{L,R}{CTL,SHIFT,CMD,ALT}KEYMASK`.
pub(crate) const MODIFIERS: [Modifier; 8] = [
    Modifier {
        key_code: 0x3B,
        flag: FLAG_CONTROL,
        device_bit: 0x0000_0001,
    },
    Modifier {
        key_code: 0x3E,
        flag: FLAG_CONTROL,
        device_bit: 0x0000_2000,
    },
    Modifier {
        key_code: 0x38,
        flag: FLAG_SHIFT,
        device_bit: 0x0000_0002,
    },
    Modifier {
        key_code: 0x3C,
        flag: FLAG_SHIFT,
        device_bit: 0x0000_0004,
    },
    Modifier {
        key_code: 0x37,
        flag: FLAG_COMMAND,
        device_bit: 0x0000_0008,
    },
    Modifier {
        key_code: 0x36,
        flag: FLAG_COMMAND,
        device_bit: 0x0000_0010,
    },
    Modifier {
        key_code: 0x3A,
        flag: FLAG_OPTION,
        device_bit: 0x0000_0020,
    },
    Modifier {
        key_code: 0x3D,
        flag: FLAG_OPTION,
        device_bit: 0x0000_0040,
    },
];

/// The modifier `key_code` names, if it is one.
pub(crate) fn modifier(key_code: u16) -> Option<Modifier> {
    MODIFIERS.into_iter().find(|m| m.key_code == key_code)
}

/// How a `flagsChanged` event for `key_code` maps to a press or release.
///
/// AppKit reports modifier keys only through `flagsChanged`, with the new
/// modifier state in `modifierFlags`; the side-specific
/// [`Modifier::device_bit`] says whether this key is now down.
pub fn modifier_is_down(key_code: u16, modifier_flags: usize) -> Option<bool> {
    modifier(key_code).map(|m| modifier_flags as u64 & m.device_bit != 0)
}

/// `kVK_CapsLock`. A `flagsChanged` for it reports the **lock** toggling, not
/// the key going down and up, so the surface forwards each one as a tap and
/// the injector turns each press back into one toggle.
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
