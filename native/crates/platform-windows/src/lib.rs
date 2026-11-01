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
//!
//! ## H.264 selection policy
//!
//! Hardware Media Foundation codec first; then Microsoft's software Media
//! Foundation codec (what machines without a GPU video decoder use); then Cisco
//! OpenH264, only if the user has it turned on. [`WindowsPlatform::h264_problem`]
//! reports, in words a user can act on, the one case where none of the three
//! exists (a Windows "N" edition without the Media Feature Pack and OpenH264
//! off, unsupported or not downloadable).
//!
//! ## References
//!
//! * `MFTEnumEx`, `_MFT_ENUM_FLAG` (hardware and synchronous-MFT flags may be
//!   combined): <https://learn.microsoft.com/windows/win32/api/mfapi/nf-mfapi-mftenumex>,
//!   <https://learn.microsoft.com/windows/win32/api/mfapi/ne-mfapi-_mft_enum_flag>
//! * H.264 video decoder / encoder MFTs, and their absence on "N" editions
//!   without the Media Feature Pack:
//!   <https://learn.microsoft.com/windows/win32/medfound/h-264-video-decoder>,
//!   <https://learn.microsoft.com/windows/win32/medfound/h-264-video-encoder>,
//!   <https://support.microsoft.com/windows/media-feature-pack-for-windows-n-editions>

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

use codec_openh264::{NativeH264, OpenH264, WithSoftwareH264};

/// GPU→CPU readback of Desktop Duplication frames for the software encoder;
/// public so tools that build their own OpenH264 factory (the smoketest's
/// `--software`) can feed it real captures.
pub use cpu::D3d11Readback;
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
                .with_readback(Box::new(cpu::D3d11Readback))
                .with_native_probe(Box::new(|| NativeH264 {
                    encode: codec::encode::can_encode_any(codec::Codec::H264),
                    decode: codec::decode::can_decode_any(codec::Codec::H264),
                })),
            secrets: Arc::new(adapters::DpapiSecretStore),
            autostart: Arc::new(adapters::RunKeyAutostart),
        }
    }

    /// The Cisco OpenH264 handle behind the Settings → Advanced toggle.
    pub fn openh264(&self) -> Arc<OpenH264> {
        self.codecs.openh264().clone()
    }

    /// Why this computer cannot host and/or view for lack of any H.264 codec,
    /// or `None`. See the module docs for the policy.
    pub fn h264_problem(&self) -> Option<String> {
        self.codecs.h264_problem()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine-B check (no GPU H.264 decoder): such a computer must advertise
    /// H.264 and report no "no codec" problem, because Microsoft's software
    /// decoder (or a downloaded OpenH264) covers it. Run on that machine with
    /// the ignored tests of this crate enabled, then view a real 1080p session
    /// from machine A and confirm at least 30 fps.
    #[test]
    #[ignore = "needs no-HW-decoder machine"]
    fn h264_is_usable_without_a_hardware_decoder() {
        let platform = WindowsPlatform::new();
        assert_eq!(platform.h264_problem(), None);
        assert!(platform
            .codecs()
            .viewer_decodable()
            .contains(&platform_api::Codec::H264));
    }
}
