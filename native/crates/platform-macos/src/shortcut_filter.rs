//! What the macOS shortcut tap swallows and what it forwards: the pure state
//! machine behind the event tap in `shortcut_hook.rs`, with no OS calls.
//!
//! The crate is macOS-only, but this file uses nothing beyond `std`, so
//! `tests/shortcut_filter.rs` compiles it on every OS and its tests run on the
//! Windows and Linux machines too, not only on the macOS CI runner.
//!
//! ## What is taken (M6 + M12)
//! While the video surface has the keyboard, the tap takes:
//! * **every key-down carrying Command**, with any other modifiers (M12).
//!   WHY all of them: AppKit offers a Command chord to `performKeyEquivalent:`
//!   down the key window's view hierarchy (WebKit claims Cmd+C/V/A/Z…) and
//!   then to the menu bar before any `keyDown:`, and the Dock, Spotlight,
//!   Mission Control and the screenshot service act on their chords before
//!   the app sees them at all. Only a session tap is ahead of all of them;
//! * Ctrl+←/→/↑/↓ — Spaces, Mission Control, App Exposé (M6), with any other
//!   modifiers.
//!
//! Its key-up is taken with it (see the invariants).
//!
//! ## What is never taken
//! * **Cmd+Q** (Command alone, no Shift/Option/Control): the one local escape
//!   hatch, so the user can always quit the viewer. The video view's key
//!   monitor lets it through to the menu bar as well (`view.rs`). "Q" is the
//!   key the layout types as `q` (what AppKit matches menu key equivalents
//!   against), or the physical ANSI Q key when the layout types no Latin
//!   letter there (Cyrillic, Greek…), see [`is_q`].
//! * [`NEVER_TAKEN`]: Force Quit (Cmd+Option+Esc) and Lock Screen
//!   (Cmd+Ctrl+Q). The user must keep a local way out whatever the remote
//!   session does, and macOS may act on them below the tap anyway.
//! * Keys macOS never delivers as key events at all, so no tap can see
//!   them: the power button / Touch ID, the media and brightness keys (they
//!   arrive as `NSSystemDefined` events, outside the tap's mask, or are
//!   handled below the window server — Apple: "Lower levels of the system trap
//!   and handle some events early in the event stream"), and anything typed
//!   while Secure Input is on.
//! * Modifier keys themselves (`flagsChanged`): never swallowed, so the OS's
//!   own modifier state can never get stuck because of us.
//! * Plain keys, and Option/Shift/Control chords other than Ctrl+arrows: the
//!   video view's key monitor forwards those, and macOS acts on none of them.
//!
//! ## Invariants (`platform_api::ShortcutHook`)
//! * A key is swallowed only while the video surface has the keyboard.
//! * Once a key-down was forwarded, its key-up is forwarded — at the latest
//!   when the surface loses the keyboard ([`ShortcutFilter::set_focus`]), so
//!   the host never keeps a key down while the user works elsewhere.
//! * A key-up is swallowed exactly when its key-down was: a lone key-up given
//!   to another app is meaningless, and one taken from it would be a stuck
//!   key. A key released to the host at focus loss but still physically held
//!   stays swallowed (silently) until its real key-up: the app that now has
//!   the keyboard never saw it go down.
//! * Every modifier the chord carries reaches the host **before** the chord
//!   key, and every modifier the chord no longer carries is released before
//!   it. The modifiers travel the surface's path (main thread → input sink),
//!   the taken key this one (tap thread → forwarder); both land in the same
//!   FIFO input channel in the engine, but the main thread may not have
//!   processed a modifier's `flagsChanged` yet when the tap already sees the
//!   next key, and when a modifier was already held as our window became key
//!   (Cmd+Tab *into* the app, then Tab again) the surface never saw it go down
//!   at all. So, per modifier key: down in the event's flags but not held by
//!   the surface → the filter forwards its down itself (and its up when it is
//!   released); up in the flags but still held by the surface → the filter
//!   forwards its up first (the surface's own, later up is then a harmless
//!   repeat on the host). This generalises M6's Cmd-only rule to all four
//!   modifiers, which M12 needs: Cmd+Shift+4 must never reach the host as
//!   Cmd+4 because Shift was still queued behind it. (The same fix as the
//!   Windows hook's synthesized Alt, `input/src/keyhook.rs`.)

/// `kVK_*` virtual key codes (Carbon `HIToolbox/Events.h`).
pub(crate) mod kvk {
    pub(crate) const ANSI_Q: u16 = 0x0C;
    pub(crate) const ESCAPE: u16 = 0x35;
    pub(crate) const RIGHT_COMMAND: u16 = 0x36;
    pub(crate) const COMMAND: u16 = 0x37;
    pub(crate) const SHIFT: u16 = 0x38;
    pub(crate) const OPTION: u16 = 0x3A;
    pub(crate) const CONTROL: u16 = 0x3B;
    pub(crate) const RIGHT_SHIFT: u16 = 0x3C;
    pub(crate) const RIGHT_OPTION: u16 = 0x3D;
    pub(crate) const RIGHT_CONTROL: u16 = 0x3E;
    pub(crate) const LEFT_ARROW: u16 = 0x7B;
    pub(crate) const RIGHT_ARROW: u16 = 0x7C;
    pub(crate) const DOWN_ARROW: u16 = 0x7D;
    pub(crate) const UP_ARROW: u16 = 0x7E;
}

/// `CGEventFlags` bits (`CGEventTypes.h`, the same values as
/// `NSEventModifierFlags`); the low word holds the device-dependent side bits
/// (`NX_DEVICE*KEYMASK`, `IOKit/hidsystem/IOLLEvent.h`).
pub(crate) mod flag {
    pub(crate) const SHIFT: u64 = 0x0002_0000;
    pub(crate) const CONTROL: u64 = 0x0004_0000;
    pub(crate) const OPTION: u64 = 0x0008_0000;
    pub(crate) const COMMAND: u64 = 0x0010_0000;
    pub(crate) const DEVICE_LEFT_CONTROL: u64 = 0x0000_0001;
    pub(crate) const DEVICE_LEFT_SHIFT: u64 = 0x0000_0002;
    pub(crate) const DEVICE_RIGHT_SHIFT: u64 = 0x0000_0004;
    pub(crate) const DEVICE_LEFT_COMMAND: u64 = 0x0000_0008;
    pub(crate) const DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
    pub(crate) const DEVICE_LEFT_OPTION: u64 = 0x0000_0020;
    pub(crate) const DEVICE_RIGHT_OPTION: u64 = 0x0000_0040;
    pub(crate) const DEVICE_RIGHT_CONTROL: u64 = 0x0000_2000;
}

/// The kind of keyboard event the tap delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyEvent {
    Down,
    Up,
    /// A modifier changed; `flags` carries the new modifier state.
    FlagsChanged,
}

/// One keyboard event, as the tap read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) kind: KeyEvent,
    /// `kVK_*` code (physical position).
    pub(crate) code: u16,
    /// `CGEventFlags` after the event.
    pub(crate) flags: u64,
    /// The character the layout types for this key, if it is exactly one
    /// (`CGEventKeyboardGetUnicodeString`); only used to recognise "Q".
    pub(crate) ch: Option<char>,
    /// `kCGKeyboardEventAutorepeat`: a key-down generated by holding the key.
    pub(crate) repeat: bool,
    /// The event carries our own injector's sentinel
    /// (`kCGEventSourceUserData == INJECT_SENTINEL`): a host on this same Mac
    /// typing for its remote viewer, not the user. Never taken, never
    /// counted: taking it would feed the remote its own input.
    pub(crate) own: bool,
}

/// What the tap does with the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Hand the event on to macOS unchanged.
    Pass,
    /// Drop it locally (the tap returns `NULL`).
    Swallow,
}

/// A combination the filter never claims, with why (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reserved {
    pub(crate) name: &'static str,
    /// The non-modifier key (`kvk::ANSI_Q` means "the Q key", see [`is_q`]).
    pub(crate) key: u16,
    /// Device-independent modifier flags that must all be held (others may
    /// be held too: Cmd+Option+Shift+Esc is the "force quit the front app"
    /// variant of the same escape).
    pub(crate) modifiers: u64,
}

/// Escape hatches that stay with macOS whatever the session does. Whether
/// macOS delivers them to a session tap at all is not documented; the filter
/// passes them on either way.
pub(crate) const NEVER_TAKEN: [Reserved; 2] = [
    Reserved {
        name: "Force Quit (Cmd+Option+Esc)",
        key: kvk::ESCAPE,
        modifiers: flag::COMMAND | flag::OPTION,
    },
    Reserved {
        name: "Lock Screen (Cmd+Ctrl+Q)",
        key: kvk::ANSI_Q,
        modifiers: flag::COMMAND | flag::CONTROL,
    },
];

/// Device-independent flags of the four chord modifiers.
const CHORD_MODIFIERS: u64 = flag::SHIFT | flag::CONTROL | flag::OPTION | flag::COMMAND;

/// Whether `code` is the key that types "q" on the current layout.
///
/// WHY the character first: AppKit matches menu key equivalents (Quit is
/// Cmd+"q") against the character the layout produces, so on AZERTY the
/// Quit key is the one in the ANSI A position. WHY the physical fallback:
/// when the layout types no Latin letter there (Cyrillic, Greek, or Option
/// turned it into a symbol), AppKit falls back to the key's position, and
/// so do we.
pub(crate) fn is_q(code: u16, ch: Option<char>) -> bool {
    match ch {
        Some(c) if c.is_ascii_alphabetic() => c.eq_ignore_ascii_case(&'q'),
        _ => code == kvk::ANSI_Q,
    }
}

/// Cmd+Q with no other modifier: quits the viewer locally, never sent.
pub(crate) fn is_local_quit(code: u16, flags: u64, ch: Option<char>) -> bool {
    flags & CHORD_MODIFIERS == flag::COMMAND && is_q(code, ch)
}

/// The [`NEVER_TAKEN`] entry this key-down is, if any.
pub(crate) fn never_taken(code: u16, flags: u64, ch: Option<char>) -> Option<&'static Reserved> {
    NEVER_TAKEN.iter().find(|r| {
        let key_matches = if r.key == kvk::ANSI_Q {
            is_q(code, ch)
        } else {
            code == r.key
        };
        key_matches && flags & r.modifiers == r.modifiers
    })
}

/// Whether the tap takes this key-down while the surface has the keyboard
/// (module docs: every Command chord except Cmd+Q and [`NEVER_TAKEN`], and
/// Ctrl+arrows).
pub(crate) fn takes(code: u16, flags: u64, ch: Option<char>) -> bool {
    if is_local_quit(code, flags, ch) || never_taken(code, flags, ch).is_some() {
        return false;
    }
    if flags & flag::COMMAND != 0 {
        return true;
    }
    matches!(
        code,
        kvk::LEFT_ARROW | kvk::RIGHT_ARROW | kvk::DOWN_ARROW | kvk::UP_ARROW
    ) && flags & flag::CONTROL != 0
}

/// One physical modifier key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModifierKey {
    pub(crate) code: u16,
    /// Device-independent flag ("some key of this kind is down").
    pub(crate) flag: u64,
    /// Side-specific device bit.
    pub(crate) side_bit: u64,
    /// Device bit of the key on the other side.
    other_bit: u64,
}

impl ModifierKey {
    const fn new(code: u16, flag: u64, side_bit: u64, other_bit: u64) -> Self {
        Self {
            code,
            flag,
            side_bit,
            other_bit,
        }
    }

    /// The left key of a pair: it stands for the kind when no side bit is set.
    fn is_left(self) -> bool {
        self.side_bit < self.other_bit
    }

    /// Whether this key is down in `flags`.
    ///
    /// WHY the fallback: hardware events carry the device-dependent side
    /// bits, but events synthesized by other software (remapping tools,
    /// accessibility apps) may carry only the device-independent flag. With
    /// no side bit at all, the left key stands for "a key of this kind": it is
    /// what [`ShortcutFilter`] forwards then, and it must be released when the
    /// device-independent flag clears — not on the next unrelated event.
    fn is_down(self, flags: u64) -> bool {
        if flags & (self.side_bit | self.other_bit) != 0 {
            flags & self.side_bit != 0
        } else {
            self.is_left() && flags & self.flag != 0
        }
    }
}

/// The eight modifier keys a chord can carry (Caps Lock and Fn are not
/// forwarded as chord modifiers: Caps Lock is a lock the surface forwards as
/// a tap, and Fn has no DOM code). Pinned to `keymap::MODIFIERS` by a test in
/// `shortcut_hook.rs`.
pub(crate) const MODIFIER_KEYS: [ModifierKey; 8] = [
    ModifierKey::new(
        kvk::COMMAND,
        flag::COMMAND,
        flag::DEVICE_LEFT_COMMAND,
        flag::DEVICE_RIGHT_COMMAND,
    ),
    ModifierKey::new(
        kvk::RIGHT_COMMAND,
        flag::COMMAND,
        flag::DEVICE_RIGHT_COMMAND,
        flag::DEVICE_LEFT_COMMAND,
    ),
    ModifierKey::new(
        kvk::CONTROL,
        flag::CONTROL,
        flag::DEVICE_LEFT_CONTROL,
        flag::DEVICE_RIGHT_CONTROL,
    ),
    ModifierKey::new(
        kvk::RIGHT_CONTROL,
        flag::CONTROL,
        flag::DEVICE_RIGHT_CONTROL,
        flag::DEVICE_LEFT_CONTROL,
    ),
    ModifierKey::new(
        kvk::OPTION,
        flag::OPTION,
        flag::DEVICE_LEFT_OPTION,
        flag::DEVICE_RIGHT_OPTION,
    ),
    ModifierKey::new(
        kvk::RIGHT_OPTION,
        flag::OPTION,
        flag::DEVICE_RIGHT_OPTION,
        flag::DEVICE_LEFT_OPTION,
    ),
    ModifierKey::new(
        kvk::SHIFT,
        flag::SHIFT,
        flag::DEVICE_LEFT_SHIFT,
        flag::DEVICE_RIGHT_SHIFT,
    ),
    ModifierKey::new(
        kvk::RIGHT_SHIFT,
        flag::SHIFT,
        flag::DEVICE_RIGHT_SHIFT,
        flag::DEVICE_LEFT_SHIFT,
    ),
];

/// The tap's memory: which forwarded presses still owe the host a release.
///
/// Lives on the tap thread only. Every method reports what to forward through
/// `emit(kVK, is_down)`, so the caller decides how a key code becomes a DOM
/// code, and nothing here allocates in the OS input callback.
#[derive(Debug)]
pub(crate) struct ShortcutFilter {
    /// Keys whose key-down was swallowed and forwarded: their key-up is owed
    /// to the host and is swallowed locally.
    swallowed: Vec<u16>,
    /// Modifier keys whose key-down this filter forwarded (the surface had
    /// not): their key-up is owed to the host.
    synthesized: Vec<ModifierKey>,
    /// Keys taken from the local apps whose release the host already got
    /// (focus moved away while they were held): their repeats and key-up are
    /// swallowed without forwarding.
    orphaned: Vec<u16>,
    /// Whether the surface had the keyboard at the last event or poll.
    focused: bool,
}

impl ShortcutFilter {
    pub(crate) fn new() -> Self {
        // A handful of keys at most are ever held at once; reserving up front
        // keeps allocation out of the OS input callback.
        Self {
            swallowed: Vec::with_capacity(8),
            synthesized: Vec::with_capacity(8),
            orphaned: Vec::with_capacity(8),
            focused: false,
        }
    }

    /// Follow the surface's keyboard focus. Losing it releases on the host
    /// everything this filter holds there, at once (clicking another window
    /// gives the user local behaviour back immediately, and the host is not
    /// left with keys down while the user works elsewhere). Called before
    /// every event and on every turn of the tap thread's run loop.
    pub(crate) fn set_focus(&mut self, focused: bool, emit: &mut impl FnMut(u16, bool)) {
        if self.focused && !focused {
            for key in self.swallowed.drain(..).rev() {
                emit(key, false);
                // WHY keep swallowing it: the app that now has the keyboard
                // never saw this key go down.
                if !self.orphaned.contains(&key) {
                    self.orphaned.push(key);
                }
            }
            for m in self.synthesized.drain(..).rev() {
                emit(m.code, false);
            }
        }
        self.focused = focused;
    }

    /// Decide one keyboard event.
    ///
    /// * `focused` — the video surface has the keyboard right now.
    /// * `surface_holds(kVK)` — the surface already forwarded that key's down
    ///   (and so owns its release).
    pub(crate) fn on_event(
        &mut self,
        key: Key,
        focused: bool,
        surface_holds: impl Fn(u16) -> bool,
        emit: &mut impl FnMut(u16, bool),
    ) -> Verdict {
        self.set_focus(focused, emit);
        if key.own {
            return Verdict::Pass;
        }
        match key.kind {
            KeyEvent::Down => self.on_down(key, surface_holds, emit),
            KeyEvent::Up => {
                if let Some(i) = self.orphaned.iter().position(|&k| k == key.code) {
                    self.orphaned.remove(i);
                    return Verdict::Swallow;
                }
                let Some(i) = self.swallowed.iter().position(|&k| k == key.code) else {
                    return Verdict::Pass;
                };
                self.swallowed.remove(i);
                emit(key.code, false);
                Verdict::Swallow
            }
            KeyEvent::FlagsChanged => {
                self.synthesized.retain(|m| {
                    let still_down = m.is_down(key.flags);
                    if !still_down {
                        emit(m.code, false);
                    }
                    still_down
                });
                Verdict::Pass
            }
        }
    }

    fn on_down(
        &mut self,
        key: Key,
        surface_holds: impl Fn(u16) -> bool,
        emit: &mut impl FnMut(u16, bool),
    ) -> Verdict {
        // Auto-repeat of a key we already took: keep taking it, so the host
        // keeps repeating and no local app sees a repeat without a press.
        if self.swallowed.contains(&key.code) {
            emit(key.code, true);
            return Verdict::Swallow;
        }
        if let Some(i) = self.orphaned.iter().position(|&k| k == key.code) {
            if key.repeat {
                return Verdict::Swallow;
            }
            // A fresh press: its key-up was lost (the tap missed it), so the
            // orphan is stale; decide this press on its own.
            self.orphaned.remove(i);
        }
        if !self.focused || !takes(key.code, key.flags, key.ch) {
            return Verdict::Pass;
        }
        self.sync_modifiers(key.flags, surface_holds, emit);
        self.swallowed.push(key.code);
        emit(key.code, true);
        Verdict::Swallow
    }

    /// Make the host's modifiers match `flags` before a taken key (see the
    /// ordering invariant in the module docs). Releases go first, so a
    /// modifier never overlaps the one replacing it into a third chord.
    fn sync_modifiers(
        &mut self,
        flags: u64,
        surface_holds: impl Fn(u16) -> bool,
        emit: &mut impl FnMut(u16, bool),
    ) {
        for m in MODIFIER_KEYS {
            // WHY the whole kind must be up: an event without side bits
            // reports only "some key of this kind"; releasing a right-hand
            // key the user still holds would be worse than a late release.
            if flags & m.flag == 0 && surface_holds(m.code) {
                emit(m.code, false);
            }
        }
        for m in MODIFIER_KEYS {
            let host_has =
                |code: u16| surface_holds(code) || self.synthesized.iter().any(|s| s.code == code);
            if !m.is_down(flags) || host_has(m.code) {
                continue;
            }
            // WHY: without side bits the event says only "a key of this kind
            // is down"; if the host already has one of the two, that is it
            // (the user holds right Shift, not an extra left one).
            let sided = flags & (m.side_bit | m.other_bit) != 0;
            if !sided
                && MODIFIER_KEYS
                    .iter()
                    .any(|o| o.flag == m.flag && host_has(o.code))
            {
                continue;
            }
            self.synthesized.push(m);
            emit(m.code, true);
        }
    }

    /// Forward a release for everything still owed and forget it: the tap is
    /// going away, or it missed events (disabled by a timeout, Secure Input)
    /// and can no longer see the real releases. Chord keys go first, so the
    /// host never sees a bare modifier-up turn the chord into another one.
    /// Orphans are forgotten too: their real key-ups may never arrive, and a
    /// stale orphan would swallow that key's next press.
    pub(crate) fn release_all(&mut self, emit: &mut impl FnMut(u16, bool)) {
        for key in self.swallowed.drain(..).rev() {
            emit(key, false);
        }
        for m in self.synthesized.drain(..).rev() {
            emit(m.code, false);
        }
        self.orphaned.clear();
    }
}
