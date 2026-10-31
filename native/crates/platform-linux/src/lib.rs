//! Linux implementation of `platform-api` (plan lane L: portal ScreenCast +
//! PipeWire or XShm/XDamage capture, VA-API/NVENC codecs, an X11 child window or
//! Wayland subsurface, XTest / portal-libei / uinput injection, Secret Service,
//! XDG autostart).
//!
//! Status: scaffold. Paths and session-type detection are real; every other
//! capability fails safely with a clear message until its task lands (see
//! `plans/05-TASKS.md`, lane L).

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

mod autostart;

use platform_api::unavailable::{NoAutostart, NoClipboard, NoCodecs, NoSecretStore};
use platform_api::{
    Autostart, CaptureError, Clipboard, CodecFactory, DisplayInfo, InputError, InputInjector,
    OsPermission, Paths, PermissionReport, PermissionState, Platform, PlatformInfo,
    RawDisplayHandle, RawWindowHandle, ScreenCapture, SecretStore, ShortcutHook, SurfaceError,
    VideoSurface,
};

const NOT_YET: &str = "not available on Linux in this version";

/// The display server this session runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    X11,
    Wayland,
}

impl SessionType {
    /// From the environment the desktop session sets. Wayland wins when both are
    /// present (XWayland exports `DISPLAY` too, but root-window capture under
    /// rootless XWayland only sees X clients).
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        )
    }

    fn from_env(wayland_display: bool, xdg_session_type: Option<&str>) -> Self {
        match xdg_session_type {
            Some("wayland") => SessionType::Wayland,
            Some("x11") if !wayland_display => SessionType::X11,
            _ if wayland_display => SessionType::Wayland,
            _ => SessionType::X11,
        }
    }
}

pub struct LinuxPlatform {
    session: SessionType,
    codecs: NoCodecs,
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self {
            session: SessionType::detect(),
            codecs: NoCodecs { reason: NOT_YET },
        }
    }

    pub fn session_type(&self) -> SessionType {
        self.session
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

impl Platform for LinuxPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            name: match self.session {
                SessionType::X11 => "linux-x11",
                SessionType::Wayland => "linux-wayland",
            },
            start_at_login_label: "Start at login",
            shortcut_capture_hint: "System shortcuts act on the remote machine while the \
                                    session window is focused, where the desktop allows it.",
            login_screen_supported: false,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            config_dir: dirs::config_dir()
                .unwrap_or_else(|| home().join(".config"))
                .join("sharectrl"),
            log_dir: dirs::state_dir()
                .unwrap_or_else(|| home().join(".local/state"))
                .join("sharectrl"),
            downloads_dir: dirs::download_dir()
                .unwrap_or_else(|| home().join("Downloads"))
                .join("ShareCtrlScreen"),
        }
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }

    fn permissions(&self) -> PermissionReport {
        PermissionReport {
            screen_capture: PermissionState::NotDetermined,
            input_injection: PermissionState::NotDetermined,
            input_monitoring: PermissionState::NotApplicable,
            notes: vec!["Screen sharing on Linux is not available in this version.".into()],
        }
    }

    fn request_permission(&self, _p: OsPermission) {}

    fn create_capture(
        &self,
        _display: Option<&str>,
    ) -> Result<Box<dyn ScreenCapture>, CaptureError> {
        Err(CaptureError::Backend(format!("screen capture {NOT_YET}")))
    }

    fn codecs(&self) -> &dyn CodecFactory {
        &self.codecs
    }

    fn create_surface(
        &self,
        _window: RawWindowHandle,
        _display: RawDisplayHandle,
    ) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
        Err(SurfaceError::Backend(format!("remote display {NOT_YET}")))
    }

    fn create_injector(&self) -> Result<Box<dyn InputInjector>, InputError> {
        Err(InputError::Unavailable(format!("remote control {NOT_YET}")))
    }

    fn create_shortcut_hook(&self) -> Option<Box<dyn ShortcutHook>> {
        None
    }

    fn create_clipboard(&self) -> Box<dyn Clipboard> {
        Box::new(NoClipboard)
    }

    fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::new(NoSecretStore)
    }

    fn autostart(&self) -> Arc<dyn Autostart> {
        match autostart::XdgAutostart::for_current_user() {
            Ok(entry) => Arc::new(entry),
            Err(e) => {
                tracing::warn!("start at login unavailable: {e}");
                Arc::new(NoAutostart {
                    reason: "start at login is unavailable: this program's file path could not be used (see app.log)",
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_type_detection() {
        assert_eq!(
            SessionType::from_env(true, Some("wayland")),
            SessionType::Wayland
        );
        assert_eq!(SessionType::from_env(false, Some("x11")), SessionType::X11);
        // XWayland exports DISPLAY and some launchers set x11 wrongly.
        assert_eq!(
            SessionType::from_env(true, Some("x11")),
            SessionType::Wayland
        );
        assert_eq!(SessionType::from_env(true, None), SessionType::Wayland);
        assert_eq!(SessionType::from_env(false, None), SessionType::X11);
        assert_eq!(SessionType::from_env(false, Some("tty")), SessionType::X11);
    }

    #[test]
    fn paths_are_per_user_and_distinct() {
        let p = LinuxPlatform::new().paths();
        assert!(p.config_dir.ends_with("sharectrl"));
        assert!(p.downloads_dir.ends_with("ShareCtrlScreen"));
        assert_ne!(p.config_dir, p.downloads_dir);
    }
}
