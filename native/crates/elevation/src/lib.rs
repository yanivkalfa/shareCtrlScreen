//! Elevation & the SYSTEM service (Plan 04 §8b). A LocalSystem Windows service
//! is mandatory to reach the secure desktop (UAC / lock / login), which "only
//! Windows processes can access". The app needs no code-signing certificate
//! (§8b/§11.4): the UIAccess that Alt+Tab / Windows-key passthrough needs is
//! given by the service as a TOKEN flag on the host it launches (W5,
//! [`uiaccess`]), not by a signed `uiAccess="true"` manifest.
//!
//! This crate provides the reusable primitives; the thin `service/` binary wires
//! them into the service dispatcher. Responsibilities:
//!   * detect the active session (`WTSGetActiveConsoleSessionId`),
//!   * get the user's primary token (`WTSQueryUserToken`, needs LocalSystem+SE_TCB),
//!   * launch the engine into the user session (`CreateProcessAsUser`,
//!     `lpDesktop = "winsta0\\default"`, `CreateEnvironmentBlock`), with the
//!     UIAccess token flag when it can be set,
//!   * follow the input desktop (`OpenInputDesktop` → `GetUserObjectInformation`
//!     → `SetThreadDesktop`) so the secure desktop / UAC prompt can be captured
//!     and clicked,
//!   * install / start / stop / uninstall / query the service (the NSIS
//!     installer's hooks drive these through `sharectrl-service.exe`).
//!
//! Honest limitations (documented, §8b): cannot silently bypass UAC (the local
//! user must click a real consent prompt if credentials are required); DRM stays
//! black; portable (non-installed) mode can elevate per-session but not reach the
//! secure desktop, and gets no UIAccess.

#![cfg(windows)]

pub mod desktop;
pub mod service;
pub mod session;
pub mod uiaccess;

pub use desktop::InputDesktopFollower;
pub use session::{active_console_session, launch_in_session, user_token, Launched, SessionError};
pub use uiaccess::{query_process_uiaccess, OwnedToken, UiAccessError};

/// Passed by the service to the host it launches into the user session, so
/// that host knows it is THE host of the session and other launches defer to
/// it (`platform_windows::instance`, task W4).
pub const SERVICE_HOST_FLAG: &str = "--service-host";

/// Encode a Rust `&str` as a NUL-terminated UTF-16 buffer for `PCWSTR`/`PWSTR`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether THIS process has the UIAccess token flag — set by the service when
/// it launches the host (W5), or by Windows for a signed `uiAccess="true"` exe
/// in a secure folder. Windows **silently ignores** injected shell shortcuts
/// (Alt+Tab, the Win key, the task switcher) from a process without it — an
/// anti-malware wall documented by Microsoft. A failed token query counts as
/// "no" here (as before); [`query_process_uiaccess`] keeps the error for the
/// host's startup line.
pub fn process_has_uiaccess() -> bool {
    matches!(query_process_uiaccess(), Ok(true))
}
