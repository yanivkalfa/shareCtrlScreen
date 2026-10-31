//! TCC permission preflight for macOS (`Platform::permissions` /
//! `Platform::request_permission`).
//!
//! Three grants matter (`plans/01-MACOS-FINDINGS.md` §2): Screen Recording
//! (host capture), Accessibility (host input injection, viewer shortcut taps)
//! and Input Monitoring (viewer taps on 10.15+). Each has a side-effect-free
//! probe and a request call.
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
//! First request per permission: the native prompt. Any later request while
//! still not granted: the System Settings pane for that permission (the
//! prompt cannot be shown twice). The call never blocks; the UI re-polls.
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

// Carbon has no objc2 binding. This one call is the documented way to learn
// whether a process has enabled Secure Event Input; it takes no arguments.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// Whether this process already asked for each permission. TCC is per process
/// identity, so process-wide state is the correct scope.
static ASKED_SCREEN_CAPTURE: AtomicBool = AtomicBool::new(false);
static ASKED_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);
static ASKED_INPUT_MONITORING: AtomicBool = AtomicBool::new(false);

/// What `request_permission` does for one permission.
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

fn notes(secure_input: bool) -> Vec<String> {
    let mut notes = vec![NOTE_SCREEN_RECORDING_RENEWAL.to_owned()];
    if secure_input {
        notes.push(NOTE_SECURE_INPUT.to_owned());
    }
    notes
}

fn asked_flag(p: OsPermission) -> &'static AtomicBool {
    match p {
        OsPermission::ScreenCapture => &ASKED_SCREEN_CAPTURE,
        OsPermission::InputInjection => &ASKED_ACCESSIBILITY,
        OsPermission::InputMonitoring => &ASKED_INPUT_MONITORING,
    }
}

fn settings_url(p: OsPermission) -> &'static str {
    match p {
        OsPermission::ScreenCapture => SETTINGS_SCREEN_CAPTURE,
        OsPermission::InputInjection => SETTINGS_ACCESSIBILITY,
        OsPermission::InputMonitoring => SETTINGS_INPUT_MONITORING,
    }
}

/// Side-effect-free probe: never shows a prompt.
fn is_granted(p: OsPermission) -> bool {
    match p {
        OsPermission::ScreenCapture => CGPreflightScreenCaptureAccess(),
        OsPermission::InputInjection => accessibility_trusted(false),
        OsPermission::InputMonitoring => CGPreflightListenEventAccess(),
    }
}

/// `AXIsProcessTrustedWithOptions`; `prompt` adds the system dialog that
/// offers to open the Accessibility pane.
fn accessibility_trusted(prompt: bool) -> bool {
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

/// Opens the Settings pane for `p`. Failure is logged, not returned: the
/// `Platform` API is fire-and-forget and the UI keeps showing the state.
fn open_settings(p: OsPermission) {
    let url = settings_url(p);
    let opened = NSURL::URLWithString(&NSString::from_str(url))
        .is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url));
    if !opened {
        tracing::warn!(url, "could not open the System Settings pane");
    }
}

pub(crate) fn report() -> PermissionReport {
    let state = |p| state_of(is_granted(p), asked_flag(p).load(Ordering::Relaxed));
    // SAFETY: argument-free query with no preconditions.
    let secure_input = unsafe { IsSecureEventInputEnabled() } != 0;
    PermissionReport {
        screen_capture: state(OsPermission::ScreenCapture),
        input_injection: state(OsPermission::InputInjection),
        input_monitoring: state(OsPermission::InputMonitoring),
        notes: notes(secure_input),
    }
}

pub(crate) fn request(p: OsPermission) {
    // `swap` makes concurrent callers agree on who is first.
    let asked = asked_flag(p).swap(true, Ordering::Relaxed);
    match step_for(is_granted(p), asked) {
        RequestStep::None => {}
        RequestStep::OpenSettings => open_settings(p),
        RequestStep::NativePrompt => match p {
            OsPermission::ScreenCapture => {
                CGRequestScreenCaptureAccess();
            }
            OsPermission::InputInjection => {
                accessibility_trusted(true);
            }
            OsPermission::InputMonitoring => {
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
    fn deep_links_name_the_right_pane() {
        assert!(settings_url(OsPermission::ScreenCapture).ends_with("Privacy_ScreenCapture"));
        assert!(settings_url(OsPermission::InputInjection).ends_with("Privacy_Accessibility"));
        assert!(settings_url(OsPermission::InputMonitoring).ends_with("Privacy_ListenEvent"));
    }

    #[test]
    fn secure_input_note_only_when_active() {
        assert_eq!(notes(false).len(), 1);
        assert!(notes(true).iter().any(|n| n.contains("Secure Input")));
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
