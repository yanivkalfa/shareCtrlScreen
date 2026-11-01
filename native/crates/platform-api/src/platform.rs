//! The one object the app constructs per OS, and the permission model.

use std::sync::Arc;

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::capture::{CaptureError, ScreenCapture};
use crate::codec::CodecFactory;
use crate::input::{InputError, InputInjector, ShortcutHook};
use crate::os::{Autostart, Clipboard, Paths, SecretStore};
use crate::surface::{SurfaceError, VideoSurface};

/// State of one OS permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    /// Not asked yet (macOS TCC "not determined", Wayland portal never shown).
    NotDetermined,
    /// This OS has no such permission.
    NotApplicable,
}

/// A permission the app may need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsPermission {
    /// macOS Screen Recording; Wayland ScreenCast portal.
    ScreenCapture,
    /// macOS Accessibility; Wayland RemoteDesktop portal; Linux uinput access.
    InputInjection,
    /// macOS Input Monitoring (viewer shortcut capture).
    InputMonitoring,
}

/// Everything the permissions preflight UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionReport {
    pub screen_capture: PermissionState,
    pub input_injection: PermissionState,
    pub input_monitoring: PermissionState,
    /// Human-readable caveats, e.g. "Hyprland has no remote-control portal;
    /// enable the uinput fallback in Settings", or the Windows UIAccess note.
    pub notes: Vec<String>,
}

impl PermissionReport {
    pub fn all_not_applicable() -> Self {
        Self {
            screen_capture: PermissionState::NotApplicable,
            input_injection: PermissionState::NotApplicable,
            input_monitoring: PermissionState::NotApplicable,
            notes: Vec::new(),
        }
    }
}

/// A display the host can share.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    /// Stable id stored in config `shareDisplayId`.
    pub id: String,
    pub name: String,
    /// Bounds in virtual-desktop pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical point (2.0 on Retina).
    pub scale: f64,
    pub primary: bool,
}

/// Per-OS facts and wording for the UI (replaces hard-coded Windows copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformInfo {
    /// `"windows"`, `"macos"`, `"linux-x11"`, `"linux-wayland"`.
    pub name: &'static str,
    /// Settings label for autostart, e.g. "Start at sign-in".
    pub start_at_login_label: &'static str,
    /// Hint under the shortcut-capture toggle.
    pub shortcut_capture_hint: &'static str,
    /// Whether the login window (nobody signed in) is reachable.
    pub login_screen_supported: bool,
}

/// Everything OS-specific, constructed once by the app.
///
/// Factories marked "call on the using thread" create objects that may hold
/// thread-bound OS resources.
pub trait Platform: Send + Sync {
    fn info(&self) -> PlatformInfo;
    fn paths(&self) -> Paths;
    fn displays(&self) -> Vec<DisplayInfo>;
    fn permissions(&self) -> PermissionReport;
    /// Show the OS prompt or open the settings pane for `p`. Non-blocking; the
    /// UI re-polls [`permissions`](Self::permissions).
    fn request_permission(&self, p: OsPermission);

    /// Call on the host media thread. `display` = a [`DisplayInfo::id`], `None`
    /// = primary.
    fn create_capture(&self, display: Option<&str>)
        -> Result<Box<dyn ScreenCapture>, CaptureError>;
    fn codecs(&self) -> &dyn CodecFactory;
    /// Call on the UI thread with the app window's handles.
    fn create_surface(
        &self,
        window: RawWindowHandle,
        display: RawDisplayHandle,
    ) -> Result<Arc<dyn VideoSurface>, SurfaceError>;
    /// Call on the host transport thread.
    fn create_injector(&self) -> Result<Box<dyn InputInjector>, InputError>;
    /// Call on the hook thread. `None` = this OS/session has no shortcut capture.
    fn create_shortcut_hook(&self) -> Option<Box<dyn ShortcutHook>>;
    /// Call on the thread that uses it.
    fn create_clipboard(&self) -> Box<dyn Clipboard>;
    fn secrets(&self) -> Arc<dyn SecretStore>;
    fn autostart(&self) -> Arc<dyn Autostart>;
}
