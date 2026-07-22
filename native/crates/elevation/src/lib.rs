//! Elevation & the SYSTEM service (Plan 04 §8b). A LocalSystem Windows service
//! is mandatory to reach the secure desktop (UAC / lock / login), which "only
//! Windows processes can access" — UIAccess is deliberately NOT used, so the app
//! needs no code-signing certificate (§8b/§11.4).
//!
//! This crate provides the reusable primitives; the thin `service/` binary wires
//! them into the service dispatcher. Responsibilities:
//!   * detect the active session (`WTSGetActiveConsoleSessionId`),
//!   * get the user's primary token (`WTSQueryUserToken`, needs LocalSystem+SE_TCB),
//!   * launch the engine into the user session (`CreateProcessAsUser`,
//!     `lpDesktop = "winsta0\\default"`, `CreateEnvironmentBlock`),
//!   * follow the input desktop (`OpenInputDesktop` → `GetUserObjectInformation`
//!     → `SetThreadDesktop`) so the secure desktop / UAC prompt can be captured
//!     and clicked,
//!   * install / uninstall the service.
//!
//! Honest limitations (documented, §8b): cannot silently bypass UAC (the local
//! user must click a real consent prompt if credentials are required); DRM stays
//! black; portable (non-installed) mode can elevate per-session but not reach the
//! secure desktop.

#![cfg(windows)]

pub mod desktop;
pub mod service;
pub mod session;

pub use desktop::InputDesktopFollower;
pub use session::{active_console_session, launch_in_session, user_token, SessionError};

/// Encode a Rust `&str` as a NUL-terminated UTF-16 buffer for `PCWSTR`/`PWSTR`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether THIS process has the UIAccess privilege (manifest `uiAccess="true"` +
/// signed + trusted install location). Windows **silently ignores** injected
/// shell shortcuts (Alt+Tab, the Win key, the task switcher) from a process
/// without it — an anti-malware wall documented by Microsoft. So the shortcut
/// hook must only run when this is true: otherwise it captures those combos
/// locally and forwards keystrokes the host can never act on, which just risks
/// stuck modifiers for zero benefit.
pub fn process_has_uiaccess() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenUIAccess, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard token query; handle closed before return.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut ui_access: u32 = 0;
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenUIAccess,
            Some(&mut ui_access as *mut _ as *mut _),
            std::mem::size_of::<u32>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && ui_access != 0
    }
}
