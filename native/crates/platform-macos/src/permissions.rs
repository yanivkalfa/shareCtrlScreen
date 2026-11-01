//! TCC permission preflight for macOS (`Platform::permissions` /
//! `Platform::request_permission`).
//!
//! Three TCC grants matter (`plans/01-MACOS-FINDINGS.md` §2): Screen
//! Recording, Accessibility and Input Monitoring. Each has a side-effect-free
//! probe and a request call. The `OsPermission`s the UI asks about are built
//! from them:
//!
//! | `OsPermission`    | needs                              | used by                  |
//! |-------------------|------------------------------------|--------------------------|
//! | `ScreenCapture`   | Screen Recording                   | host capture             |
//! | `InputInjection`  | Accessibility                      | host input injection     |
//! | `InputMonitoring` | Input Monitoring **+ Accessibility** | viewer shortcut capture |
//!
//! WHY shortcut capture needs Accessibility too: its event tap must *drop*
//! Cmd+Tab and friends, so it is an active tap, and macOS grants active
//! keyboard taps to Accessibility-trusted apps (Input Monitoring covers
//! listen-only taps; see `shortcut_hook.rs`). The viewer's preflight asks
//! only for `InputMonitoring`, so that row must cover both grants, or a user
//! who allowed everything the modal showed would still get no capture. A
//! combined permission reports the state of its first missing grant, and a
//! request works on that grant; the grants are bookkept separately, so
//! Accessibility asked for by either row counts as asked for both.
//!
//! ## Reporting `NotDetermined`
//!
//! None of the probes distinguishes "never asked" from "refused": all return
//! `false` for both. TCC's own database is not readable by an unprivileged
//! app. We therefore remember, per process, whether *we* already asked:
//! `false` before the first request is reported as `NotDetermined`, `false`
//! after it as `Denied`. A grant refused in an earlier run therefore reads
//! `NotDetermined` once; the first request is then silent (macOS never
//! re-prompts a refused app), the next poll reads `Denied`, and the following
//! request opens the Settings pane, which is the only place it can be changed.
//!
//! ## Request flow
//!
//! First request per grant: the native prompt. Any later request while still
//! not granted: the System Settings pane for that grant (the prompt cannot be
//! shown twice). The call never blocks; the UI re-polls, and a combined
//! permission moves on to its next missing grant on the following request.
//!
//! ## References
//!
//! * `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`:
//!   <https://developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess()>
//!   (macOS 10.15+; the request returns `false` while the prompt is up and
//!   the grant only takes effect after the app restarts).
//! * `CGPreflightListenEventAccess` / `CGRequestListenEventAccess`:
//!   <https://developer.apple.com/documentation/coregraphics/cgpreflightlisteneventaccess()>
//! * `AXIsProcessTrustedWithOptions` and `kAXTrustedCheckOptionPrompt`:
//!   <https://developer.apple.com/documentation/applicationservices/1459186-axisprocesstrustedwithoptions>
//!   (with the prompt option it shows the system dialog and returns at once).
//! * Active event taps need Accessibility, listen-only taps Input Monitoring:
//!   Apple Developer Forums thread 735204,
//!   <https://developer.apple.com/forums/thread/735204>
//! * `IsSecureEventInputEnabled` (Carbon HIToolbox, `CarbonEventsCore.h`):
//!   <https://developer.apple.com/documentation/carbon/1462033-issecureeventinputenabled>
//! * `NSWorkspace.open(_:)`:
//!   <https://developer.apple.com/documentation/appkit/nsworkspace/1533463-open>
//! * Settings deep links `x-apple.systempreferences:com.apple.preference.security?Privacy_*`:
//!   `plans/01-MACOS-FINDINGS.md` §2 (still honoured by System Settings on
//!   macOS 13 to 15).
//! * Sequoia monthly screen-recording re-approval:
//!   <https://9to5mac.com/2024/08/14/macos-sequoia-screen-recording-prompt-monthly/>

use std::sync::atomic::{AtomicBool, Ordering};

use objc2_app_kit::NSWorkspace;
use objc2_application_services::{kAXTrustedCheckOptionPrompt, AXIsProcessTrustedWithOptions};
use objc2_core_foundation::{kCFBooleanTrue, CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{
    CGPreflightListenEventAccess, CGPreflightScreenCaptureAccess, CGRequestListenEventAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_foundation::{NSString, NSURL};
use platform_api::{OsPermission, PermissionReport, PermissionState};

const SETTINGS_SCREEN_CAPTURE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const SETTINGS_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const SETTINGS_INPUT_MONITORING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

const NOTE_SECURE_INPUT: &str = "Secure Input is active (a password field or a terminal's \
    \"Secure Keyboard Entry\" has the keyboard), so shortcut capture cannot work until it ends.";
const NOTE_SCREEN_RECORDING_RENEWAL: &str = "macOS 15 and later asks every month to re-approve \
    screen recording. A Mac left unattended stops sharing until someone clicks Allow.";
const NOTE_SHORTCUTS_NEED_ACCESSIBILITY: &str = "Keyboard shortcuts need two switches in System \
    Settings → Privacy & Security: Input Monitoring and Accessibility.";

// Carbon has no objc2 binding. This one call is the documented way to learn
// whether a process has enabled Secure Event Input; it takes no arguments.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// Whether this process already asked for each grant. TCC is per process
/// identity, so process-wide state is the correct scope.
static ASKED_SCREEN_RECORDING: AtomicBool = AtomicBool::new(false);
static ASKED_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);
static ASKED_INPUT_MONITORING: AtomicBool = AtomicBool::new(false);

/// One TCC grant (a row in System Settings → Privacy & Security).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
}

/// The grants an `OsPermission` needs, in the order they are requested (see
/// the module docs for why shortcut capture needs two).
fn grants(p: OsPermission) -> &'static [Grant] {
    match p {
        OsPermission::ScreenCapture => &[Grant::ScreenRecording],
        OsPermission::InputInjection => &[Grant::Accessibility],
        OsPermission::InputMonitoring => &[Grant::InputMonitoring, Grant::Accessibility],
    }
}

/// What `request_permission` does for one grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestStep {
    /// Already granted; nothing to do.
    None,
    /// First ask: let macOS show its own prompt.
    NativePrompt,
    /// Asked before and still not granted: macOS will not prompt again.
    OpenSettings,
}

/// Maps a probe result to the reported state (see the module docs).
fn state_of(granted: bool, asked: bool) -> PermissionState {
    match (granted, asked) {
        (true, _) => PermissionState::Granted,
        (false, false) => PermissionState::NotDetermined,
        (false, true) => PermissionState::Denied,
    }
}

fn step_for(granted: bool, asked: bool) -> RequestStep {
    match (granted, asked) {
        (true, _) => RequestStep::None,
        (false, false) => RequestStep::NativePrompt,
        (false, true) => RequestStep::OpenSettings,
    }
}

/// The first grant of `needed` that `granted` says is missing: the one the
/// permission's state is reported from, and the one a request works on.
fn first_missing(needed: &[Grant], granted: impl Fn(Grant) -> bool) -> Option<Grant> {
    needed.iter().copied().find(|&g| !granted(g))
}

/// State of a permission built from `needed` grants.
fn combined_state(
    needed: &[Grant],
    granted: impl Fn(Grant) -> bool,
    asked: impl Fn(Grant) -> bool,
) -> PermissionState {
    match first_missing(needed, granted) {
        None => PermissionState::Granted,
        Some(g) => state_of(false, asked(g)),
    }
}

/// `shortcuts_granted`: both grants of `InputMonitoring` are in place. Until
/// they are, say that the one "Keyboard shortcuts" row stands for two
/// switches, so a user fixing it in System Settings by hand flips both.
fn notes(secure_input: bool, shortcuts_granted: bool) -> Vec<String> {
    let mut notes = vec![NOTE_SCREEN_RECORDING_RENEWAL.to_owned()];
    if !shortcuts_granted {
        notes.push(NOTE_SHORTCUTS_NEED_ACCESSIBILITY.to_owned());
    }
    if secure_input {
        notes.push(NOTE_SECURE_INPUT.to_owned());
    }
    notes
}

fn asked_flag(g: Grant) -> &'static AtomicBool {
    match g {
        Grant::ScreenRecording => &ASKED_SCREEN_RECORDING,
        Grant::Accessibility => &ASKED_ACCESSIBILITY,
        Grant::InputMonitoring => &ASKED_INPUT_MONITORING,
    }
}

fn settings_url(g: Grant) -> &'static str {
    match g {
        Grant::ScreenRecording => SETTINGS_SCREEN_CAPTURE,
        Grant::Accessibility => SETTINGS_ACCESSIBILITY,
        Grant::InputMonitoring => SETTINGS_INPUT_MONITORING,
    }
}

/// Whether every grant `p` needs is in place. Side-effect-free probe: never
/// shows a prompt (the injector checks `InputInjection` with it).
pub(crate) fn is_granted(p: OsPermission) -> bool {
    first_missing(grants(p), grant_given).is_none()
}

/// Side-effect-free probe of one grant: never shows a prompt.
fn grant_given(g: Grant) -> bool {
    match g {
        Grant::ScreenRecording => CGPreflightScreenCaptureAccess(),
        Grant::Accessibility => accessibility_trusted(false),
        Grant::InputMonitoring => CGPreflightListenEventAccess(),
    }
}

/// `AXIsProcessTrustedWithOptions`; `prompt` adds the system dialog that
/// offers to open the Accessibility pane.
pub(crate) fn accessibility_trusted(prompt: bool) -> bool {
    if !prompt {
        // SAFETY: a null options dictionary is documented as "no options".
        return unsafe { AXIsProcessTrustedWithOptions(None) };
    }
    let Some(yes) = (unsafe { kCFBooleanTrue }) else {
        // The constant is only absent if CoreFoundation failed to load.
        tracing::warn!("kCFBooleanTrue unavailable; cannot prompt for Accessibility");
        return false;
    };
    // SAFETY: the extern static is a valid, immortal CFString.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[yes]);
    // SAFETY: keys are CFStrings and values CFBooleans, the types the
    // function documents for its options dictionary.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

/// Opens the Settings pane for `g`. Failure is logged, not returned: the
/// `Platform` API is fire-and-forget and the UI keeps showing the state.
fn open_settings(g: Grant) {
    let url = settings_url(g);
    let opened = NSURL::URLWithString(&NSString::from_str(url))
        .is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url));
    if !opened {
        tracing::warn!(url, "could not open the System Settings pane");
    }
}

/// Whether some process has Secure Event Input on (a password field, a
/// terminal's "Secure Keyboard Entry"). While it is, event taps see no key
/// events at all, system-wide.
pub(crate) fn secure_input_active() -> bool {
    // SAFETY: argument-free query with no preconditions.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

pub(crate) fn report() -> PermissionReport {
    let asked = |g: Grant| asked_flag(g).load(Ordering::Relaxed);
    let state = |p| combined_state(grants(p), grant_given, asked);
    let input_monitoring = state(OsPermission::InputMonitoring);
    PermissionReport {
        screen_capture: state(OsPermission::ScreenCapture),
        input_injection: state(OsPermission::InputInjection),
        input_monitoring,
        notes: notes(
            secure_input_active(),
            input_monitoring == PermissionState::Granted,
        ),
    }
}

pub(crate) fn request(p: OsPermission) {
    let Some(g) = first_missing(grants(p), grant_given) else {
        return;
    };
    // `swap` makes concurrent callers agree on who is first.
    let asked = asked_flag(g).swap(true, Ordering::Relaxed);
    match step_for(false, asked) {
        RequestStep::None => {}
        RequestStep::OpenSettings => open_settings(g),
        RequestStep::NativePrompt => match g {
            Grant::ScreenRecording => {
                CGRequestScreenCaptureAccess();
            }
            Grant::Accessibility => {
                accessibility_trusted(true);
            }
            Grant::InputMonitoring => {
                CGRequestListenEventAccess();
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_wins_regardless_of_history() {
        assert_eq!(state_of(true, false), PermissionState::Granted);
        assert_eq!(state_of(true, true), PermissionState::Granted);
    }

    #[test]
    fn not_granted_is_undetermined_until_we_asked() {
        assert_eq!(state_of(false, false), PermissionState::NotDetermined);
        assert_eq!(state_of(false, true), PermissionState::Denied);
    }

    #[test]
    fn request_prompts_once_then_opens_settings() {
        assert_eq!(step_for(false, false), RequestStep::NativePrompt);
        assert_eq!(step_for(false, true), RequestStep::OpenSettings);
        assert_eq!(step_for(true, false), RequestStep::None);
        assert_eq!(step_for(true, true), RequestStep::None);
    }

    #[test]
    fn shortcut_capture_needs_input_monitoring_and_accessibility() {
        let needed = grants(OsPermission::InputMonitoring);
        assert_eq!(needed, [Grant::InputMonitoring, Grant::Accessibility]);
        let never = |_| false;
        // Input Monitoring allowed, Accessibility not: still not granted, and
        // the request moves on to Accessibility.
        let only_listen = |g| g == Grant::InputMonitoring;
        assert_eq!(
            combined_state(needed, only_listen, never),
            PermissionState::NotDetermined
        );
        assert_eq!(
            first_missing(needed, only_listen),
            Some(Grant::Accessibility)
        );
        // Accessibility already asked for (e.g. by the host's preflight).
        let asked_ax = |g| g == Grant::Accessibility;
        assert_eq!(
            combined_state(needed, only_listen, asked_ax),
            PermissionState::Denied
        );
        assert_eq!(
            combined_state(needed, |_| true, never),
            PermissionState::Granted
        );
        assert_eq!(first_missing(needed, never), Some(Grant::InputMonitoring));
    }

    #[test]
    fn the_host_permissions_need_one_grant_each() {
        assert_eq!(
            grants(OsPermission::ScreenCapture),
            [Grant::ScreenRecording]
        );
        assert_eq!(grants(OsPermission::InputInjection), [Grant::Accessibility]);
    }

    #[test]
    fn deep_links_name_the_right_pane() {
        assert!(settings_url(Grant::ScreenRecording).ends_with("Privacy_ScreenCapture"));
        assert!(settings_url(Grant::Accessibility).ends_with("Privacy_Accessibility"));
        assert!(settings_url(Grant::InputMonitoring).ends_with("Privacy_ListenEvent"));
    }

    #[test]
    fn secure_input_note_only_when_active() {
        assert!(!notes(false, true)
            .iter()
            .any(|n| n.contains("Secure Input")));
        assert!(notes(true, true).iter().any(|n| n.contains("Secure Input")));
    }

    #[test]
    fn the_two_switch_note_shows_until_shortcuts_are_granted() {
        assert!(notes(false, false)
            .iter()
            .any(|n| n.contains("Accessibility")));
        assert!(!notes(false, true)
            .iter()
            .any(|n| n.contains("Accessibility")));
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn report_on_a_real_mac_does_not_panic() {
        let r = report();
        assert_ne!(r.screen_capture, PermissionState::NotApplicable);
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn accessibility_probe_never_prompts() {
        // Run from a fresh account: no dialog may appear.
        let _ = accessibility_trusted(false);
    }
}
