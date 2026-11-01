//! macOS implementation of `platform-api` (plan lane M: ScreenCaptureKit,
//! VideoToolbox, an NSView + Metal surface, CGEvent injection and event taps,
//! NSPasteboard, Keychain, LaunchAgent).
//!
//! Status: screen capture and display enumeration (M1), VideoToolbox encode
//! (M2) and decode (M3), the video surface and presenter (M4), input
//! injection (M5), clipboard (M7), Keychain (M8), autostart (M9),
//! permissions (M11) and paths are real; every other
//! capability fails safely with a clear message until its task lands (see
//! `plans/05-TASKS.md`, lane M).

#![cfg(target_os = "macos")]

mod autostart;
mod avcc;
mod capture;
mod codecs;
mod convert;
mod cursor;
mod decode;
mod display_id;
mod displays;
mod focus;
pub mod gpu;
mod import;
mod inject_state;
mod injector;
pub mod keymap;
mod permissions;
mod presenter;
mod slot;
mod surface;
mod view;
mod vt_encoder;
mod vt_params;

pub use capture::SckCapture;

use std::path::PathBuf;
use std::sync::Arc;

use codec_openh264::{OpenH264, WithSoftwareH264};
use codecs::VtCodecs;
use platform_api::unavailable::NoAutostart;
use platform_api::{
    Autostart, CaptureError, Clipboard, CodecFactory, DisplayInfo, InputError, InputInjector,
    OsPermission, Paths, PermissionReport, Platform, PlatformInfo, RawDisplayHandle,
    RawWindowHandle, ScreenCapture, SecretStore, ShortcutHook, SurfaceError, VideoSurface,
};

mod clipboard;
mod secret;

pub struct MacPlatform {
    /// VideoToolbox encode (M2) and decode (M3); Cisco OpenH264 for H.264
    /// wherever VideoToolbox refuses it — for decoding, when
    /// `VtCodecs::new_decoder`'s probe session cannot be created.
    codecs: WithSoftwareH264<VtCodecs>,
}

impl MacPlatform {
    pub fn new() -> Self {
        let openh264 = Arc::new(OpenH264::new(config_dir().join("codecs")));
        let hardware = VtCodecs::new();
        hardware.start_probe();
        Self {
            codecs: WithSoftwareH264::new(hardware, openh264),
        }
    }

    /// The Cisco OpenH264 handle behind the Settings → Advanced toggle.
    pub fn openh264(&self) -> Arc<OpenH264> {
        self.codecs.openh264().clone()
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

/// `~/Library/Application Support/ShareCtrlScreen`.
fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home().join("Library/Application Support"))
        .join("ShareCtrlScreen")
}

impl Platform for MacPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            name: "macos",
            start_at_login_label: "Start at sign-in",
            shortcut_capture_hint: "Cmd+Tab and other system shortcuts act on the remote \
                                    machine while the session window is focused (needs the \
                                    Accessibility permission).",
            login_screen_supported: false,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            config_dir: config_dir(),
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
        window: RawWindowHandle,
        _display: RawDisplayHandle,
    ) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
        Ok(surface::MacVideoSurface::create(window)?)
    }

    fn create_injector(&self) -> Result<Box<dyn InputInjector>, InputError> {
        Ok(Box::new(injector::CgInjector::new()?))
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
