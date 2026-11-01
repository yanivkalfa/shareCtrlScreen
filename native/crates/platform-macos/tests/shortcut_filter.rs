//! The macOS shortcut tap's decisions (plan tasks M6 and M12), tested on
//! every OS.
//!
//! WHY a `#[path]` include: the crate is `#![cfg(target_os = "macos")]`, so
//! its own unit tests run only on the macOS CI job. `src/shortcut_filter.rs`
//! is pure logic with no dependencies beyond `std`, so compiling the file
//! straight into this test lets `cargo test --workspace` exercise the swallow
//! rules and the stuck-key bookkeeping on Windows and Linux as well.

// WHY: this test drives the filter's behaviour, not every helper the tap
// uses (e.g. `ModifierKey::side_bit`, pinned by the macOS-only unit test).
#[allow(dead_code)]
#[path = "../src/shortcut_filter.rs"]
mod shortcut_filter;

use shortcut_filter::{
    flag, is_local_quit, is_q, kvk, never_taken, stays_local, takes, Key, KeyEvent, ShortcutFilter,
    Verdict, NEVER_TAKEN,
};

/// Cmd held with the left key, as hardware reports it.
const CMD_L: u64 = flag::COMMAND | flag::DEVICE_LEFT_COMMAND;
const CMD_R: u64 = flag::COMMAND | flag::DEVICE_RIGHT_COMMAND;
const CTRL_L: u64 = flag::CONTROL | flag::DEVICE_LEFT_CONTROL;
const CTRL_R: u64 = flag::CONTROL | flag::DEVICE_RIGHT_CONTROL;
const OPT_L: u64 = flag::OPTION | flag::DEVICE_LEFT_OPTION;
const SHIFT_L: u64 = flag::SHIFT | flag::DEVICE_LEFT_SHIFT;

// `kVK_*` codes the tests type (Carbon `HIToolbox/Events.h`).
const KEY_A: u16 = 0x00;
const KEY_D: u16 = 0x02;
const KEY_F: u16 = 0x03;
const KEY_H: u16 = 0x04;
const KEY_C: u16 = 0x08;
const KEY_V: u16 = 0x09;
const KEY_W: u16 = 0x0D;
const DIGIT_3: u16 = 0x14;
const DIGIT_4: u16 = 0x15;
const DIGIT_5: u16 = 0x17;
const ENTER: u16 = 0x24;
const TAB: u16 = 0x30;
const SPACE: u16 = 0x31;
const GRAVE: u16 = 0x32;
const KEY_M: u16 = 0x2E;
const F3: u16 = 0x63;

/// Drives a filter and records everything it forwards.
struct Harness {
    filter: ShortcutFilter,
    focused: bool,
    surface_held: Vec<u16>,
    sent: Vec<(u16, bool)>,
}

impl Harness {
    fn focused() -> Self {
        Self {
            filter: ShortcutFilter::new(),
            focused: true,
            surface_held: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn feed(&mut self, key: Key) -> Verdict {
        let held = self.surface_held.clone();
        let sent = &mut self.sent;
        self.filter
            .on_event(key, self.focused, |k| held.contains(&k), &mut |k, down| {
                sent.push((k, down))
            })
    }

    fn key(kind: KeyEvent, code: u16, flags: u64) -> Key {
        Key {
            kind,
            code,
            flags,
            ch: None,
            repeat: false,
            own: false,
        }
    }

    fn down(&mut self, code: u16, flags: u64) -> Verdict {
        self.feed(Self::key(KeyEvent::Down, code, flags))
    }

    /// A key-down typing `ch` on the current layout.
    fn down_ch(&mut self, code: u16, flags: u64, ch: char) -> Verdict {
        self.feed(Key {
            ch: Some(ch),
            ..Self::key(KeyEvent::Down, code, flags)
        })
    }

    fn repeat(&mut self, code: u16, flags: u64) -> Verdict {
        self.feed(Key {
            repeat: true,
            ..Self::key(KeyEvent::Down, code, flags)
        })
    }

    fn up(&mut self, code: u16, flags: u64) -> Verdict {
        self.feed(Self::key(KeyEvent::Up, code, flags))
    }

    fn flags(&mut self, code: u16, flags: u64) -> Verdict {
        self.feed(Self::key(KeyEvent::FlagsChanged, code, flags))
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        let sent = &mut self.sent;
        self.filter
            .set_focus(focused, &mut |k, down| sent.push((k, down)));
    }

    fn release_all(&mut self) {
        let sent = &mut self.sent;
        self.filter.release_all(&mut |k, down| sent.push((k, down)));
    }

    fn take(&mut self) -> Vec<(u16, bool)> {
        std::mem::take(&mut self.sent)
    }
}

// ---- M12: what is taken -------------------------------------------------

#[test]
fn any_cmd_chord_is_taken_and_forwarded() {
    for (key, flags) in [
        (KEY_C, CMD_L),
        (KEY_V, CMD_L),
        (KEY_A, CMD_L),
        (KEY_W, CMD_L),
        (KEY_H, CMD_L),
        (KEY_M, CMD_L),
        (ENTER, CMD_L),
        (ENTER, CMD_R),
        (DIGIT_3, CMD_L | SHIFT_L),
        (DIGIT_4, CMD_L | SHIFT_L),
        (DIGIT_5, CMD_L | SHIFT_L),
        (KEY_D, CMD_L | OPT_L),
        (KEY_F, CMD_L | CTRL_L),
        (SPACE, CMD_L | CTRL_L),
        (F3, CMD_L),
        (kvk::LEFT_ARROW, CMD_L),
        (kvk::ESCAPE, CMD_L),
        (kvk::ANSI_Q, CMD_L | SHIFT_L),
        (kvk::ANSI_Q, CMD_L | OPT_L),
        // The M6 set is a subset.
        (TAB, CMD_L),
        (TAB, CMD_L | SHIFT_L),
        (GRAVE, CMD_L),
        (SPACE, CMD_L),
        (SPACE, CMD_L | OPT_L),
    ] {
        let mut h = Harness::focused();
        assert!(takes(key, flags, None), "{key:#04x} {flags:#x}");
        assert_eq!(
            h.down(key, flags),
            Verdict::Swallow,
            "{key:#04x} {flags:#x}"
        );
        assert_eq!(h.take().last(), Some(&(key, true)), "{key:#04x}");
        assert_eq!(h.up(key, flags), Verdict::Swallow, "{key:#04x}");
        assert_eq!(h.take(), [(key, false)], "{key:#04x}");
    }
}

#[test]
fn ctrl_arrows_are_taken_with_any_other_modifier() {
    for key in [
        kvk::LEFT_ARROW,
        kvk::RIGHT_ARROW,
        kvk::UP_ARROW,
        kvk::DOWN_ARROW,
    ] {
        for flags in [CTRL_L, CTRL_R, CTRL_L | SHIFT_L, CTRL_L | OPT_L] {
            let mut h = Harness::focused();
            assert_eq!(
                h.down(key, flags),
                Verdict::Swallow,
                "{key:#04x} {flags:#x}"
            );
            assert_eq!(h.take().last(), Some(&(key, true)));
            assert_eq!(h.up(key, flags), Verdict::Swallow);
            assert_eq!(h.take(), [(key, false)]);
        }
    }
}

#[test]
fn plain_keys_and_non_cmd_chords_are_untouched() {
    let mut h = Harness::focused();
    for (key, flags) in [
        (KEY_C, 0),
        (KEY_C, SHIFT_L),
        (KEY_C, OPT_L),
        (KEY_C, CTRL_L),
        (TAB, 0),
        (TAB, CTRL_L),
        (TAB, OPT_L),
        (SPACE, SHIFT_L),
        (SPACE, CTRL_L),
        (ENTER, 0),
        (kvk::ESCAPE, 0),
        (kvk::LEFT_ARROW, 0),
        (kvk::LEFT_ARROW, SHIFT_L),
        (kvk::UP_ARROW, OPT_L),
        (F3, 0),
    ] {
        assert!(!takes(key, flags, None), "{key:#04x} {flags:#x}");
        assert_eq!(h.down(key, flags), Verdict::Pass, "{key:#04x} {flags:#x}");
        assert_eq!(h.up(key, flags), Verdict::Pass, "{key:#04x} {flags:#x}");
    }
    assert!(h.take().is_empty());
}

#[test]
fn cmd_q_stays_local() {
    let mut h = Harness::focused();
    assert_eq!(h.down_ch(kvk::ANSI_Q, CMD_L, 'q'), Verdict::Pass);
    assert_eq!(h.down_ch(kvk::ANSI_Q, CMD_R, 'q'), Verdict::Pass);
    // No character known: the physical Q key.
    assert_eq!(h.down(kvk::ANSI_Q, CMD_L), Verdict::Pass);
    // Caps Lock on does not change it.
    assert_eq!(
        h.down_ch(kvk::ANSI_Q, CMD_L | 0x0001_0000, 'Q'),
        Verdict::Pass
    );
    assert_eq!(h.up(kvk::ANSI_Q, CMD_L), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn the_quit_key_follows_the_layout_like_appkit_does() {
    // AZERTY: "q" is typed by the key in the ANSI A position…
    assert!(is_local_quit(KEY_A, CMD_L, Some('q')));
    // …and the ANSI Q position types "a": that is Cmd+A, taken.
    assert!(!is_local_quit(kvk::ANSI_Q, CMD_L, Some('a')));
    assert!(takes(kvk::ANSI_Q, CMD_L, Some('a')));
    // A non-Latin layout (Russian: "й") falls back to the physical key.
    assert!(is_local_quit(kvk::ANSI_Q, CMD_L, Some('й')));
    assert!(!is_local_quit(KEY_A, CMD_L, Some('ф')));
    assert!(is_q(kvk::ANSI_Q, None));
    // Only Command alone: Cmd+Shift+Q and Cmd+Option+Q go to the remote.
    assert!(!is_local_quit(kvk::ANSI_Q, CMD_L | SHIFT_L, Some('q')));
    assert!(!is_local_quit(kvk::ANSI_Q, CMD_L | OPT_L, None));
    assert!(!is_local_quit(kvk::ANSI_Q, 0, Some('q')));
}

#[test]
fn reserved_combos_are_listed_and_never_claimed() {
    let names: Vec<&str> = NEVER_TAKEN.iter().map(|r| r.name).collect();
    assert!(names.iter().any(|n| n.contains("Force Quit")), "{names:?}");
    assert!(names.iter().any(|n| n.contains("Lock Screen")), "{names:?}");
    let mut h = Harness::focused();
    for r in NEVER_TAKEN {
        assert!(
            never_taken(r.key, r.modifiers, None).is_some(),
            "{}",
            r.name
        );
        assert!(!takes(r.key, r.modifiers, None), "{}", r.name);
        assert_eq!(h.down(r.key, r.modifiers), Verdict::Pass, "{}", r.name);
        assert_eq!(h.up(r.key, r.modifiers), Verdict::Pass, "{}", r.name);
    }
    // With side bits, the extra Shift of "force quit the front app", and the
    // layout's "q" for Lock Screen.
    assert_eq!(h.down(kvk::ESCAPE, CMD_L | OPT_L), Verdict::Pass);
    assert_eq!(h.down(kvk::ESCAPE, CMD_R | OPT_L | SHIFT_L), Verdict::Pass);
    assert_eq!(h.down_ch(KEY_A, CMD_L | CTRL_L, 'q'), Verdict::Pass);
    assert_eq!(h.down(kvk::ANSI_Q, CMD_L | CTRL_L), Verdict::Pass);
    assert!(h.take().is_empty());
    // Escape with Cmd alone is an ordinary Cmd chord.
    assert!(never_taken(kvk::ESCAPE, CMD_L, None).is_none());
}

#[test]
fn the_key_monitor_lets_every_escape_hatch_through() {
    // Key-downs: Cmd+Q, Force Quit (also with Shift), Lock Screen.
    assert!(stays_local(true, kvk::ANSI_Q, CMD_L, Some('q'), false));
    assert!(stays_local(true, kvk::ESCAPE, CMD_L | OPT_L, None, false));
    assert!(stays_local(
        true,
        kvk::ESCAPE,
        CMD_R | OPT_L | SHIFT_L,
        None,
        false
    ));
    assert!(stays_local(
        true,
        kvk::ANSI_Q,
        CMD_L | CTRL_L,
        Some('q'),
        false
    ));
    assert!(stays_local(true, KEY_A, CMD_L | CTRL_L, Some('q'), false));
    // Their key-ups, whatever the modifiers are by then.
    assert!(stays_local(false, kvk::ANSI_Q, 0, Some('q'), false));
    assert!(stays_local(false, kvk::ESCAPE, 0, None, false));
    // Unless the remote got the press: its release is owed there.
    assert!(!stays_local(true, kvk::ANSI_Q, CMD_L, Some('q'), true));
    assert!(!stays_local(false, kvk::ESCAPE, 0, None, true));
    // Everything else goes to the remote.
    for (code, flags, ch) in [
        (kvk::ANSI_Q, CMD_L | SHIFT_L, Some('q')),
        (kvk::ANSI_Q, 0, Some('q')),
        (kvk::ESCAPE, 0, None),
        (kvk::ESCAPE, CMD_L, None),
        (KEY_C, CMD_L, Some('c')),
        (KEY_A, CMD_L, Some('a')),
    ] {
        assert!(!stays_local(true, code, flags, ch, false), "{code:#04x}");
    }
    assert!(!stays_local(false, KEY_C, CMD_L, Some('c'), false));
}

#[test]
fn our_own_injected_events_are_ignored() {
    let mut h = Harness::focused();
    let own = |kind, code, flags| Key {
        own: true,
        ..Harness::key(kind, code, flags)
    };
    assert_eq!(h.feed(own(KeyEvent::Down, KEY_C, CMD_L)), Verdict::Pass);
    assert_eq!(h.feed(own(KeyEvent::Down, TAB, CMD_L)), Verdict::Pass);
    assert_eq!(h.feed(own(KeyEvent::Up, TAB, CMD_L)), Verdict::Pass);
    assert!(h.take().is_empty());
    // An injected modifier change does not release the user's own Cmd.
    h.down(TAB, CMD_L);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (TAB, true)]);
    assert_eq!(
        h.feed(own(KeyEvent::FlagsChanged, kvk::COMMAND, 0)),
        Verdict::Pass
    );
    assert!(h.take().is_empty());
    // Nor does an injected key-up of a key the user is holding.
    assert_eq!(h.feed(own(KeyEvent::Up, TAB, CMD_L)), Verdict::Pass);
    assert!(h.take().is_empty());
    assert_eq!(h.up(TAB, CMD_L), Verdict::Swallow);
    assert_eq!(h.take(), [(TAB, false)]);
}

// ---- ordering: every modifier reaches the host before the chord key ------

#[test]
fn cmd_shift_4_forwards_the_shift_the_surface_has_not_sent_yet() {
    // The surface sent Cmd; Shift's flagsChanged is still queued on the main
    // thread when the tap sees 4.
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(DIGIT_4, CMD_L | SHIFT_L);
    assert_eq!(h.take(), [(kvk::SHIFT, true), (DIGIT_4, true)]);
    h.up(DIGIT_4, CMD_L | SHIFT_L);
    h.flags(kvk::SHIFT, CMD_L);
    assert_eq!(h.take(), [(DIGIT_4, false), (kvk::SHIFT, false)]);
}

#[test]
fn every_modifier_held_before_capture_is_forwarded_first() {
    let mut h = Harness::focused();
    h.down(KEY_D, CMD_L | OPT_L | SHIFT_L | CTRL_R);
    assert_eq!(
        h.take(),
        [
            (kvk::COMMAND, true),
            (kvk::RIGHT_CONTROL, true),
            (kvk::OPTION, true),
            (kvk::SHIFT, true),
            (KEY_D, true)
        ]
    );
    h.up(KEY_D, CMD_L | OPT_L | SHIFT_L | CTRL_R);
    h.release_all();
    assert_eq!(
        h.take(),
        [
            (KEY_D, false),
            (kvk::SHIFT, false),
            (kvk::OPTION, false),
            (kvk::RIGHT_CONTROL, false),
            (kvk::COMMAND, false)
        ]
    );
}

#[test]
fn a_modifier_the_surface_still_holds_but_the_chord_lacks_is_released_first() {
    // Cmd+Shift+Z, Shift let go, Z again: the surface has not processed the
    // Shift-up yet, so the host would see Cmd+Shift+Z twice.
    let mut h = Harness::focused();
    h.surface_held.extend([kvk::COMMAND, kvk::SHIFT]);
    h.down(0x06, CMD_L | SHIFT_L);
    h.up(0x06, CMD_L | SHIFT_L);
    h.take();
    h.down(0x06, CMD_L);
    assert_eq!(h.take(), [(kvk::SHIFT, false), (0x06, true)]);
}

#[test]
fn a_side_bitless_event_never_releases_a_held_right_key() {
    // Software-synthesized events may carry only the shared flag.
    let mut h = Harness::focused();
    h.surface_held.extend([kvk::COMMAND, kvk::RIGHT_SHIFT]);
    h.down(KEY_C, flag::COMMAND | flag::SHIFT);
    assert_eq!(h.take(), [(KEY_C, true)]);
}

// ---- held before capture, focus loss, release ---------------------------

#[test]
fn a_cmd_pressed_before_capture_is_released_on_the_host() {
    // Cmd went down while another app had the keyboard, so the surface never
    // sent it; the tap sends it with the first chord and owes its release.
    let mut h = Harness::focused();
    h.flags(kvk::COMMAND, CMD_L);
    assert!(h.take().is_empty());
    h.down(KEY_C, CMD_L);
    h.up(KEY_C, CMD_L);
    assert_eq!(
        h.take(),
        [(kvk::COMMAND, true), (KEY_C, true), (KEY_C, false)]
    );
    assert_eq!(h.flags(kvk::COMMAND, 0), Verdict::Pass);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
}

#[test]
fn a_key_pressed_before_capture_keeps_its_key_up() {
    // C went down plainly (the surface forwarded it); its key-up, now with
    // Cmd held, is not swallowed: the view's monitor forwards it.
    let mut h = Harness::focused();
    h.surface_held.push(KEY_C);
    assert_eq!(h.down(KEY_C, 0), Verdict::Pass);
    assert_eq!(h.up(KEY_C, CMD_L), Verdict::Pass);
    // Pressed while unfocused, released after focus came: also untouched.
    h.set_focus(false);
    assert_eq!(h.down(KEY_V, CMD_L), Verdict::Pass);
    h.set_focus(true);
    assert_eq!(h.up(KEY_V, CMD_L), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn focus_loss_releases_everything_at_once_and_restores_local_keys() {
    let mut h = Harness::focused();
    h.down(KEY_C, CMD_L);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (KEY_C, true)]);
    // A click on another window, with Cmd and C still physically held.
    h.set_focus(false);
    assert_eq!(h.take(), [(KEY_C, false), (kvk::COMMAND, false)]);
    // The other app never saw C go down: its repeats and its up stay ours,
    // silently.
    assert_eq!(h.repeat(KEY_C, CMD_L), Verdict::Swallow);
    assert_eq!(h.up(KEY_C, CMD_L), Verdict::Swallow);
    assert_eq!(h.flags(kvk::COMMAND, 0), Verdict::Pass);
    assert!(h.take().is_empty());
    // Everything is local again.
    assert_eq!(h.down(KEY_C, CMD_L), Verdict::Pass);
    assert_eq!(h.down(TAB, CMD_L), Verdict::Pass);
    assert_eq!(h.up(TAB, CMD_L), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn focus_loss_is_also_seen_through_the_next_event() {
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(TAB, CMD_L);
    h.take();
    // No poll in between: the next event carries the new focus.
    h.focused = false;
    assert_eq!(h.up(TAB, CMD_L), Verdict::Swallow);
    assert_eq!(h.take(), [(TAB, false)]);
    assert_eq!(h.down(TAB, CMD_L), Verdict::Pass);
}

#[test]
fn a_fresh_press_after_a_lost_key_up_is_decided_anew() {
    let mut h = Harness::focused();
    h.down(KEY_C, CMD_L);
    h.set_focus(false);
    h.take();
    // C's up was lost; focus came back; C pressed again (not a repeat).
    h.set_focus(true);
    assert_eq!(h.down(KEY_C, CMD_L), Verdict::Swallow);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (KEY_C, true)]);
}

#[test]
fn auto_repeat_of_a_taken_key_keeps_going_to_the_host() {
    let mut h = Harness::focused();
    h.surface_held.push(kvk::CONTROL);
    h.down(kvk::RIGHT_ARROW, CTRL_L);
    assert_eq!(h.repeat(kvk::RIGHT_ARROW, CTRL_L), Verdict::Swallow);
    assert_eq!(h.up(kvk::RIGHT_ARROW, CTRL_L), Verdict::Swallow);
    assert_eq!(
        h.take(),
        [
            (kvk::RIGHT_ARROW, true),
            (kvk::RIGHT_ARROW, true),
            (kvk::RIGHT_ARROW, false)
        ]
    );
}

#[test]
fn release_all_also_forgets_orphans() {
    let mut h = Harness::focused();
    h.down(KEY_C, CMD_L);
    h.set_focus(false);
    h.take();
    // Secure Input or a tap timeout: the orphan's up may never come.
    h.release_all();
    assert!(h.take().is_empty());
    assert_eq!(h.repeat(KEY_C, CMD_L), Verdict::Pass);
}

// ---- M6 behaviour kept ---------------------------------------------------

#[test]
fn modifier_keys_are_never_swallowed() {
    let mut h = Harness::focused();
    assert_eq!(h.flags(kvk::COMMAND, CMD_L), Verdict::Pass);
    assert_eq!(h.down(TAB, CMD_L), Verdict::Swallow);
    assert_eq!(h.flags(kvk::COMMAND, 0), Verdict::Pass);
    assert_eq!(h.flags(kvk::CONTROL, CTRL_L), Verdict::Pass);
}

#[test]
fn nothing_is_taken_while_the_surface_lacks_the_keyboard() {
    let mut h = Harness::focused();
    h.focused = false;
    assert_eq!(h.down(TAB, CMD_L), Verdict::Pass);
    assert_eq!(h.up(TAB, CMD_L), Verdict::Pass);
    assert_eq!(h.down(KEY_C, CMD_L), Verdict::Pass);
    assert_eq!(h.down(kvk::UP_ARROW, CTRL_L), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn quick_release_of_the_modifier_first_still_releases_the_key() {
    // Cmd released before Tab: the Tab-up no longer looks like a chord.
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(TAB, CMD_L);
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.up(TAB, 0), Verdict::Swallow);
    assert_eq!(h.take(), [(TAB, true), (TAB, false)]);
}

#[test]
fn the_modifier_is_not_duplicated_when_the_surface_already_sent_it() {
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(TAB, CMD_L);
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(TAB, true)]);
}

#[test]
fn a_modifier_held_before_focus_is_forwarded_first_and_released_later() {
    // Cmd+Tab *into* the app with Cmd still held: the surface never saw Cmd.
    let mut h = Harness::focused();
    h.down(TAB, CMD_L);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (TAB, true)]);
    // Further Tabs cycle the host's switcher without another Cmd-down.
    h.up(TAB, CMD_L);
    h.down(TAB, CMD_L);
    assert_eq!(h.take(), [(TAB, false), (TAB, true)]);
    h.up(TAB, CMD_L);
    assert_eq!(h.take(), [(TAB, false)]);
    // Letting go of Cmd releases it.
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
    h.release_all();
    assert!(h.take().is_empty());
}

#[test]
fn shift_added_mid_chord_is_forwarded_and_does_not_release_cmd() {
    let mut h = Harness::focused();
    h.down(TAB, CMD_L);
    h.up(TAB, CMD_L);
    h.take();
    // Shift pressed (the surface forwards it on the main thread).
    h.surface_held.push(kvk::SHIFT);
    h.flags(kvk::SHIFT, CMD_L | SHIFT_L);
    h.down(TAB, CMD_L | SHIFT_L);
    assert_eq!(h.take(), [(TAB, true)]);
}

#[test]
fn the_right_hand_modifier_is_forwarded_as_the_right_key() {
    let mut h = Harness::focused();
    h.down(TAB, CMD_R);
    assert_eq!(h.take(), [(kvk::RIGHT_COMMAND, true), (TAB, true)]);
    h.up(TAB, CMD_R);
    h.take();
    h.down(kvk::LEFT_ARROW, CMD_R | CTRL_R);
    assert_eq!(
        h.take(),
        [(kvk::RIGHT_CONTROL, true), (kvk::LEFT_ARROW, true)]
    );
    h.up(kvk::LEFT_ARROW, CMD_R | CTRL_R);
    h.take();
    // Releasing the right Cmd while right Ctrl stays down.
    h.flags(kvk::RIGHT_COMMAND, CTRL_R);
    assert_eq!(h.take(), [(kvk::RIGHT_COMMAND, false)]);
    h.flags(kvk::RIGHT_CONTROL, 0);
    assert_eq!(h.take(), [(kvk::RIGHT_CONTROL, false)]);
}

#[test]
fn both_sides_held_are_both_forwarded_and_released_one_by_one() {
    let both = flag::COMMAND | flag::DEVICE_LEFT_COMMAND | flag::DEVICE_RIGHT_COMMAND;
    let mut h = Harness::focused();
    h.down(SPACE, both);
    assert_eq!(
        h.take(),
        [
            (kvk::COMMAND, true),
            (kvk::RIGHT_COMMAND, true),
            (SPACE, true)
        ]
    );
    h.up(SPACE, both);
    h.take();
    // The left one goes up; the shared flag is still set by the right one.
    h.flags(kvk::COMMAND, CMD_R);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
    h.flags(kvk::RIGHT_COMMAND, 0);
    assert_eq!(h.take(), [(kvk::RIGHT_COMMAND, false)]);
}

#[test]
fn flags_without_side_bits_stand_for_the_left_key_until_the_flag_clears() {
    // Events synthesized by other software may lack the device bits.
    let mut h = Harness::focused();
    h.down(TAB, flag::COMMAND);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (TAB, true)]);
    h.up(TAB, flag::COMMAND);
    h.take();
    h.flags(kvk::SHIFT, flag::COMMAND | SHIFT_L);
    assert!(h.take().is_empty(), "Cmd is still down");
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
}

#[test]
fn release_all_frees_keys_before_modifiers_and_forgets_them() {
    let mut h = Harness::focused();
    h.down(TAB, CMD_L);
    h.down(GRAVE, CMD_L);
    h.take();
    h.release_all();
    assert_eq!(
        h.take(),
        [(GRAVE, false), (TAB, false), (kvk::COMMAND, false)]
    );
    // The real releases arriving afterwards are no longer ours.
    assert_eq!(h.up(TAB, CMD_L), Verdict::Pass);
    h.flags(kvk::COMMAND, 0);
    assert!(h.take().is_empty());
}
