//! macOS implementation of `platform-api` (plan lane M: ScreenCaptureKit,
//! VideoToolbox, an NSView + Metal surface, CGEvent injection and event taps,
//! NSPasteboard, Keychain, LaunchAgent).
//!
//! Status: screen capture and display enumeration (M1), clipboard (M7),
//! Keychain (M8), autostart (M9), permissions (M11) and paths are real; every other
//! capability fails safely with a clear message until its task lands (see
//! `plans/05-TASKS.md`, lane M).

#![cfg(target_os = "macos")]

mod autostart;
mod capture;
mod convert;
mod cursor;
mod display_id;
mod displays;
pub mod gpu;
mod permissions;
mod slot;

pub use capture::SckCapture;

use std::path::PathBuf;
use std::sync::Arc;

use platform_api::unavailable::{NoAutostart, NoCodecs};
use platform_api::{
    Autostart, CaptureError, Clipboard, CodecFactory, DisplayInfo, InputError, InputInjector,
    OsPermission, Paths, PermissionReport, Platform, PlatformInfo, RawDisplayHandle,
    RawWindowHandle, ScreenCapture, SecretStore, ShortcutHook, SurfaceError, VideoSurface,
};

mod clipboard;
mod secret;

const NOT_YET: &str = "not available on macOS in this version";

pub struct MacPlatform {
    codecs: NoCodecs,
}

impl MacPlatform {
    pub fn new() -> Self {
        Self {
            codecs: NoCodecs { reason: NOT_YET },
        }
    }
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

impl Platform for MacPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            name: "macos",
            start_at_login_label: "Start at login",
            shortcut_capture_hint: "Cmd+Tab and other system shortcuts act on the remote \
                                    machine while the session window is focused (needs the \
                                    Accessibility permission).",
            login_screen_supported: false,
        }
    }

    fn paths(&self) -> Paths {
        let support = dirs::config_dir()
            .unwrap_or_else(|| home().join("Library/Application Support"))
            .join("ShareCtrlScreen");
        Paths {
            config_dir: support,
            log_dir: home().join("Library/Logs/ShareCtrlScreen"),
            downloads_dir: dirs::download_dir()
                .unwrap_or_else(|| home().join("Downloads"))
                .join("ShareCtrlScreen"),
        }
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        displays::enumerate()
    }

    fn permissions(&self) -> PermissionReport {
        permissions::report()
    }

    fn request_permission(&self, p: OsPermission) {
        permissions::request(p);
    }

    fn create_capture(
        &self,
        display: Option<&str>,
    ) -> Result<Box<dyn ScreenCapture>, CaptureError> {
        Ok(Box::new(SckCapture::new(display)?))
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
        Box::new(clipboard::MacClipboard)
    }

    fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::new(secret::KeychainSecretStore)
    }

    fn autostart(&self) -> Arc<dyn Autostart> {
        match autostart::LaunchAgent::for_current_user() {
            Ok(agent) => Arc::new(agent),
            Err(_) => Arc::new(NoAutostart {
                reason: "start at login is unavailable: this program's path or home folder could not be found",
            }),
        }
    }
}
