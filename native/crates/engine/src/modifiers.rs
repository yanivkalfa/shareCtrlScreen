//! Modifier translation for the host's OS (task D8), on the viewer side.
//!
//! Keys travel as physical DOM codes, and the shared keymap names the Mac
//! Command key `Meta` — the same code as the Windows key and Super. Forwarded
//! literally, a Mac viewer's Cmd+C reaches a Windows host as Win+C and Cmd+Tab
//! as Win+Tab (Task View), and a PC viewer's Ctrl+C reaches a Mac as
//! Control+C, which is not Copy there. [`KeyTranslator`] rewrites every key
//! event the viewer forwards (surface keys and captured OS shortcuts alike) so
//! the host receives the chord that means on its OS what the user meant on
//! theirs. The host stays dumb: it injects whatever codes arrive.
//!
//! ## The mapping
//!
//! * Mac viewer → Windows/Linux host: Cmd ↔ Ctrl swapped (`MetaLeft/Right` ↔
//!   `ControlLeft/Right`), so Cmd+C/V/Z/S are Ctrl+C/V/Z/S and the Mac
//!   Control key is the Win/Super key. Chords: Cmd+Tab, Cmd+Shift+Tab and
//!   Cmd+\` become Alt+Tab, Alt+Shift+Tab and Alt+\`. Option stays Alt.
//! * Windows/Linux viewer → Mac host: Ctrl ↔ Cmd swapped, so Ctrl+C is Cmd+C.
//!   Chord: Alt+Tab (+Shift) becomes Cmd+Tab (+Shift). **The Win/Super key is
//!   swapped too, to Control**: the Ctrl key already produces Cmd, so leaving
//!   Win as Meta would give two keys for Cmd and none for the Mac's Control
//!   (Control+C in Terminal, Control+arrows for Spaces). With the swap every
//!   Mac modifier stays reachable, in the same positions a Mac keyboard has.
//! * Both directions: Ctrl+Tab keeps meaning Ctrl+Tab (next tab in browsers
//!   and editors on all three OSes). The swap alone would turn it into
//!   Win+Tab / Cmd+Tab — the very shortcuts this module exists to avoid.
//! * Same-OS pairs, Linux ↔ Windows, an unknown host OS, or the setting off:
//!   identity (the keys as pressed, the behaviour before D8).
//!
//! ## State
//!
//! The translator tracks what it **sent**, not what was pressed: every held
//! physical key remembers the code the host received for it, and its release
//! always releases that code, even if the setting changed in between. A chord
//! rewrite changes a held modifier's sent code (Cmd, sent as Ctrl, becomes
//! Alt when Tab goes down), and the modifier then stays that code until it is
//! released: holding Cmd across several Tabs keeps the host's Alt+Tab switcher
//! open, exactly as holding Alt does on Windows. Two physical keys may hold the
//! same sent code (Option held while Cmd+Tab turns Cmd into Alt); the host sees
//! one press and one release for it, the release when the last of them goes up.
//! `release_all` on the host and the surface's synthesized key-ups on focus
//! loss therefore release exactly what the host holds.
//!
//! ## References
//!
//! * Microsoft, "Use keyboard, mouse, touch, and pen in Windows App"
//!   (<https://learn.microsoft.com/en-us/windows-app/input-keyboard-mouse-touch-pen>):
//!   the Mac client maps common macOS shortcuts so they work in Windows — the
//!   default-on behaviour this follows (owner rule: "feel like AnyDesk", which
//!   also translates by default).
//! * Apple, "Windows keys on a Mac keyboard"
//!   (<https://support.apple.com/guide/mac-help/cpmh0152/mac>): Command plays
//!   the role of the PC's Control in common shortcuts; Option is Alt.
//! * AutoHotkey, `#MenuMaskKey`: Windows opens the Start menu when the Win key
//!   is pressed and released with no other key in between, and a Ctrl event
//!   inside the press suppresses it. Hence rewrites press the new code
//!   **before** releasing the old one (see [`KeyTranslator::resend`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use protocol::InputMsg;

/// An operating-system family, as far as modifier keys are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OsFamily {
    Windows,
    Mac,
    Linux,
    /// A peer that did not say (an older host) or said something we do not
    /// know: keys go through untouched.
    Unknown,
}

impl OsFamily {
    /// From a [`platform_api::PlatformInfo::name`] (`"windows"`, `"macos"`,
    /// `"linux-x11"`, `"linux-wayland"`) or a connect-response `os` value
    /// (`"windows"`, `"macos"`, `"linux"`).
    pub(crate) fn from_name(name: &str) -> Self {
        match name {
            "windows" => Self::Windows,
            "macos" => Self::Mac,
            n if n == "linux" || n.starts_with("linux-") => Self::Linux,
            _ => Self::Unknown,
        }
    }

    /// The connect-response `os` value; `None` for [`OsFamily::Unknown`]
    /// (the field is then left out, which every viewer reads as unknown).
    pub(crate) fn wire_name(self) -> Option<&'static str> {
        match self {
            Self::Windows => Some("windows"),
            Self::Mac => Some("macos"),
            Self::Linux => Some("linux"),
            Self::Unknown => None,
        }
    }
}

/// Which rewrite applies to the current key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapping {
    Identity,
    /// Mac viewer, Windows or Linux host.
    MacToPc,
    /// Windows or Linux viewer, Mac host.
    PcToMac,
}

fn mapping(viewer: OsFamily, host: OsFamily, enabled: bool) -> Mapping {
    if !enabled {
        return Mapping::Identity;
    }
    match (viewer, host) {
        (OsFamily::Mac, OsFamily::Windows | OsFamily::Linux) => Mapping::MacToPc,
        (OsFamily::Windows | OsFamily::Linux, OsFamily::Mac) => Mapping::PcToMac,
        _ => Mapping::Identity,
    }
}

/// The code `code` is sent as when pressed outside a rewritten chord. Both
/// directions are the same swap: the key a user presses for Copy (Cmd on a
/// Mac, Ctrl on a PC) becomes the host's Copy key, and the other one takes its
/// place.
fn remap(m: Mapping, code: &str) -> &str {
    if m == Mapping::Identity {
        return code;
    }
    match code {
        "MetaLeft" => "ControlLeft",
        "MetaRight" => "ControlRight",
        "ControlLeft" => "MetaLeft",
        "ControlRight" => "MetaRight",
        other => other,
    }
}

/// When `key` goes down while the physical modifier `modifier` is held, the
/// code that modifier must be sent as for the chord to mean on the host what
/// it means on the viewer. `None` = no chord rule, the modifier keeps its code.
fn chord(m: Mapping, modifier: &str, key: &str) -> Option<&'static str> {
    match (m, modifier, key) {
        // The app switcher, and Cmd+` (next window of the app). WHY AltLeft for
        // both Cmd keys: on layouts with AltGr, Windows reads a right Alt as
        // AltGr (Ctrl+Alt), and AltGr+Tab does not switch apps.
        (Mapping::MacToPc, "MetaLeft" | "MetaRight", "Tab" | "Backquote") => Some("AltLeft"),
        (Mapping::PcToMac, "AltLeft" | "AltRight", "Tab") => Some("MetaLeft"),
        // Ctrl+Tab switches tabs everywhere; undo the swap for it.
        (Mapping::MacToPc | Mapping::PcToMac, "ControlLeft", "Tab") => Some("ControlLeft"),
        (Mapping::MacToPc | Mapping::PcToMac, "ControlRight", "Tab") => Some("ControlRight"),
        _ => None,
    }
}

/// One physical key the host currently holds a code for.
#[derive(Debug)]
struct Held {
    physical: String,
    sent: String,
}

/// Rewrites one session's forwarded key events for the host's OS.
///
/// One per viewer session, shared (behind a mutex) by the surface's input
/// thread and the shortcut-hook thread: both paths must go through the same
/// held-key state, and emitting inside the lock keeps the order in which the
/// host receives the rewritten events the order the translator decided them.
pub(crate) struct KeyTranslator {
    viewer: OsFamily,
    host: OsFamily,
    /// The live "Translate modifiers for the remote OS" setting. Read per key
    /// event, so a change applies to the next key; held keys are unaffected
    /// because their release uses [`Held::sent`].
    enabled: Arc<AtomicBool>,
    /// A handful of keys at most are held at once; a Vec beats a map here.
    held: Vec<Held>,
}

impl KeyTranslator {
    pub(crate) fn new(viewer: OsFamily, host: OsFamily, enabled: Arc<AtomicBool>) -> Self {
        Self {
            viewer,
            host,
            enabled,
            held: Vec::with_capacity(8),
        }
    }

    /// Translate one physical key event; `emit` receives the messages to send
    /// to the host, in order (zero, one or several).
    pub(crate) fn key(&mut self, code: &str, down: bool, emit: &mut impl FnMut(InputMsg)) {
        if down {
            self.down(code, emit);
        } else {
            self.up(code, emit);
        }
    }

    fn down(&mut self, code: &str, emit: &mut impl FnMut(InputMsg)) {
        // Auto-repeat: the key is already down on the host as some code; repeat
        // that code (never a newly mapped one: its release would not match).
        if let Some(h) = self.held.iter().find(|h| h.physical == code) {
            emit(InputMsg::KeyDown {
                code: h.sent.clone(),
            });
            return;
        }
        let m = mapping(self.viewer, self.host, self.enabled.load(Ordering::Relaxed));
        // Rewrite held modifiers first, so the host has the chord's modifier
        // down before the chord key arrives.
        for i in 0..self.held.len() {
            if let Some(to) = chord(m, &self.held[i].physical, code) {
                if self.held[i].sent != to {
                    self.resend(i, to, emit);
                }
            }
        }
        let sent = remap(m, code).to_string();
        if !self.holds(&sent, None) {
            emit(InputMsg::KeyDown { code: sent.clone() });
        }
        self.held.push(Held {
            physical: code.to_string(),
            sent,
        });
    }

    fn up(&mut self, code: &str, emit: &mut impl FnMut(InputMsg)) {
        let sent = match self.held.iter().position(|h| h.physical == code) {
            Some(i) => self.held.remove(i).sent,
            // A release we never saw pressed (pressed before the session, or a
            // hook forwarding an up unconditionally): pass it on as the current
            // mapping would have sent it, so a key the host does hold for it is
            // not left down.
            None => {
                let m = mapping(self.viewer, self.host, self.enabled.load(Ordering::Relaxed));
                remap(m, code).to_string()
            }
        };
        // Another physical key still holds this code on the host.
        if !self.holds(&sent, None) {
            emit(InputMsg::KeyUp { code: sent });
        }
    }

    /// Change the code held key `i` is sent as. Make before break: the new
    /// code goes down before the old one comes up, so the old modifier is
    /// never released alone (a lone Win press and release opens the Start
    /// menu; a Ctrl event inside it does not, see the module references).
    fn resend(&mut self, i: usize, to: &'static str, emit: &mut impl FnMut(InputMsg)) {
        if !self.holds(to, Some(i)) {
            emit(InputMsg::KeyDown { code: to.into() });
        }
        let old = std::mem::replace(&mut self.held[i].sent, to.to_string());
        if !self.holds(&old, None) {
            emit(InputMsg::KeyUp { code: old });
        }
    }

    /// Whether some held key (other than `except`) holds `code` on the host.
    fn holds(&self, code: &str, except: Option<usize>) -> bool {
        self.held
            .iter()
            .enumerate()
            .any(|(j, h)| Some(j) != except && h.sent == code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translator(viewer: OsFamily, host: OsFamily) -> (KeyTranslator, Arc<AtomicBool>) {
        let on = Arc::new(AtomicBool::new(true));
        (KeyTranslator::new(viewer, host, on.clone()), on)
    }

    /// Run `keys` ("+Code" = down, "-Code" = up) and return what the host got
    /// in the same notation.
    fn run(t: &mut KeyTranslator, keys: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for k in keys {
            let (down, code) = match k.split_at(1) {
                ("+", c) => (true, c),
                ("-", c) => (false, c),
                _ => panic!("bad test key {k}"),
            };
            t.key(code, down, &mut |m| {
                out.push(match m {
                    InputMsg::KeyDown { code } => format!("+{code}"),
                    InputMsg::KeyUp { code } => format!("-{code}"),
                    other => panic!("not a key message: {other:?}"),
                })
            });
        }
        out
    }

    fn mac_to_windows() -> KeyTranslator {
        translator(OsFamily::Mac, OsFamily::Windows).0
    }

    #[test]
    fn os_names_parse() {
        assert_eq!(OsFamily::from_name("windows"), OsFamily::Windows);
        assert_eq!(OsFamily::from_name("macos"), OsFamily::Mac);
        assert_eq!(OsFamily::from_name("linux"), OsFamily::Linux);
        assert_eq!(OsFamily::from_name("linux-x11"), OsFamily::Linux);
        assert_eq!(OsFamily::from_name("linux-wayland"), OsFamily::Linux);
        assert_eq!(OsFamily::from_name("mock"), OsFamily::Unknown);
        assert_eq!(OsFamily::from_name("linuxish"), OsFamily::Unknown);
        assert_eq!(OsFamily::from_name(""), OsFamily::Unknown);
        for os in [OsFamily::Windows, OsFamily::Mac, OsFamily::Linux] {
            let wire = os.wire_name().unwrap();
            assert_eq!(OsFamily::from_name(wire), os, "{wire} round-trips");
        }
        assert_eq!(OsFamily::Unknown.wire_name(), None);
    }

    #[test]
    fn mapping_table_covers_every_pair() {
        use OsFamily::*;
        let all = [Windows, Mac, Linux, Unknown];
        for viewer in all {
            for host in all {
                let expected = match (viewer, host) {
                    (Mac, Windows | Linux) => Mapping::MacToPc,
                    (Windows | Linux, Mac) => Mapping::PcToMac,
                    _ => Mapping::Identity,
                };
                assert_eq!(
                    mapping(viewer, host, true),
                    expected,
                    "{viewer:?} → {host:?}"
                );
                assert_eq!(
                    mapping(viewer, host, false),
                    Mapping::Identity,
                    "{viewer:?} → {host:?} with the setting off"
                );
            }
        }
    }

    #[test]
    fn remap_swaps_only_cmd_and_ctrl() {
        for m in [Mapping::MacToPc, Mapping::PcToMac] {
            assert_eq!(remap(m, "MetaLeft"), "ControlLeft");
            assert_eq!(remap(m, "MetaRight"), "ControlRight");
            assert_eq!(remap(m, "ControlLeft"), "MetaLeft");
            assert_eq!(remap(m, "ControlRight"), "MetaRight");
            for same in [
                "AltLeft",
                "AltRight",
                "ShiftLeft",
                "KeyC",
                "Tab",
                "Backquote",
            ] {
                assert_eq!(remap(m, same), same);
            }
        }
        for code in ["MetaLeft", "ControlLeft", "AltLeft", "KeyC"] {
            assert_eq!(remap(Mapping::Identity, code), code);
        }
    }

    #[test]
    fn mac_cmd_c_is_ctrl_c_on_windows() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"]),
            ["+ControlLeft", "+KeyC", "-KeyC", "-ControlLeft"]
        );
        assert_eq!(
            run(&mut t, &["+MetaRight", "+KeyV", "-KeyV", "-MetaRight"]),
            ["+ControlRight", "+KeyV", "-KeyV", "-ControlRight"]
        );
        // The Mac Control key is the Win key there; Option stays Alt.
        assert_eq!(run(&mut t, &["+ControlLeft"]), ["+MetaLeft"]);
        assert_eq!(run(&mut t, &["-ControlLeft"]), ["-MetaLeft"]);
        assert_eq!(
            run(&mut t, &["+AltLeft", "-AltLeft"]),
            ["+AltLeft", "-AltLeft"]
        );
    }

    #[test]
    fn cmd_tab_with_tab_released_first() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+Tab", "-Tab", "-MetaLeft"]),
            [
                "+ControlLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Tab",
                "-Tab",
                "-AltLeft"
            ]
        );
    }

    #[test]
    fn cmd_tab_with_cmd_released_first() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+Tab", "-MetaLeft", "-Tab"]),
            [
                "+ControlLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Tab",
                "-AltLeft",
                "-Tab"
            ]
        );
    }

    #[test]
    fn cmd_held_across_several_tabs_keeps_alt() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(
                &mut t,
                &[
                    "+MetaLeft",
                    "+Tab",
                    "-Tab",
                    "+Tab",
                    "-Tab",
                    "+Tab",
                    "-Tab",
                    "-MetaLeft"
                ]
            ),
            [
                "+ControlLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Tab",
                "-Tab",
                "+Tab",
                "-Tab",
                "+Tab",
                "-Tab",
                "-AltLeft"
            ]
        );
    }

    #[test]
    fn shift_added_mid_chord_goes_backwards() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(
                &mut t,
                &[
                    "+MetaLeft",
                    "+Tab",
                    "-Tab",
                    "+ShiftLeft",
                    "+Tab",
                    "-Tab",
                    "-ShiftLeft",
                    "-MetaLeft"
                ]
            ),
            [
                "+ControlLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Tab",
                "-Tab",
                "+ShiftLeft",
                "+Tab",
                "-Tab",
                "-ShiftLeft",
                "-AltLeft"
            ]
        );
        // Cmd+Shift+Tab from the start is Alt+Shift+Tab.
        assert_eq!(
            run(
                &mut t,
                &[
                    "+MetaLeft",
                    "+ShiftLeft",
                    "+Tab",
                    "-Tab",
                    "-ShiftLeft",
                    "-MetaLeft"
                ]
            ),
            [
                "+ControlLeft",
                "+ShiftLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Tab",
                "-Tab",
                "-ShiftLeft",
                "-AltLeft"
            ]
        );
    }

    #[test]
    fn right_cmd_tab_and_cmd_backquote_use_left_alt() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+MetaRight", "+Tab", "-Tab", "-MetaRight"]),
            [
                "+ControlRight",
                "+AltLeft",
                "-ControlRight",
                "+Tab",
                "-Tab",
                "-AltLeft"
            ]
        );
        assert_eq!(
            run(
                &mut t,
                &["+MetaLeft", "+Backquote", "-Backquote", "-MetaLeft"]
            ),
            [
                "+ControlLeft",
                "+AltLeft",
                "-ControlLeft",
                "+Backquote",
                "-Backquote",
                "-AltLeft"
            ]
        );
    }

    #[test]
    fn tab_before_cmd_is_not_a_chord() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+Tab", "+MetaLeft", "-Tab", "-MetaLeft"]),
            ["+Tab", "+ControlLeft", "-Tab", "-ControlLeft"]
        );
    }

    #[test]
    fn ctrl_tab_stays_ctrl_tab_both_ways() {
        // Mac Control is sent as Win; Tab turns it back into Ctrl, pressing Ctrl
        // before releasing Win so Windows does not open the Start menu.
        let mut t = mac_to_windows();
        assert_eq!(
            run(&mut t, &["+ControlLeft", "+Tab", "-Tab", "-ControlLeft"]),
            [
                "+MetaLeft",
                "+ControlLeft",
                "-MetaLeft",
                "+Tab",
                "-Tab",
                "-ControlLeft"
            ]
        );
        let (mut t, _) = translator(OsFamily::Windows, OsFamily::Mac);
        assert_eq!(
            run(&mut t, &["+ControlRight", "+Tab", "-Tab", "-ControlRight"]),
            [
                "+MetaRight",
                "+ControlRight",
                "-MetaRight",
                "+Tab",
                "-Tab",
                "-ControlRight"
            ]
        );
    }

    #[test]
    fn option_held_during_cmd_tab_shares_one_alt() {
        let mut t = mac_to_windows();
        assert_eq!(
            run(
                &mut t,
                &[
                    "+AltLeft",
                    "+MetaLeft",
                    "+Tab",
                    "-Tab",
                    "-MetaLeft",
                    "-AltLeft"
                ]
            ),
            [
                "+AltLeft",
                "+ControlLeft",
                // AltLeft is already down for Option: no second press.
                "-ControlLeft",
                "+Tab",
                "-Tab",
                // Option still holds it: no release yet.
                "-AltLeft"
            ]
        );
    }

    #[test]
    fn pc_ctrl_c_is_cmd_c_on_a_mac() {
        for viewer in [OsFamily::Windows, OsFamily::Linux] {
            let (mut t, _) = translator(viewer, OsFamily::Mac);
            assert_eq!(
                run(&mut t, &["+ControlLeft", "+KeyC", "-KeyC", "-ControlLeft"]),
                ["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"]
            );
            // The Win/Super key is the Mac's Control (documented choice).
            assert_eq!(
                run(&mut t, &["+MetaLeft", "-MetaLeft"]),
                ["+ControlLeft", "-ControlLeft"]
            );
        }
    }

    #[test]
    fn pc_alt_tab_is_cmd_tab_on_a_mac() {
        let (mut t, _) = translator(OsFamily::Windows, OsFamily::Mac);
        assert_eq!(
            run(
                &mut t,
                &[
                    "+AltLeft",
                    "+Tab",
                    "-Tab",
                    "+ShiftLeft",
                    "+Tab",
                    "-Tab",
                    "-ShiftLeft",
                    "-AltLeft"
                ]
            ),
            [
                "+AltLeft",
                "+MetaLeft",
                "-AltLeft",
                "+Tab",
                "-Tab",
                "+ShiftLeft",
                "+Tab",
                "-Tab",
                "-ShiftLeft",
                "-MetaLeft"
            ]
        );
        // Alt released before Tab.
        assert_eq!(
            run(&mut t, &["+AltRight", "+Tab", "-AltRight", "-Tab"]),
            [
                "+AltRight",
                "+MetaLeft",
                "-AltRight",
                "+Tab",
                "-MetaLeft",
                "-Tab"
            ]
        );
    }

    #[test]
    fn identity_pairs_are_untouched() {
        use OsFamily::*;
        let keys = [
            "+MetaLeft",
            "+Tab",
            "-Tab",
            "-MetaLeft",
            "+ControlLeft",
            "+KeyC",
            "-KeyC",
            "-ControlLeft",
            "+AltLeft",
            "+Tab",
            "-Tab",
            "-AltLeft",
        ];
        for (viewer, host) in [
            (Mac, Mac),
            (Windows, Windows),
            (Linux, Linux),
            (Linux, Windows),
            (Windows, Linux),
            (Mac, Unknown),
            (Windows, Unknown),
            (Unknown, Mac),
        ] {
            let (mut t, _) = translator(viewer, host);
            assert_eq!(run(&mut t, &keys), keys, "{viewer:?} → {host:?}");
        }
        // The setting off is identity too.
        let (mut t, on) = translator(Mac, Windows);
        on.store(false, Ordering::Relaxed);
        assert_eq!(run(&mut t, &keys), keys);
    }

    #[test]
    fn a_key_held_across_a_toggle_flip_is_released_as_sent() {
        let (mut t, on) = translator(OsFamily::Mac, OsFamily::Windows);
        assert_eq!(run(&mut t, &["+MetaLeft"]), ["+ControlLeft"]);
        on.store(false, Ordering::Relaxed);
        // New keys are literal at once; the held Cmd still releases Ctrl.
        assert_eq!(run(&mut t, &["+MetaRight"]), ["+MetaRight"]);
        assert_eq!(
            run(&mut t, &["-MetaLeft", "-MetaRight"]),
            ["-ControlLeft", "-MetaRight"]
        );
        // And the other way round.
        assert_eq!(run(&mut t, &["+MetaLeft"]), ["+MetaLeft"]);
        on.store(true, Ordering::Relaxed);
        assert_eq!(run(&mut t, &["-MetaLeft"]), ["-MetaLeft"]);
        // A chord rewrite after the flip still releases what was sent.
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+Tab"]),
            ["+ControlLeft", "+AltLeft", "-ControlLeft", "+Tab"]
        );
        on.store(false, Ordering::Relaxed);
        assert_eq!(run(&mut t, &["-Tab", "-MetaLeft"]), ["-Tab", "-AltLeft"]);
    }

    #[test]
    fn auto_repeat_repeats_the_sent_code() {
        let (mut t, on) = translator(OsFamily::Mac, OsFamily::Windows);
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+KeyZ", "+KeyZ", "+KeyZ"]),
            ["+ControlLeft", "+KeyZ", "+KeyZ", "+KeyZ"]
        );
        on.store(false, Ordering::Relaxed);
        assert_eq!(run(&mut t, &["+MetaLeft"]), ["+ControlLeft"]);
        assert_eq!(
            run(&mut t, &["-KeyZ", "-MetaLeft"]),
            ["-KeyZ", "-ControlLeft"]
        );
        assert!(t.held.is_empty());
    }

    #[test]
    fn an_unseen_release_is_forwarded_as_currently_mapped() {
        let mut t = mac_to_windows();
        assert_eq!(run(&mut t, &["-MetaLeft"]), ["-ControlLeft"]);
        assert_eq!(run(&mut t, &["-KeyQ"]), ["-KeyQ"]);
        // ...but not while a held key owns that code on the host: Cmd+Tab made
        // Cmd the host's Alt, and a stray Option release must not drop it.
        assert_eq!(
            run(&mut t, &["+MetaLeft", "+Tab", "-Tab"]),
            ["+ControlLeft", "+AltLeft", "-ControlLeft", "+Tab", "-Tab"]
        );
        assert_eq!(run(&mut t, &["-AltLeft"]), Vec::<String>::new());
        assert_eq!(run(&mut t, &["-MetaLeft"]), ["-AltLeft"]);
        assert!(t.held.is_empty());
    }
}
