//! Windows implementation of `platform-api`: DXGI Desktop Duplication capture,
//! Media Foundation codecs, the D3D11 child-window surface, `SendInput`
//! injection, the `WH_KEYBOARD_LL` shortcut hook, the Win32 clipboard, DPAPI and
//! the per-user Run key.
//!
//! The media crates (`capture`, `codec`, `render`, `input`, `elevation`) keep
//! their Windows types; this crate adapts them to the OS-neutral traits. GPU
//! objects cross the trait boundary as the wrapper types in [`gpu`], which the
//! adapters here downcast — capture → encoder and decoder → presenter stay
//! zero-copy on one D3D11 device each.

#![cfg(windows)]

pub mod autostart;
pub mod clipboard;
pub mod secret;

mod adapters;
mod cpu;
mod displays;
pub mod gpu;

use std::path::PathBuf;
use std::sync::Arc;

use codec_openh264::{OpenH264, WithSoftwareH264};
use platform_api::{
    Autostart, CaptureError, Clipboard, CodecFactory, DisplayInfo, InputError, InputInjector,
    OsPermission, Paths, PermissionReport, Platform, PlatformInfo, RawDisplayHandle,
    RawWindowHandle, ScreenCapture, SecretStore, ShortcutHook, SurfaceError, VideoSurface,
};

/// The Windows platform. Cheap to construct; holds no OS resources itself.
pub struct WindowsPlatform {
    /// Media Foundation first; Cisco OpenH264 when MF has no usable H.264
    /// (e.g. Windows "N" editions without the Media Feature Pack). Desktop
    /// Duplication frames live on the GPU, so the software encoder reads them
    /// back through a staging texture.
    codecs: WithSoftwareH264<adapters::MfCodecFactory>,
    secrets: Arc<adapters::DpapiSecretStore>,
    autostart: Arc<adapters::RunKeyAutostart>,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        let openh264 = Arc::new(OpenH264::new(app_dir().join("codecs")));
        Self {
            codecs: WithSoftwareH264::new(adapters::MfCodecFactory, openh264)
                .with_readback(Box::new(cpu::D3d11Readback)),
            secrets: Arc::new(adapters::DpapiSecretStore),
            autostart: Arc::new(adapters::RunKeyAutostart),
        }
    }

    /// The Cisco OpenH264 handle behind the Settings → Advanced toggle.
    pub fn openh264(&self) -> Arc<OpenH264> {
        self.codecs.openh264().clone()
    }
}

impl Default for WindowsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

/// `%APPDATA%\ShareCtrlScreen` — the location every previous version used, so
/// existing config, recents and saved logins carry over.
fn app_dir() -> PathBuf {
    dirs::config_dir()
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("ShareCtrlScreen")
}

impl Platform for WindowsPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            name: "windows",
            start_at_login_label: "Start with Windows",
            shortcut_capture_hint: "Alt+Tab and the Windows key act on the remote machine \
                                    while the session window is focused. Ctrl+Alt+Del is \
                                    reserved by Windows and can never be captured.",
            login_screen_supported: true,
        }
    }

    fn paths(&self) -> Paths {
        let dir = app_dir();
        // The Known Folder, so a Downloads folder the user moved is respected.
        let downloads = dirs::download_dir()
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Downloads")))
            .unwrap_or_else(std::env::temp_dir)
            .join("ShareCtrlScreen");
        Paths {
            config_dir: dir.clone(),
            log_dir: dir,
            downloads_dir: downloads,
        }
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        displays::enumerate()
    }

    fn permissions(&self) -> PermissionReport {
        let mut report = PermissionReport::all_not_applicable();
        if !elevation::process_has_uiaccess() {
            report.notes.push(
                "Remote Alt+Tab and the Windows key only take effect on this machine \
                 when ShareCtrlScreen runs as the installed service."
                    .into(),
            );
        }
        report
    }

    fn request_permission(&self, _p: OsPermission) {}

    fn create_capture(
        &self,
        display: Option<&str>,
    ) -> Result<Box<dyn ScreenCapture>, CaptureError> {
        let output = match display {
            None => 0,
            Some(id) => displays::output_index(id).ok_or(CaptureError::NoOutput)?,
        };
        Ok(Box::new(adapters::DdaCapture::new(output)?))
    }

    fn codecs(&self) -> &dyn CodecFactory {
        &self.codecs
    }

    fn create_surface(
        &self,
        window: RawWindowHandle,
        _display: RawDisplayHandle,
    ) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
        let RawWindowHandle::Win32(h) = window else {
            return Err(SurfaceError::UnsupportedHandle);
        };
        Ok(Arc::new(adapters::ChildWindowSurface::create(
            h.hwnd.get(),
        )?))
    }

    fn create_injector(&self) -> Result<Box<dyn InputInjector>, InputError> {
        Ok(Box::new(adapters::SendInputInjector::new()))
    }

    fn create_shortcut_hook(&self) -> Option<Box<dyn ShortcutHook>> {
        Some(Box::new(adapters::LowLevelKeyHook::default()))
    }

    fn create_clipboard(&self) -> Box<dyn Clipboard> {
        Box::new(adapters::Win32Clipboard)
    }

    fn secrets(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }

    fn autostart(&self) -> Arc<dyn Autostart> {
        self.autostart.clone()
    }
}
