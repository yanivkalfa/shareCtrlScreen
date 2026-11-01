//! Keymap facts the wlroots virtual keyboard needs: the default keymap text,
//! and which keys drive which modifier.
//!
//! wlroots applies a virtual keyboard's key events without updating its
//! modifier state (`update_state = false` in `wlr_virtual_keyboard_v1.c`):
//! the client must send `modifiers` itself, as a hardware keyboard's driver
//! would. That needs the keymap's modifier map. The keymap we hand over is
//! either the seat's own keymap (as serialized by xkbcommon, which writes one
//! `modifier_map` statement per key) or [`DEFAULT_KEYMAP`]; reading the
//! `xkb_keycodes` and `modifier_map` statements of that text is enough, no
//! xkbcommon needed in this process.
//!
//! ## References
//! * XKB keymap text format (xkbcommon): <https://xkbcommon.org/doc/current/keymap-text-format-v1.html>
//! * virtual-keyboard-unstable-v1 `keymap` / `modifiers`: <https://wayland.app/protocols/virtual-keyboard-unstable-v1>
//! * Real-modifier order (Shift, Lock, Control, Mod1..Mod5 = indices 0..7) is
//!   fixed by the X11 core protocol and kept by xkbcommon.

use std::collections::HashMap;

/// The keymap used when the seat has no keyboard to copy (a headless
/// compositor, a kiosk with touch only): the RMLVO default `evdev` / `pc105` /
/// `us`, which every xkeyboard-config resolves. The compositor compiles it with
/// its own xkbcommon, so `include` statements are resolved there. Its keycodes
/// are evdev + 8, matching what `KeyId::to_evdev` produces.
pub(super) const DEFAULT_KEYMAP: &str = "xkb_keymap {\n\
    xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n\
    xkb_types { include \"complete\" };\n\
    xkb_compat { include \"complete\" };\n\
    xkb_symbols { include \"pc+us+inet(evdev)\" };\n\
};\n";

/// The `modifier_map` of `pc` symbols, by evdev code, for [`DEFAULT_KEYMAP`]
/// (its includes are not expanded in our copy of the text) and for a keymap
/// whose text has no `modifier_map` statements.
const PC_MODIFIERS: &[(u16, u8)] = &[
    (42, SHIFT),   // KEY_LEFTSHIFT
    (54, SHIFT),   // KEY_RIGHTSHIFT
    (58, LOCK),    // KEY_CAPSLOCK
    (29, CONTROL), // KEY_LEFTCTRL
    (97, CONTROL), // KEY_RIGHTCTRL
    (56, MOD1),    // KEY_LEFTALT
    (100, MOD1),   // KEY_RIGHTALT (Alt_R in `us`)
    (69, MOD2),    // KEY_NUMLOCK
    (125, MOD4),   // KEY_LEFTMETA
    (126, MOD4),   // KEY_RIGHTMETA
];

const SHIFT: u8 = 0;
const LOCK: u8 = 1;
const CONTROL: u8 = 2;
const MOD1: u8 = 3;
const MOD2: u8 = 4;
const MOD4: u8 = 6;

/// Caps Lock and Num Lock latch their modifier (XKB `LockMods`) in every
/// stock layout; all other modifier keys act while held (`SetMods`).
const LOCKING_KEYS: &[u16] = &[58, 69];

/// XKB keycode = evdev code + 8 (the `evdev` keycodes file).
const XKB_KEYCODE_OFFSET: u32 = 8;

/// Modifier state of our virtual keyboard, as `zwp_virtual_keyboard_v1.modifiers`
/// wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ModState {
    pub depressed: u32,
    pub locked: u32,
}

/// Which modifier bit each key sets, and the state that results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Modifiers {
    by_evdev: HashMap<u16, u8>,
    held: HashMap<u16, u8>,
    state: ModState,
}

impl Modifiers {
    /// From a keymap text; falls back to the `pc` map when the text has no
    /// usable `modifier_map` statements.
    pub(super) fn from_keymap(text: &str) -> Self {
        let parsed = parse_modifier_map(text);
        let by_evdev = if parsed.is_empty() {
            PC_MODIFIERS.iter().copied().collect()
        } else {
            parsed
        };
        Self {
            by_evdev,
            held: HashMap::new(),
            state: ModState::default(),
        }
    }

    /// Apply one key event; `Some(new state)` when it changed.
    pub(super) fn key(&mut self, evdev: u16, down: bool) -> Option<ModState> {
        let bit = 1u32 << self.by_evdev.get(&evdev)?;
        let before = self.state;
        if down {
            self.held.insert(evdev, 0);
            if LOCKING_KEYS.contains(&evdev) {
                self.state.locked ^= bit;
            }
        } else {
            self.held.remove(&evdev);
        }
        // Recomputed from every held key, so releasing one Shift while the
        // other is still down keeps Shift active.
        self.state.depressed = self
            .held
            .keys()
            .filter_map(|k| self.by_evdev.get(k))
            .fold(0, |mask, idx| mask | 1 << idx);
        (self.state != before).then_some(self.state)
    }
}

/// Real-modifier index of an XKB modifier name.
fn modifier_index(name: &str) -> Option<u8> {
    Some(match name {
        "Shift" => 0,
        "Lock" => 1,
        "Control" => 2,
        "Mod1" => 3,
        "Mod2" => 4,
        "Mod3" => 5,
        "Mod4" => 6,
        "Mod5" => 7,
        _ => return None,
    })
}

/// evdev code → real-modifier index, from the keymap's `xkb_keycodes`
/// (`<NAME> = 38;`, `alias <A> = <B>;`) and `modifier_map Mod1 { <LALT> };`
/// statements. Keysym entries (`modifier_map Mod1 { Alt_L }`) only appear in
/// unexpanded source files, never in a compiled keymap, and are skipped.
fn parse_modifier_map(text: &str) -> HashMap<u16, u8> {
    let mut codes: HashMap<&str, u32> = HashMap::new();
    let mut aliases: Vec<(&str, &str)> = Vec::new();
    let mut maps: Vec<(u8, &str)> = Vec::new();
    // Every statement of interest fits on one line and ends in `;`; its line
    // is the last one of the text since the previous `;` (which may also hold
    // a section header such as `xkb_keycodes "evdev" {`).
    for statement in text.split(';') {
        let line = statement.rsplit('\n').next().map_or("", str::trim);
        if let Some(at) = line.find("modifier_map") {
            let rest = line[at + "modifier_map".len()..].trim();
            let Some((name, keys)) = rest.split_once('{') else {
                continue;
            };
            let Some(index) = modifier_index(name.trim()) else {
                continue;
            };
            for key in keys.trim_end_matches('}').split(',') {
                let key = key.trim();
                if key.starts_with('<') {
                    maps.push((index, key_name(key)));
                }
            }
        } else if let Some((left, right)) = line.split_once('=') {
            // A one-line block (`xkb_keycodes { <AB01> = 52; };`) has its
            // header in front of the first statement.
            let left = left.rsplit('{').next().map_or("", str::trim);
            if let Some(alias) = left.strip_prefix("alias") {
                aliases.push((key_name(alias), key_name(right)));
            } else if left.starts_with('<') {
                if let Ok(code) = right.trim().parse() {
                    codes.insert(key_name(left), code);
                }
            }
        }
    }
    for (alias, real) in aliases {
        if let Some(&code) = codes.get(real) {
            codes.entry(alias).or_insert(code);
        }
    }
    maps.into_iter()
        .filter_map(|(index, name)| {
            let xkb = *codes.get(name)?;
            let evdev = u16::try_from(xkb.checked_sub(XKB_KEYCODE_OFFSET)?).ok()?;
            Some((evdev, index))
        })
        .collect()
}

/// `<LALT>` → `LALT`.
fn key_name(s: &str) -> &str {
    s.trim().trim_start_matches('<').trim_end_matches('>')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of `xkb_keymap_get_as_string` output (trimmed).
    const SERIALIZED: &str = r#"xkb_keymap {
xkb_keycodes "evdev+aliases(qwertz)" {
	minimum = 8;
	maximum = 255;
	<ESC>                = 9;
	<LFSH>               = 50;
	<RTSH>               = 62;
	<LALT>               = 64;
	<LCTL>               = 37;
	<CAPS>               = 66;
	<RALT>               = 108;
	<LWIN>               = 133;
	<NMLK>               = 77;
	alias <LMTA>         = <LWIN>;
	indicator 1 = "Caps Lock";
};
xkb_types "complete" {
	virtual_modifiers NumLock,Alt,LevelThree;
};
xkb_symbols "pc+de+inet(evdev)" {
	key <LALT>  { [ Alt_L, Meta_L ] };
	modifier_map Shift { <LFSH> };
	modifier_map Shift { <RTSH> };
	modifier_map Lock { <CAPS> };
	modifier_map Control { <LCTL> };
	modifier_map Mod1 { <LALT> };
	modifier_map Mod2 { <NMLK> };
	modifier_map Mod4 { <LMTA> };
	modifier_map Mod5 { <RALT>, Mode_switch };
};
};
"#;

    #[test]
    fn serialized_keymap_modifiers_are_read_by_evdev_code() {
        let map = parse_modifier_map(SERIALIZED);
        assert_eq!(map.get(&42), Some(&SHIFT), "LFSH 50 - 8");
        assert_eq!(map.get(&54), Some(&SHIFT));
        assert_eq!(map.get(&58), Some(&LOCK));
        assert_eq!(map.get(&29), Some(&CONTROL));
        assert_eq!(map.get(&56), Some(&MOD1));
        assert_eq!(map.get(&69), Some(&MOD2));
        assert_eq!(map.get(&125), Some(&MOD4), "through the LMTA alias");
        assert_eq!(map.get(&100), Some(&7), "AltGr is Mod5 in German");
        assert_eq!(map.get(&1), None, "Escape is no modifier");
    }

    #[test]
    fn default_keymap_uses_the_pc_map() {
        let m = Modifiers::from_keymap(DEFAULT_KEYMAP);
        assert_eq!(m.by_evdev, PC_MODIFIERS.iter().copied().collect());
    }

    #[test]
    fn held_modifiers_set_depressed_until_the_last_is_released() {
        let mut m = Modifiers::from_keymap(DEFAULT_KEYMAP);
        assert_eq!(m.key(30, true), None, "a letter changes nothing");
        assert_eq!(m.key(42, true).map(|s| s.depressed), Some(1 << SHIFT));
        assert_eq!(m.key(54, true), None, "the second Shift adds nothing");
        assert_eq!(m.key(42, false), None, "one Shift is still down");
        assert_eq!(m.key(54, false).map(|s| s.depressed), Some(0));
        let ctrl_alt = m.key(29, true).and(m.key(56, true)).map(|s| s.depressed);
        assert_eq!(ctrl_alt, Some(1 << CONTROL | 1 << MOD1));
    }

    #[test]
    fn caps_lock_toggles_the_locked_mask() {
        let mut m = Modifiers::from_keymap(DEFAULT_KEYMAP);
        let on = m.key(58, true).map(|s| (s.depressed, s.locked));
        assert_eq!(on, Some((1 << LOCK, 1 << LOCK)));
        assert_eq!(m.key(58, false).map(|s| s.locked), Some(1 << LOCK));
        m.key(58, true);
        assert_eq!(m.key(58, false).map(|s| s.locked), Some(0));
    }

    #[test]
    fn a_keymap_without_modifier_maps_falls_back_to_pc() {
        let m = Modifiers::from_keymap("xkb_keymap { xkb_keycodes { <AB01> = 52; }; };");
        assert_eq!(m.by_evdev.get(&42), Some(&SHIFT));
    }
}
