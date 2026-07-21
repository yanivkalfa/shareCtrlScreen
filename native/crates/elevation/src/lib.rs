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
