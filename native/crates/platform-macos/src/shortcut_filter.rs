//! What the macOS shortcut tap swallows and what it forwards: the pure state
//! machine behind the event tap in `shortcut_hook.rs`, with no OS calls.
//!
//! The crate is macOS-only, but this file uses nothing beyond `std`, so
//! `tests/shortcut_filter.rs` compiles it on every OS and its tests run on the
//! Windows and Linux machines too, not only on the macOS CI runner.
//!
//! ## What is reserved
//! The chords macOS acts on **before** the focused app sees them (the Dock,
//! Spotlight and Mission Control are system services), so the video view's own
//! key monitor can never forward them:
//! * Cmd+Tab (+Shift) — app switcher; Cmd+\` — next window of the app;
//!   Cmd+Space — Spotlight / input source;
//! * Ctrl+←/→ — switch Space; Ctrl+↑ — Mission Control; Ctrl+↓ — App Exposé.
//!
//! Deliberately **not** reserved:
//! * Cmd+Q, Cmd+H, Cmd+M, Cmd+W — app menu shortcuts. They go to the focused
//!   app, where the video view's key monitor already sends them to the remote
//!   (`view.rs`, `install_key_monitor`); a second path would forward them twice.
//! * Cmd+Option+Esc (Force Quit) and the Cmd+Shift+3/4/5 screenshots: the
//!   user must always keep a local way out, whatever the remote session does.
//! * Modifier keys themselves (`flagsChanged`): never swallowed, so the OS's
//!   own modifier state can never get stuck because of us.
//!
//! ## Invariants (`platform_api::ShortcutHook`)
//! * A key is swallowed only while the video surface has the keyboard.
//! * Once a key-down was forwarded, its key-up is forwarded unconditionally —
//!   focus may move away mid-press, and the host would keep the key down.
//! * A key-up is swallowed exactly when its key-down was: a lone key-up given
//!   to another app is meaningless, and one taken from it would be a stuck key.
//! * The chord's modifier reaches the host **before** the chord key. The
//!   modifier itself travels the surface's path (main thread → input sink),
//!   the swallowed key this one (tap thread → forwarder); and when Cmd was
//!   already held as our window became key (Cmd+Tab *into* the app, then Tab
//!   again) the surface never saw Cmd go down at all. So unless the surface
//!   already holds it, the filter forwards the modifier's down itself, and
//!   its up when the modifier is released (the same fix as the Windows hook's
//!   synthesized Alt, `input/src/keyhook.rs`).

/// `kVK_*` virtual key codes (Carbon `HIToolbox/Events.h`).
pub(crate) mod kvk {
    pub(crate) const TAB: u16 = 0x30;
    pub(crate) const SPACE: u16 = 0x31;
    pub(crate) const GRAVE: u16 = 0x32;
    pub(crate) const RIGHT_COMMAND: u16 = 0x36;
    pub(crate) const COMMAND: u16 = 0x37;
    pub(crate) const CONTROL: u16 = 0x3B;
    pub(crate) const RIGHT_CONTROL: u16 = 0x3E;
    pub(crate) const LEFT_ARROW: u16 = 0x7B;
    pub(crate) const RIGHT_ARROW: u16 = 0x7C;
    pub(crate) const DOWN_ARROW: u16 = 0x7D;
    pub(crate) const UP_ARROW: u16 = 0x7E;
}

/// `CGEventFlags` bits (`CGEventTypes.h`); the low word holds the
/// device-dependent side bits (`IOKit/hidsystem/IOLLEvent.h`).
pub(crate) mod flag {
    pub(crate) const CONTROL: u64 = 0x0004_0000;
    pub(crate) const COMMAND: u64 = 0x0010_0000;
    pub(crate) const DEVICE_LEFT_CONTROL: u64 = 0x0000_0001;
    pub(crate) const DEVICE_LEFT_COMMAND: u64 = 0x0000_0008;
    pub(crate) const DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
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

/// What the tap does with the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Hand the event on to macOS unchanged.
    Pass,
    /// Drop it locally (the tap returns `NULL`).
    Swallow,
}

/// The modifier a reserved chord is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    Command,
    Control,
}

impl Modifier {
    /// Device-independent flag ("some Command key is down").
    fn flag(self) -> u64 {
        match self {
            Self::Command => flag::COMMAND,
            Self::Control => flag::CONTROL,
        }
    }

    /// `(kVK, device bit)` of the left and the right key.
    fn sides(self) -> [(u16, u64); 2] {
        match self {
            Self::Command => [
                (kvk::COMMAND, flag::DEVICE_LEFT_COMMAND),
                (kvk::RIGHT_COMMAND, flag::DEVICE_RIGHT_COMMAND),
            ],
            Self::Control => [
                (kvk::CONTROL, flag::DEVICE_LEFT_CONTROL),
                (kvk::RIGHT_CONTROL, flag::DEVICE_RIGHT_CONTROL),
            ],
        }
    }

    /// Whether the side key with device bit `side_bit` is down in `flags`.
    ///
    /// WHY the fallback: hardware events carry the device-dependent side
    /// bits, but events synthesized by other software (remapping tools,
    /// accessibility apps) may carry only the device-independent flag. With
    /// no side bit at all, the left key stands for "a key of this kind": it is
    /// what [`ShortcutFilter`] forwards then, and it must be released when the
    /// device-independent flag clears — not on the next unrelated event.
    fn side_is_down(self, side_bit: u64, flags: u64) -> bool {
        let [(_, left), (_, right)] = self.sides();
        if flags & (left | right) != 0 {
            flags & side_bit != 0
        } else {
            side_bit == left && flags & self.flag() != 0
        }
    }
}

/// The reserved chord `key` completes under `flags`, if any (see the module
/// docs for the list and what is deliberately left out).
fn reserved_chord(key: u16, flags: u64) -> Option<Modifier> {
    match key {
        kvk::TAB | kvk::GRAVE | kvk::SPACE if flags & flag::COMMAND != 0 => Some(Modifier::Command),
        kvk::LEFT_ARROW | kvk::RIGHT_ARROW | kvk::DOWN_ARROW | kvk::UP_ARROW
            if flags & flag::CONTROL != 0 =>
        {
            Some(Modifier::Control)
        }
        _ => None,
    }
}

/// A modifier key whose key-down the filter forwarded itself.
#[derive(Debug, Clone, Copy)]
struct Synthesized {
    key: u16,
    modifier: Modifier,
    side_bit: u64,
}

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
    synthesized: Vec<Synthesized>,
}

impl ShortcutFilter {
    pub(crate) fn new() -> Self {
        // A handful of keys at most are ever held at once; reserving up front
        // keeps allocation out of the OS input callback.
        Self {
            swallowed: Vec::with_capacity(8),
            synthesized: Vec::with_capacity(4),
        }
    }

    /// Decide one keyboard event.
    ///
    /// * `focused` — the video surface has the keyboard right now.
    /// * `surface_holds(kVK)` — the surface already forwarded that key's down
    ///   (and so owns its release).
    pub(crate) fn on_event(
        &mut self,
        event: KeyEvent,
        key: u16,
        flags: u64,
        focused: bool,
        surface_holds: impl Fn(u16) -> bool,
        emit: &mut impl FnMut(u16, bool),
    ) -> Verdict {
        match event {
            KeyEvent::Down => self.on_down(key, flags, focused, surface_holds, emit),
            KeyEvent::Up => {
                let Some(i) = self.swallowed.iter().position(|&k| k == key) else {
                    return Verdict::Pass;
                };
                self.swallowed.remove(i);
                emit(key, false);
                Verdict::Swallow
            }
            KeyEvent::FlagsChanged => {
                self.synthesized.retain(|s| {
                    let still_down = s.modifier.side_is_down(s.side_bit, flags);
                    if !still_down {
                        emit(s.key, false);
                    }
                    still_down
                });
                Verdict::Pass
            }
        }
    }

    fn on_down(
        &mut self,
        key: u16,
        flags: u64,
        focused: bool,
        surface_holds: impl Fn(u16) -> bool,
        emit: &mut impl FnMut(u16, bool),
    ) -> Verdict {
        // Auto-repeat of a key we already took: keep taking it, whatever the
        // focus now, so the other app never sees a repeat without a press.
        if self.swallowed.contains(&key) {
            emit(key, true);
            return Verdict::Swallow;
        }
        if !focused {
            return Verdict::Pass;
        }
        let Some(modifier) = reserved_chord(key, flags) else {
            return Verdict::Pass;
        };
        for (side, side_bit) in modifier.sides() {
            if modifier.side_is_down(side_bit, flags)
                && !self.synthesized.iter().any(|s| s.key == side)
                && !surface_holds(side)
            {
                self.synthesized.push(Synthesized {
                    key: side,
                    modifier,
                    side_bit,
                });
                emit(side, true);
            }
        }
        self.swallowed.push(key);
        emit(key, true);
        Verdict::Swallow
    }

    /// Forward a release for everything still owed and forget it: the tap is
    /// going away, or it missed events (disabled by a timeout, Secure Input)
    /// and can no longer see the real releases. Chord keys go first, so the
    /// host never sees a bare modifier-up turn the chord into another one.
    pub(crate) fn release_all(&mut self, emit: &mut impl FnMut(u16, bool)) {
        for key in self.swallowed.drain(..).rev() {
            emit(key, false);
        }
        for s in self.synthesized.drain(..).rev() {
            emit(s.key, false);
        }
    }
}
