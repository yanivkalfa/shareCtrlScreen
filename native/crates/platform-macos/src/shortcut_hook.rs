//! `impl ShortcutHook` for macOS: a Quartz event tap that, while the video
//! view has the keyboard, takes the shortcuts macOS would act on itself
//! (Cmd+Tab, Cmd+\`, Cmd+Space, Ctrl+arrows) and sends them to the remote
//! instead. Which events are taken, and what is forwarded, is decided by the
//! pure [`ShortcutFilter`]; this module is the OS plumbing around it.
//!
//! ## The tap
//! An **active** tap (`kCGEventTapOptionDefault`: it may drop events) at the
//! **session** level (`kCGSessionEventTap`: every key event of this login
//! session, before the Dock, Spotlight or any app sees it), inserted at the
//! **head** (`kCGHeadInsertEventTap`: before other session taps, so a key
//! remapper's tap cannot act on a chord we hand to the remote). Mask: key
//! down, key up and flagsChanged. Returning `NULL` from the callback drops the
//! event; returning it unchanged passes it on.
//!
//! ## Threading and lifetime
//! The trait runs `install`, `run` and `uninstall` on one dedicated thread.
//! The tap's run-loop source is added to **that** thread's `CFRunLoop`, so the
//! callback only ever runs inside `run`, on that thread — which is why the
//! callback state needs no lock, and why `uninstall` can free it once the
//! port is invalidated: no callback can be running or start afterwards. The
//! Core Foundation handles are not `Send`, so neither is the hook: it cannot
//! be dropped on another thread by mistake.
//!
//! ## Never block the input path
//! The callback reads one atomic (the view's keyboard focus), may *try* the
//! surface's input mutex (never waiting for it) and calls the forwarder (a
//! channel send). macOS disables a tap whose callback is slow
//! (`kCGEventTapDisabledByTimeout`); the callback re-enables it at once, and a
//! once-a-second health check in `run` catches a tap disabled without notice
//! (seen after re-signing, see References). Whenever the tap may have missed
//! events — disabled, or Secure Input turned on — every release still owed to
//! the host is sent, so no key stays down there.
//!
//! ## Guaranteed escapes
//! * Focus: the view publishes `SurfaceState::has_key_focus` from the main
//!   thread on every focus change (first responder, key window, hidden), and
//!   nothing is taken without it. Clicking another window or app clears it
//!   before the next key event arrives.
//! * Force Quit (Cmd+Option+Esc) is never taken, even if our main thread
//!   hangs with the flag still set.
//! * Accessibility revoked while the tap runs: macOS may then block input,
//!   and `AXIsProcessTrusted` keeps answering `true`. The health check
//!   therefore probes by creating a throwaway active tap (`NULL` = no longer
//!   permitted) and removes ours at once — the best an app can do; whether
//!   that alone unblocks input is checked on a device (checklist M6 §5.4).
//!
//! ## Permission and Secure Input
//! An active keyboard tap needs **Accessibility** (Input Monitoring covers
//! listen-only taps). `install` checks it up front and returns `false` with
//! the reason in the log. Secure Input does not stop `install`: a tap created
//! under it simply sees no keys until it ends, and `run` logs the pause and
//! the resume.
//!
//! ## References
//! * `CGEventTapCreate`: <https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)>
//!   and `CGEvent.h` (returns `NULL` when the process may not tap; the tap is
//!   inert until its port is added to a run loop and enabled).
//! * `CGEventTapCallBack` (return `NULL` to delete the event):
//!   <https://developer.apple.com/documentation/coregraphics/cgeventtapcallback>
//! * `CGEventTapLocation.cgSessionEventTap`, `CGEventTapPlacement.headInsertEventTap`,
//!   `CGEventTapOptions.defaultTap`: <https://developer.apple.com/documentation/coregraphics/cgeventtaplocation>,
//!   <https://developer.apple.com/documentation/coregraphics/cgeventtapplacement>,
//!   <https://developer.apple.com/documentation/coregraphics/cgeventtapoptions>
//! * `kCGEventTapDisabledByTimeout` / `kCGEventTapDisabledByUserInput`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventtype/tapdisabledbytimeout>
//! * `CGEventTapEnable`, `CGEventTapIsEnabled`:
//!   <https://developer.apple.com/documentation/coregraphics/cgevent/tapenable(tap:enable:)>
//! * `kCGKeyboardEventKeycode`, `kCGEventSourceUserData`, `CGEventGetFlags`:
//!   <https://developer.apple.com/documentation/coregraphics/cgeventfield>,
//!   <https://developer.apple.com/documentation/coregraphics/cgeventflags>
//! * `CFMachPortCreateRunLoopSource`, `CFMachPortInvalidate`:
//!   <https://developer.apple.com/documentation/corefoundation/cfmachport>
//! * `CFRunLoopAddSource`, `CFRunLoopRunInMode`, `CFRunLoopSourceInvalidate`
//!   (removes the source from every mode of every run loop):
//!   <https://developer.apple.com/documentation/corefoundation/cfrunloop>
//! * `IsSecureEventInputEnabled`, `AXIsProcessTrustedWithOptions`: see
//!   `permissions.rs`.
//! * Active taps need Accessibility, listen-only taps Input Monitoring; a
//!   grant revoked under a running active tap may block input (the thread
//!   reports it lasting until a restart), and probing with a throwaway tap
//!   is the reliable detector (`AXIsProcessTrusted` stays `true`): Apple
//!   Developer Forums thread 735204,
//!   <https://developer.apple.com/forums/thread/735204>
//! * Taps silently disabled after re-signing; health-check and re-enable:
//!   <https://danielraffel.me/til/2026/02/19/cgevent-taps-and-code-signing-the-silent-disable-race/>
//! * The Windows hook this ports: `input/src/keyhook.rs`.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFMachPort, CFRetained, CFRunLoop, CFRunLoopRunResult, CFRunLoopSource,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};
use platform_api::{KeyForwarder, ShortcutHook, VideoSurface};
use surface_wgpu::SurfaceState;

use crate::inject_state::INJECT_SENTINEL;
use crate::shortcut_filter::{KeyEvent, ShortcutFilter, Verdict};
use crate::surface::MacVideoSurface;
use crate::{keymap, permissions};

/// How long one `run` turn waits for events before re-checking `stop`.
const POLL: Duration = Duration::from_millis(50);
/// How often `run` checks the tap's health (enabled, still permitted, Secure
/// Input). Each check creates and destroys a probe tap, so not every turn.
const HEALTH_EVERY: Duration = Duration::from_secs(1);

const REASON_ACCESSIBILITY: &str = "shortcut capture is off: ShareCtrlScreen does not have the \
    Accessibility permission. Turn it on in System Settings → Privacy & Security → \
    Accessibility, then reconnect.";
const PAUSED_SECURE_INPUT: &str = "shortcut capture paused: Secure Input is active (a password \
    field, or a terminal's \"Secure Keyboard Entry\", has the keyboard), so macOS hides every key \
    from other apps. It resumes by itself when that field closes or the option is turned off.";
const REASON_TAP_REFUSED: &str = "shortcut capture is off: macOS refused the keyboard tap. In \
    System Settings → Privacy & Security, make sure ShareCtrlScreen is on under both \
    Accessibility and Input Monitoring; if it already is, switch each off and on again (an app \
    update can leave a stale entry), then reconnect.";
const REASON_REVOKED: &str = "shortcut capture stopped: the Accessibility permission was turned \
    off. Turn it back on in System Settings → Privacy & Security → Accessibility, then reconnect.";

fn event_bit(t: CGEventType) -> CGEventMask {
    1 << t.0
}

/// Key down, key up and modifier changes. The tap-disabled notifications are
/// delivered whatever the mask.
fn key_events() -> CGEventMask {
    event_bit(CGEventType::KeyDown)
        | event_bit(CGEventType::KeyUp)
        | event_bit(CGEventType::FlagsChanged)
}

/// `kCFRunLoopDefaultMode`: the only mode our dedicated thread ever runs.
fn default_mode() -> Option<&'static objc2_core_foundation::CFRunLoopMode> {
    // SAFETY: an immutable Core Foundation constant, set at load time.
    unsafe { kCFRunLoopDefaultMode }
}

/// Everything the tap callback needs. Boxed so its address — the callback's
/// `user_info` — never moves; freed only after the tap's port is invalidated.
struct TapContext {
    state: Arc<SurfaceState>,
    forward: KeyForwarder,
    /// Only the tap thread touches it (see the module docs); `RefCell` turns a
    /// would-be re-entrant use into a pass-through instead of a data race.
    filter: RefCell<ShortcutFilter>,
    /// The tap itself, so the callback can re-enable it. Set once, before
    /// the run-loop source exists, so before any callback can run.
    port: OnceCell<CFRetained<CFMachPort>>,
}

impl TapContext {
    fn forward_key(&self, key: u16, down: bool) {
        // Every key the filter can emit has a DOM code (keymap test below);
        // one that had none would mean nothing to the host anyway.
        if let Some(code) = keymap::dom_code(key) {
            (self.forward)(code, down);
        }
    }

    /// Whether the video view already sent this key's down to the host.
    ///
    /// Never waits: if the UI thread holds the input lock right now, answer
    /// "not held". The worst case is a duplicate modifier-down on the host
    /// (a harmless repeat); waiting inside the OS input callback is not.
    fn surface_holds(&self, key: u16) -> bool {
        keymap::dom_code(key).is_some_and(|code| {
            self.state
                .try_input()
                .is_some_and(|input| input.is_key_held(code))
        })
    }

    /// Send every release still owed (the tap missed, or will miss, them).
    fn release_owed(&self) {
        if let Ok(mut filter) = self.filter.try_borrow_mut() {
            filter.release_all(&mut |key, down| self.forward_key(key, down));
        }
    }

    /// The OS turned the tap off; turn it back on and settle what it missed.
    fn reenable(&self, why: &str) {
        if let Some(port) = self.port.get() {
            CGEvent::tap_enable(port, true);
        }
        self.release_owed();
        tracing::warn!("shortcut tap was disabled by macOS ({why}); re-enabled");
    }

    fn handle(&self, event_type: CGEventType, event: &CGEvent) -> Verdict {
        let kind = match event_type {
            CGEventType::KeyDown => KeyEvent::Down,
            CGEventType::KeyUp => KeyEvent::Up,
            CGEventType::FlagsChanged => KeyEvent::FlagsChanged,
            CGEventType::TapDisabledByTimeout => {
                self.reenable("callback timeout");
                return Verdict::Pass;
            }
            CGEventType::TapDisabledByUserInput => {
                self.reenable("user input");
                return Verdict::Pass;
            }
            _ => return Verdict::Pass,
        };
        // Our own injector's events (a host on this same Mac) are not the
        // user's keys; taking them would feed the remote its own input.
        if CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
            == INJECT_SENTINEL
        {
            return Verdict::Pass;
        }
        let raw_key = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
        let Ok(key) = u16::try_from(raw_key) else {
            return Verdict::Pass;
        };
        let flags = CGEvent::flags(Some(event)).0;
        let focused = self.state.has_key_focus();
        let Ok(mut filter) = self.filter.try_borrow_mut() else {
            return Verdict::Pass;
        };
        filter.on_event(
            kind,
            key,
            flags,
            focused,
            |k| self.surface_holds(k),
            &mut |k, down| self.forward_key(k, down),
        )
    }
}

/// The tap callback. Any doubt — no context, a panic — passes the event on:
/// a bug here must never eat the user's keyboard.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let pass = event.as_ptr();
    if user_info.is_null() {
        return pass;
    }
    // SAFETY: `user_info` is the boxed `TapContext` of the installed tap. It
    // outlives every callback: callbacks run only on the tap thread inside
    // `run`, and `uninstall` (same thread) invalidates the port before
    // freeing the box.
    let ctx = unsafe { &*user_info.cast::<TapContext>() };
    // SAFETY: Quartz passes a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    match catch_unwind(AssertUnwindSafe(|| ctx.handle(event_type, event_ref))) {
        Ok(Verdict::Swallow) => std::ptr::null_mut(),
        Ok(Verdict::Pass) | Err(_) => pass,
    }
}

/// Create an active session tap on `mask`. `user_info` must be null or a
/// `TapContext` that outlives the tap.
fn create_tap(
    place: CGEventTapPlacement,
    mask: CGEventMask,
    user_info: *mut c_void,
) -> Option<CFRetained<CFMachPort>> {
    // SAFETY: `tap_callback` has the exact `CGEventTapCallBack` signature and
    // accepts a null `user_info`; the caller guarantees a non-null one stays
    // valid while the tap exists.
    unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            place,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            user_info,
        )
    }
}

/// Whether this process may still create an active keyboard tap. The only
/// reliable live check: `AXIsProcessTrusted` keeps answering `true` after
/// the grant is removed. The probe is never added to a run loop, so it never
/// sees an event, and it is destroyed at once.
fn may_filter_keys() -> bool {
    match create_tap(
        CGEventTapPlacement::TailAppendEventTap,
        event_bit(CGEventType::KeyDown),
        std::ptr::null_mut(),
    ) {
        Some(probe) => {
            probe.invalidate();
            true
        }
        None => false,
    }
}

struct InstalledTap {
    /// Declared before `ctx`, and invalidated explicitly in `uninstall`
    /// before `ctx` is freed.
    source: CFRetained<CFRunLoopSource>,
    ctx: Box<TapContext>,
    /// Secure Input as last seen by the health check.
    secure_input: Cell<bool>,
}

/// The macOS shortcut hook (see the module docs).
#[derive(Default)]
pub(crate) struct EventTapHook {
    tap: Option<InstalledTap>,
}

impl EventTapHook {
    /// Re-enable a tap macOS turned off without telling us, follow Secure
    /// Input, and detect a revoked permission. `false` = the tap must go.
    fn check_health(tap: &InstalledTap) -> bool {
        if !may_filter_keys() {
            tracing::warn!("{REASON_REVOKED}");
            return false;
        }
        let secure = permissions::secure_input_active();
        if secure != tap.secure_input.replace(secure) {
            if secure {
                // The tap sees no keys from now on, so it would miss the
                // releases of anything held.
                tap.ctx.release_owed();
                tracing::info!("{PAUSED_SECURE_INPUT}");
            } else {
                tracing::info!("shortcut capture resumed: Secure Input ended");
            }
        }
        if let Some(port) = tap.ctx.port.get() {
            if !CGEvent::tap_is_enabled(port) {
                tap.ctx.reenable("found disabled by the health check");
                if !CGEvent::tap_is_enabled(port) {
                    tracing::warn!("{REASON_TAP_REFUSED}");
                }
            }
        }
        true
    }
}

impl ShortcutHook for EventTapHook {
    fn install(&mut self, surface: &dyn VideoSurface, forward: KeyForwarder) -> bool {
        self.uninstall();
        let Some(surface) = surface.as_any().downcast_ref::<MacVideoSurface>() else {
            tracing::warn!("shortcut capture is off: the video surface is not the macOS view");
            return false;
        };
        // Checked before creating anything: without it macOS refuses the tap,
        // and this message says why where a NULL tap could not.
        if !permissions::accessibility_trusted(false) {
            tracing::warn!("{REASON_ACCESSIBILITY}");
            return false;
        }
        let Some(mode) = default_mode() else {
            tracing::warn!("shortcut capture is off: Core Foundation's run-loop mode is missing");
            return false;
        };
        let Some(run_loop) = CFRunLoop::current() else {
            tracing::warn!("shortcut capture is off: this thread has no run loop");
            return false;
        };
        let ctx = Box::new(TapContext {
            state: surface.shared_state().clone(),
            forward,
            filter: RefCell::new(ShortcutFilter::new()),
            port: OnceCell::new(),
        });
        let user_info: *const TapContext = &*ctx;
        let Some(port) = create_tap(
            CGEventTapPlacement::HeadInsertEventTap,
            key_events(),
            user_info.cast_mut().cast(),
        ) else {
            tracing::warn!("{REASON_TAP_REFUSED}");
            return false;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            port.invalidate();
            tracing::warn!("shortcut capture is off: could not attach the keyboard tap");
            return false;
        };
        // Before the source joins the run loop: from then on callbacks may run.
        let port = ctx.port.get_or_init(|| port);
        run_loop.add_source(Some(&source), Some(mode));
        CGEvent::tap_enable(port, true);
        tracing::info!("shortcut capture on: Cmd+Tab, Cmd+`, Cmd+Space and Ctrl+arrows go to the remote while its window has the keyboard");
        // WHY install anyway under Secure Input: it is often on only briefly
        // (a password manager, a background Terminal with Secure Keyboard
        // Entry). The tap exists but sees no keys until it ends, and the
        // health check logs the resume — refusing here would lose capture for
        // the whole session.
        let secure_input = permissions::secure_input_active();
        if secure_input {
            tracing::info!("{PAUSED_SECURE_INPUT}");
        }
        self.tap = Some(InstalledTap {
            source,
            ctx,
            secure_input: Cell::new(secure_input),
        });
        true
    }

    fn run(&mut self, stop: &AtomicBool) {
        let Some(mode) = default_mode() else {
            return;
        };
        let mut next_check = Instant::now() + HEALTH_EVERY;
        while !stop.load(Ordering::SeqCst) {
            let Some(tap) = &self.tap else {
                return;
            };
            let result = CFRunLoop::run_in_mode(Some(mode), POLL.as_secs_f64(), false);
            if result == CFRunLoopRunResult::Finished {
                // No source in the mode (cannot happen while installed); never
                // spin on it.
                std::thread::sleep(POLL);
            }
            if Instant::now() >= next_check {
                next_check = Instant::now() + HEALTH_EVERY;
                if !Self::check_health(tap) {
                    self.uninstall();
                    return;
                }
            }
        }
    }

    fn uninstall(&mut self) {
        let Some(tap) = self.tap.take() else {
            return;
        };
        if let Some(port) = tap.ctx.port.get() {
            CGEvent::tap_enable(port, false);
            // Removes the source from every run loop: no callback can start.
            tap.source.invalidate();
            port.invalidate();
        }
        // The host must not be left holding what we pressed for it.
        tap.ctx.release_owed();
        tracing::info!("shortcut capture off");
    }
}

impl Drop for EventTapHook {
    fn drop(&mut self) {
        self.uninstall();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcut_filter::kvk;

    #[test]
    fn every_key_the_filter_forwards_has_a_dom_code() {
        for (key, dom) in [
            (kvk::TAB, "Tab"),
            (kvk::SPACE, "Space"),
            (kvk::GRAVE, "Backquote"),
            (kvk::COMMAND, "MetaLeft"),
            (kvk::RIGHT_COMMAND, "MetaRight"),
            (kvk::CONTROL, "ControlLeft"),
            (kvk::RIGHT_CONTROL, "ControlRight"),
            (kvk::LEFT_ARROW, "ArrowLeft"),
            (kvk::RIGHT_ARROW, "ArrowRight"),
            (kvk::DOWN_ARROW, "ArrowDown"),
            (kvk::UP_ARROW, "ArrowUp"),
        ] {
            assert_eq!(keymap::dom_code(key), Some(dom), "{key:#04x}");
        }
    }

    #[test]
    fn the_mask_is_exactly_the_three_key_events() {
        assert_eq!(key_events(), (1 << 10) | (1 << 11) | (1 << 12));
    }

    /// Run by hand on a Mac (plans/checklists/M6.md §5), from a terminal
    /// that has the Accessibility grant: `cargo test -p platform-macos --lib
    /// -- --ignored probe`. Must return promptly either way, never hang.
    #[test]
    #[ignore = "needs a Mac with a logged-in GUI session"]
    fn probe_tap_answers_without_hanging() {
        let started = Instant::now();
        let allowed = may_filter_keys();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "allowed={allowed}"
        );
    }
}
