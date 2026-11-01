//! DOM `KeyboardEvent.code` ⇄ Windows Set-1 scan code, backed by the one
//! generated keymap every OS shares ([`platform_api::keymap`]).
//!
//! Injection sends **scan codes, not virtual keys**: the bytes are identical
//! regardless of keyboard language, which is what makes remote typing
//! layout-independent. The host's own layout turns the scan code into the
//! right character. Extended keys (the `E0`-prefixed ones) carry
//! `KEYEVENTF_EXTENDEDKEY` on the way out and arrive with bit 24 of `lParam`
//! set on the way in, so `(make code, extended)` is the whole identity of a
//! key and the two directions use the same [`Set1`] pair.
//!
//! This module holds no table of its own (a second copy would drift from the
//! keymap the macOS and Linux backends use); it only adapts the generated one
//! to the string/number shapes the injector and the viewer window use.
//!
//! `MapVirtualKeyW` is deliberately not used: it depends on the active
//! keyboard layout and cannot tell NumLock from Pause without the extended
//! bit, whereas the table is fixed and layout-free.
//!
//! ## NumLock and Pause
//!
//! NumLock is the extended scan code `E0 45`; plain `45` is Pause (whose full
//! sequence is `E1 1D 45`, and Windows reports its make code as `45` without
//! the extended bit). Chromium records NumLock as `0xE045` for exactly this
//! reason, and Microsoft documents the same split (see the references). The
//! table this module replaced sent NumLock as plain `45`, which Windows reads
//! as Pause; the generated table sends it extended. On the viewer side the
//! same rule means a real Pause (`45`, not extended) is dropped, as Pause is
//! unsupported, instead of being forwarded as NumLock.
//!
//! ## References
//!
//! * Microsoft, `SendInput` and `KEYBDINPUT` (`KEYEVENTF_SCANCODE`,
//!   `KEYEVENTF_EXTENDEDKEY`, `KEYEVENTF_KEYUP`):
//!   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-sendinput>,
//!   <https://learn.microsoft.com/windows/win32/api/winuser/ns-winuser-keybdinput>
//! * Microsoft, Keyboard Input / `WM_KEYDOWN` (bits 16-23 scan code, bit 24
//!   extended-key flag):
//!   <https://learn.microsoft.com/windows/win32/inputdev/wm-keydown>
//! * Microsoft, "About Keyboard Input", Scan Codes table and Extended-Key Flag
//!   (NumLock `0xE045` and Pause `0x0045` in legacy messages; "the Num Lock key
//!   has a separate scan code from the Pause key (which uses the same scan code
//!   without the extended-key flag)"):
//!   <https://learn.microsoft.com/windows/win32/inputdev/about-keyboard-input#scan-codes>
//! * Microsoft, `keybd_event` (its NumLock sample sends scan `0x45` with
//!   `KEYEVENTF_EXTENDEDKEY`):
//!   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-keybd_event>
//! * Microsoft, `MapVirtualKeyW` (`MAPVK_VSC_TO_VK_EX`, layout dependence):
//!   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-mapvirtualkeyw>
//! * Chromium `ui/events/keycodes/dom/dom_code_data.inc` (the generated
//!   table's source; NumLock `0xe045`, Pause `0x0045`).

use platform_api::keymap::{dom_for, from_dom, KeyId};

pub use platform_api::keymap::Set1;

/// Resolve a DOM code to its scan code, or `None` for deliberately
/// unsupported keys (Pause, media keys, Fn) and unknown codes.
pub fn lookup(code: &str) -> Option<Set1> {
    from_dom(code)?.to_set1()
}

/// Reverse map: the scan code and extended flag of a captured `WM_KEYDOWN` /
/// `WM_KEYUP` back to the layout-independent DOM code the host re-injects.
/// `None` for keys the keymap does not carry.
pub fn code_for(sc: u16, ext: bool) -> Option<&'static str> {
    // The scan code field of `lParam` is eight bits wide, so anything larger
    // did not come from a keyboard message and names no key.
    let code = u8::try_from(sc).ok()?;
    KeyId::from_set1(Set1 {
        code,
        extended: ext,
    })
    .map(dom_for)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written table this module used to hold, captured verbatim as
    /// `(DOM code, Set-1 make code, extended)`. The hand table is gone; this
    /// fixture is what keeps the generated one honest about every key it handled.
    const OLD_TABLE: &[(&str, u16, bool)] = &[
        ("Escape", 0x01, false),
        ("Digit1", 0x02, false),
        ("Digit2", 0x03, false),
        ("Digit3", 0x04, false),
        ("Digit4", 0x05, false),
        ("Digit5", 0x06, false),
        ("Digit6", 0x07, false),
        ("Digit7", 0x08, false),
        ("Digit8", 0x09, false),
        ("Digit9", 0x0a, false),
        ("Digit0", 0x0b, false),
        ("Minus", 0x0c, false),
        ("Equal", 0x0d, false),
        ("Backspace", 0x0e, false),
        ("Tab", 0x0f, false),
        ("KeyQ", 0x10, false),
        ("KeyW", 0x11, false),
        ("KeyE", 0x12, false),
        ("KeyR", 0x13, false),
        ("KeyT", 0x14, false),
        ("KeyY", 0x15, false),
        ("KeyU", 0x16, false),
        ("KeyI", 0x17, false),
        ("KeyO", 0x18, false),
        ("KeyP", 0x19, false),
        ("BracketLeft", 0x1a, false),
        ("BracketRight", 0x1b, false),
        ("Enter", 0x1c, false),
        ("ControlLeft", 0x1d, false),
        ("KeyA", 0x1e, false),
        ("KeyS", 0x1f, false),
        ("KeyD", 0x20, false),
        ("KeyF", 0x21, false),
        ("KeyG", 0x22, false),
        ("KeyH", 0x23, false),
        ("KeyJ", 0x24, false),
        ("KeyK", 0x25, false),
        ("KeyL", 0x26, false),
        ("Semicolon", 0x27, false),
        ("Quote", 0x28, false),
        ("Backquote", 0x29, false),
        ("ShiftLeft", 0x2a, false),
        ("Backslash", 0x2b, false),
        ("KeyZ", 0x2c, false),
        ("KeyX", 0x2d, false),
        ("KeyC", 0x2e, false),
        ("KeyV", 0x2f, false),
        ("KeyB", 0x30, false),
        ("KeyN", 0x31, false),
        ("KeyM", 0x32, false),
        ("Comma", 0x33, false),
        ("Period", 0x34, false),
        ("Slash", 0x35, false),
        ("ShiftRight", 0x36, false),
        ("NumpadMultiply", 0x37, false),
        ("AltLeft", 0x38, false),
        ("Space", 0x39, false),
        ("CapsLock", 0x3a, false),
        ("F1", 0x3b, false),
        ("F2", 0x3c, false),
        ("F3", 0x3d, false),
        ("F4", 0x3e, false),
        ("F5", 0x3f, false),
        ("F6", 0x40, false),
        ("F7", 0x41, false),
        ("F8", 0x42, false),
        ("F9", 0x43, false),
        ("F10", 0x44, false),
        ("NumLock", 0x45, false),
        ("ScrollLock", 0x46, false),
        ("Numpad7", 0x47, false),
        ("Numpad8", 0x48, false),
        ("Numpad9", 0x49, false),
        ("NumpadSubtract", 0x4a, false),
        ("Numpad4", 0x4b, false),
        ("Numpad5", 0x4c, false),
        ("Numpad6", 0x4d, false),
        ("NumpadAdd", 0x4e, false),
        ("Numpad1", 0x4f, false),
        ("Numpad2", 0x50, false),
        ("Numpad3", 0x51, false),
        ("Numpad0", 0x52, false),
        ("NumpadDecimal", 0x53, false),
        ("IntlBackslash", 0x56, false),
        ("F11", 0x57, false),
        ("F12", 0x58, false),
        ("NumpadEnter", 0x1c, true),
        ("ControlRight", 0x1d, true),
        ("NumpadDivide", 0x35, true),
        ("AltRight", 0x38, true),
        ("Home", 0x47, true),
        ("ArrowUp", 0x48, true),
        ("PageUp", 0x49, true),
        ("ArrowLeft", 0x4b, true),
        ("ArrowRight", 0x4d, true),
        ("End", 0x4f, true),
        ("ArrowDown", 0x50, true),
        ("PageDown", 0x51, true),
        ("Insert", 0x52, true),
        ("Delete", 0x53, true),
        ("MetaLeft", 0x5b, true),
        ("MetaRight", 0x5c, true),
        ("ContextMenu", 0x5d, true),
        ("PrintScreen", 0x37, true),
    ];

    /// Every intentional difference from the old table, with the reason.
    /// Anything not listed here must be identical.
    const INTENTIONAL: &[(&str, (u16, bool))] = &[
        // Plain 0x45 is Pause to Windows; NumLock is E0 45 (see the module docs).
        ("NumLock", (0x45, true)),
    ];

    #[test]
    fn generated_table_agrees_with_every_old_entry() {
        assert_eq!(OLD_TABLE.len(), 104);
        for &(code, sc, ext) in OLD_TABLE {
            let expected = INTENTIONAL
                .iter()
                .find(|(c, _)| *c == code)
                .map_or((sc, ext), |&(_, new)| new);
            let hit = lookup(code).unwrap_or_else(|| panic!("{code} lost from the keymap"));
            assert_eq!((u16::from(hit.code), hit.extended), expected, "{code}");
        }
    }

    #[test]
    fn every_old_entry_round_trips_through_the_scan_code() {
        for &(code, _, _) in OLD_TABLE {
            let hit = lookup(code).unwrap();
            assert_eq!(
                code_for(u16::from(hit.code), hit.extended),
                Some(code),
                "{code}"
            );
        }
    }

    #[test]
    fn every_scan_code_the_keymap_names_round_trips() {
        // The old fixture only covers 104 keys; the generated table has more
        // (numpad extras, international keys). All of them must round-trip.
        for sc in 0..=0xffu16 {
            for ext in [false, true] {
                if let Some(code) = code_for(sc, ext) {
                    let hit = lookup(code).unwrap();
                    assert_eq!((u16::from(hit.code), hit.extended), (sc, ext), "{code}");
                }
            }
        }
    }

    #[test]
    fn enter_and_numpad_enter_differ_only_by_the_extended_flag() {
        assert_eq!(code_for(0x1c, false), Some("Enter"));
        assert_eq!(code_for(0x1c, true), Some("NumpadEnter"));
    }

    #[test]
    fn numlock_is_extended_and_plain_45_is_not_forwarded() {
        assert_eq!(code_for(0x45, true), Some("NumLock"));
        // Pause reports 0x45 without the extended bit and is unsupported.
        assert_eq!(code_for(0x45, false), None);
    }

    #[test]
    fn unsupported_and_unknown_codes_have_no_scan_code() {
        for code in ["Pause", "MediaPlayPause", "AudioVolumeUp", "Fn", "Nonsense"] {
            assert_eq!(lookup(code), None, "{code}");
        }
        assert_eq!(code_for(0x100, false), None);
    }
}
