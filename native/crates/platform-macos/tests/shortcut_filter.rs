//! The macOS shortcut tap's decisions (plan task M6), tested on every OS.
//!
//! WHY a `#[path]` include: the crate is `#![cfg(target_os = "macos")]`, so
//! its own unit tests run only on the macOS CI job. `src/shortcut_filter.rs`
//! is pure logic with no dependencies beyond `std`, so compiling the file
//! straight into this test lets `cargo test --workspace` exercise the swallow
//! rules and the stuck-key bookkeeping on Windows and Linux as well.

#[path = "../src/shortcut_filter.rs"]
mod shortcut_filter;

use shortcut_filter::{flag, kvk, KeyEvent, ShortcutFilter, Verdict};

/// Cmd held with the left key, as hardware reports it.
const CMD_L: u64 = flag::COMMAND | flag::DEVICE_LEFT_COMMAND;
const CMD_R: u64 = flag::COMMAND | flag::DEVICE_RIGHT_COMMAND;
const CTRL_L: u64 = flag::CONTROL | flag::DEVICE_LEFT_CONTROL;
const CTRL_R: u64 = flag::CONTROL | flag::DEVICE_RIGHT_CONTROL;
/// Option (left), for chords that must stay local.
const OPT_L: u64 = 0x0008_0000 | 0x0000_0020;
const SHIFT_L: u64 = 0x0002_0000 | 0x0000_0002;
const ESCAPE: u16 = 0x35;
const KEY_Q: u16 = 0x0C;
const DIGIT_3: u16 = 0x14;

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

    fn feed(&mut self, event: KeyEvent, key: u16, flags: u64) -> Verdict {
        let held = self.surface_held.clone();
        let sent = &mut self.sent;
        self.filter.on_event(
            event,
            key,
            flags,
            self.focused,
            |k| held.contains(&k),
            &mut |k, down| sent.push((k, down)),
        )
    }

    fn down(&mut self, key: u16, flags: u64) -> Verdict {
        self.feed(KeyEvent::Down, key, flags)
    }

    fn up(&mut self, key: u16, flags: u64) -> Verdict {
        self.feed(KeyEvent::Up, key, flags)
    }

    fn flags(&mut self, key: u16, flags: u64) -> Verdict {
        self.feed(KeyEvent::FlagsChanged, key, flags)
    }

    fn release_all(&mut self) {
        let sent = &mut self.sent;
        self.filter.release_all(&mut |k, down| sent.push((k, down)));
    }

    fn take(&mut self) -> Vec<(u16, bool)> {
        std::mem::take(&mut self.sent)
    }
}

#[test]
fn every_reserved_chord_is_swallowed_and_forwarded() {
    for (key, flags) in [
        (kvk::TAB, CMD_L),
        (kvk::TAB, CMD_L | SHIFT_L),
        (kvk::GRAVE, CMD_L),
        (kvk::SPACE, CMD_L),
        (kvk::SPACE, CMD_L | OPT_L),
        (kvk::LEFT_ARROW, CTRL_L),
        (kvk::RIGHT_ARROW, CTRL_L),
        (kvk::UP_ARROW, CTRL_L),
        (kvk::DOWN_ARROW, CTRL_L),
    ] {
        let mut h = Harness::focused();
        assert_eq!(h.down(key, flags), Verdict::Swallow, "{key:#04x}");
        assert_eq!(h.take().last(), Some(&(key, true)), "{key:#04x}");
        assert_eq!(h.up(key, flags), Verdict::Swallow, "{key:#04x}");
        assert_eq!(h.take(), [(key, false)], "{key:#04x}");
    }
}

#[test]
fn the_local_escape_hatches_and_app_shortcuts_pass() {
    let mut h = Harness::focused();
    // Force Quit, screenshots, Cmd+Q (the video view's monitor owns it).
    assert_eq!(h.down(ESCAPE, CMD_L | OPT_L), Verdict::Pass);
    assert_eq!(h.down(DIGIT_3, CMD_L | SHIFT_L), Verdict::Pass);
    assert_eq!(h.down(KEY_Q, CMD_L), Verdict::Pass);
    // The chord keys without their modifier, or with the other one.
    assert_eq!(h.down(kvk::TAB, 0), Verdict::Pass);
    assert_eq!(h.down(kvk::TAB, CTRL_L), Verdict::Pass);
    assert_eq!(h.down(kvk::LEFT_ARROW, CMD_L), Verdict::Pass);
    assert_eq!(h.down(kvk::SPACE, SHIFT_L), Verdict::Pass);
    // Ups of keys never taken.
    assert_eq!(h.up(kvk::TAB, 0), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn modifier_keys_are_never_swallowed() {
    let mut h = Harness::focused();
    assert_eq!(h.flags(kvk::COMMAND, CMD_L), Verdict::Pass);
    assert_eq!(h.down(kvk::TAB, CMD_L), Verdict::Swallow);
    assert_eq!(h.flags(kvk::COMMAND, 0), Verdict::Pass);
    assert_eq!(h.flags(kvk::CONTROL, CTRL_L), Verdict::Pass);
}

#[test]
fn nothing_is_taken_while_the_surface_lacks_the_keyboard() {
    let mut h = Harness::focused();
    h.focused = false;
    assert_eq!(h.down(kvk::TAB, CMD_L), Verdict::Pass);
    assert_eq!(h.up(kvk::TAB, CMD_L), Verdict::Pass);
    assert_eq!(h.down(kvk::UP_ARROW, CTRL_L), Verdict::Pass);
    assert!(h.take().is_empty());
}

#[test]
fn the_release_is_owed_even_after_focus_moved_away() {
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    assert_eq!(h.down(kvk::TAB, CMD_L), Verdict::Swallow);
    h.focused = false;
    // Repeats and the release of the taken key stay ours, and reach the host.
    assert_eq!(h.down(kvk::TAB, CMD_L), Verdict::Swallow);
    assert_eq!(h.up(kvk::TAB, CMD_L), Verdict::Swallow);
    assert_eq!(
        h.take(),
        [(kvk::TAB, true), (kvk::TAB, true), (kvk::TAB, false)]
    );
    // A fresh press while unfocused is local again.
    assert_eq!(h.down(kvk::TAB, CMD_L), Verdict::Pass);
}

#[test]
fn quick_release_of_the_modifier_first_still_releases_the_key() {
    // Cmd released before Tab: the Tab-up no longer looks like a chord.
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(kvk::TAB, CMD_L);
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.up(kvk::TAB, 0), Verdict::Swallow);
    assert_eq!(h.take(), [(kvk::TAB, true), (kvk::TAB, false)]);
}

#[test]
fn the_modifier_is_not_duplicated_when_the_surface_already_sent_it() {
    let mut h = Harness::focused();
    h.surface_held.push(kvk::COMMAND);
    h.down(kvk::TAB, CMD_L);
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(kvk::TAB, true)]);
}

#[test]
fn a_modifier_held_before_focus_is_forwarded_first_and_released_later() {
    // Cmd+Tab *into* the app with Cmd still held: the surface never saw Cmd.
    let mut h = Harness::focused();
    h.down(kvk::TAB, CMD_L);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (kvk::TAB, true)]);
    // Further Tabs cycle the host's switcher without another Cmd-down.
    h.up(kvk::TAB, CMD_L);
    h.down(kvk::TAB, CMD_L);
    assert_eq!(h.take(), [(kvk::TAB, false), (kvk::TAB, true)]);
    h.up(kvk::TAB, CMD_L);
    // Shift changing does not release Cmd; letting go of Cmd does.
    h.flags(0x38, CMD_L | SHIFT_L);
    assert_eq!(h.take(), [(kvk::TAB, false)]);
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
    h.release_all();
    assert!(h.take().is_empty());
}

#[test]
fn the_right_hand_modifier_is_forwarded_as_the_right_key() {
    let mut h = Harness::focused();
    h.down(kvk::TAB, CMD_R);
    assert_eq!(h.take(), [(kvk::RIGHT_COMMAND, true), (kvk::TAB, true)]);
    h.up(kvk::TAB, CMD_R);
    h.take();
    h.down(kvk::LEFT_ARROW, CTRL_R);
    assert_eq!(
        h.take(),
        [(kvk::RIGHT_CONTROL, true), (kvk::LEFT_ARROW, true)]
    );
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
    h.down(kvk::SPACE, both);
    assert_eq!(
        h.take(),
        [
            (kvk::COMMAND, true),
            (kvk::RIGHT_COMMAND, true),
            (kvk::SPACE, true)
        ]
    );
    h.up(kvk::SPACE, both);
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
    h.down(kvk::TAB, flag::COMMAND);
    assert_eq!(h.take(), [(kvk::COMMAND, true), (kvk::TAB, true)]);
    h.up(kvk::TAB, flag::COMMAND);
    h.take();
    h.flags(0x38, flag::COMMAND | SHIFT_L);
    assert!(h.take().is_empty(), "Cmd is still down");
    h.flags(kvk::COMMAND, 0);
    assert_eq!(h.take(), [(kvk::COMMAND, false)]);
}

#[test]
fn release_all_frees_keys_before_modifiers_and_forgets_them() {
    let mut h = Harness::focused();
    h.down(kvk::TAB, CMD_L);
    h.down(kvk::GRAVE, CMD_L);
    h.take();
    h.release_all();
    assert_eq!(
        h.take(),
        [
            (kvk::GRAVE, false),
            (kvk::TAB, false),
            (kvk::COMMAND, false)
        ]
    );
    // The real releases arriving afterwards are no longer ours.
    assert_eq!(h.up(kvk::TAB, CMD_L), Verdict::Pass);
    h.flags(kvk::COMMAND, 0);
    assert!(h.take().is_empty());
}
