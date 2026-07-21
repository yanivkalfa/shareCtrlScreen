//! DOM `KeyboardEvent.code` → Set-1 scan code table (Plan 04 §8a; ported
//! **verbatim** from `app/main/input.js` §7.4 — the bytes are identical
//! regardless of keyboard language, which is exactly why scan-code injection is
//! layout-independent).

/// A resolved scan code: the Set-1 byte and whether it is an extended key
/// (prefixed `0xE0` on the wire → `KEYEVENTF_EXTENDEDKEY` for `SendInput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub sc: u16,
    pub ext: bool,
}

/// Resolve a DOM code to a scan code, or `None` for deliberately-unsupported
/// keys (Pause, media keys, Fn — §7.4). Unknown codes are silently ignored.
pub fn lookup(code: &str) -> Option<ScanCode> {
    // Non-extended (SC map in input.js).
    let sc = match code {
        "Escape" => 0x01,
        "Digit1" => 0x02,
        "Digit2" => 0x03,
        "Digit3" => 0x04,
        "Digit4" => 0x05,
        "Digit5" => 0x06,
        "Digit6" => 0x07,
        "Digit7" => 0x08,
        "Digit8" => 0x09,
        "Digit9" => 0x0a,
        "Digit0" => 0x0b,
        "Minus" => 0x0c,
        "Equal" => 0x0d,
        "Backspace" => 0x0e,
        "Tab" => 0x0f,
        "KeyQ" => 0x10,
        "KeyW" => 0x11,
        "KeyE" => 0x12,
        "KeyR" => 0x13,
        "KeyT" => 0x14,
        "KeyY" => 0x15,
        "KeyU" => 0x16,
        "KeyI" => 0x17,
        "KeyO" => 0x18,
        "KeyP" => 0x19,
        "BracketLeft" => 0x1a,
        "BracketRight" => 0x1b,
        "Enter" => 0x1c,
        "ControlLeft" => 0x1d,
        "KeyA" => 0x1e,
        "KeyS" => 0x1f,
        "KeyD" => 0x20,
        "KeyF" => 0x21,
        "KeyG" => 0x22,
        "KeyH" => 0x23,
        "KeyJ" => 0x24,
        "KeyK" => 0x25,
        "KeyL" => 0x26,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "ShiftLeft" => 0x2a,
        "Backslash" => 0x2b,
        "KeyZ" => 0x2c,
        "KeyX" => 0x2d,
        "KeyC" => 0x2e,
        "KeyV" => 0x2f,
        "KeyB" => 0x30,
        "KeyN" => 0x31,
        "KeyM" => 0x32,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "ShiftRight" => 0x36,
        "NumpadMultiply" => 0x37,
        "AltLeft" => 0x38,
        "Space" => 0x39,
        "CapsLock" => 0x3a,
        "F1" => 0x3b,
        "F2" => 0x3c,
        "F3" => 0x3d,
        "F4" => 0x3e,
        "F5" => 0x3f,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "NumLock" => 0x45,
        "ScrollLock" => 0x46,
        "Numpad7" => 0x47,
        "Numpad8" => 0x48,
        "Numpad9" => 0x49,
        "NumpadSubtract" => 0x4a,
        "Numpad4" => 0x4b,
        "Numpad5" => 0x4c,
        "Numpad6" => 0x4d,
        "NumpadAdd" => 0x4e,
        "Numpad1" => 0x4f,
        "Numpad2" => 0x50,
        "Numpad3" => 0x51,
        "Numpad0" => 0x52,
        "NumpadDecimal" => 0x53,
        "IntlBackslash" => 0x56,
        "F11" => 0x57,
        "F12" => 0x58,
        _ => 0,
    };
    if sc != 0 {
        return Some(ScanCode { sc, ext: false });
    }

    // Extended (SC_EXT map in input.js).
    let sc = match code {
        "NumpadEnter" => 0x1c,
        "ControlRight" => 0x1d,
        "NumpadDivide" => 0x35,
        "AltRight" => 0x38,
        "Home" => 0x47,
        "ArrowUp" => 0x48,
        "PageUp" => 0x49,
        "ArrowLeft" => 0x4b,
        "ArrowRight" => 0x4d,
        "End" => 0x4f,
        "ArrowDown" => 0x50,
        "PageDown" => 0x51,
        "Insert" => 0x52,
        "Delete" => 0x53,
        "MetaLeft" => 0x5b,
        "MetaRight" => 0x5c,
        "ContextMenu" => 0x5d,
        "PrintScreen" => 0x37,
        _ => 0,
    };
    if sc != 0 {
        Some(ScanCode { sc, ext: true })
    } else {
        None
    }
}

/// Reverse map: Set-1 scan code → DOM `KeyboardEvent.code`. Used by the viewer's
/// input capture (Plan 04 §7) to turn a captured Win32 scancode back into the
/// layout-independent code the host re-injects. The inverse of [`lookup`].
pub fn code_for(sc: u16, ext: bool) -> Option<&'static str> {
    // Delegate to a small table built from the same data as `lookup`, keeping a
    // single source of truth: try every code and match the scan code.
    for &code in ALL_CODES {
        if let Some(hit) = lookup(code) {
            if hit.sc == sc && hit.ext == ext {
                return Some(code);
            }
        }
    }
    None
}

/// Every DOM code we support, for the reverse lookup.
const ALL_CODES: &[&str] = &[
    "Escape",
    "Digit1",
    "Digit2",
    "Digit3",
    "Digit4",
    "Digit5",
    "Digit6",
    "Digit7",
    "Digit8",
    "Digit9",
    "Digit0",
    "Minus",
    "Equal",
    "Backspace",
    "Tab",
    "KeyQ",
    "KeyW",
    "KeyE",
    "KeyR",
    "KeyT",
    "KeyY",
    "KeyU",
    "KeyI",
    "KeyO",
    "KeyP",
    "BracketLeft",
    "BracketRight",
    "Enter",
    "ControlLeft",
    "KeyA",
    "KeyS",
    "KeyD",
    "KeyF",
    "KeyG",
    "KeyH",
    "KeyJ",
    "KeyK",
    "KeyL",
    "Semicolon",
    "Quote",
    "Backquote",
    "ShiftLeft",
    "Backslash",
    "KeyZ",
    "KeyX",
    "KeyC",
    "KeyV",
    "KeyB",
    "KeyN",
    "KeyM",
    "Comma",
    "Period",
    "Slash",
    "ShiftRight",
    "NumpadMultiply",
    "AltLeft",
    "Space",
    "CapsLock",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "NumLock",
    "ScrollLock",
    "Numpad7",
    "Numpad8",
    "Numpad9",
    "NumpadSubtract",
    "Numpad4",
    "Numpad5",
    "Numpad6",
    "NumpadAdd",
    "Numpad1",
    "Numpad2",
    "Numpad3",
    "Numpad0",
    "NumpadDecimal",
    "IntlBackslash",
    "F11",
    "F12",
    "NumpadEnter",
    "ControlRight",
    "NumpadDivide",
    "AltRight",
    "Home",
    "ArrowUp",
    "PageUp",
    "ArrowLeft",
    "ArrowRight",
    "End",
    "ArrowDown",
    "PageDown",
    "Insert",
    "Delete",
    "MetaLeft",
    "MetaRight",
    "ContextMenu",
    "PrintScreen",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_lookup_roundtrips() {
        for code in ["KeyA", "Enter", "ArrowUp", "ControlRight", "F5", "Space"] {
            let sc = lookup(code).unwrap();
            assert_eq!(code_for(sc.sc, sc.ext), Some(code));
        }
        // Non-extended 0x1c is Enter; extended 0x1c is NumpadEnter.
        assert_eq!(code_for(0x1c, false), Some("Enter"));
        assert_eq!(code_for(0x1c, true), Some("NumpadEnter"));
    }

    #[test]
    fn known_non_extended() {
        assert_eq!(
            lookup("KeyA"),
            Some(ScanCode {
                sc: 0x1e,
                ext: false
            })
        );
        assert_eq!(
            lookup("Enter"),
            Some(ScanCode {
                sc: 0x1c,
                ext: false
            })
        );
    }

    #[test]
    fn known_extended() {
        assert_eq!(
            lookup("ArrowUp"),
            Some(ScanCode {
                sc: 0x48,
                ext: true
            })
        );
        assert_eq!(
            lookup("ControlRight"),
            Some(ScanCode {
                sc: 0x1d,
                ext: true
            })
        );
    }

    #[test]
    fn unsupported_and_unknown() {
        assert_eq!(lookup("Pause"), None);
        assert_eq!(lookup("MediaPlayPause"), None);
        assert_eq!(lookup("Nonsense"), None);
    }
}
