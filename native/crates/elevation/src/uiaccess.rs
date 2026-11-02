//! The UIAccess token flag for the host the service starts (task W5).
//!
//! Windows refuses synthesized shell shortcuts — Alt+Tab, the Windows key, the
//! task switcher — from a process without UIAccess, and UIPI stops it driving
//! windows of a higher integrity level. Windows grants UIAccess in two ways:
//! the AppInfo service, for a signed `uiAccess="true"` exe in a secure folder
//! (not used here — that is the separate signed-build card), or a token that
//! already carries the flag when the process is created. The SYSTEM service
//! holds SeTcbPrivilege, so before it calls `CreateProcessAsUserW` it sets
//! `TokenUIAccess = 1` on a duplicate of the signed-in user's token. This is
//! the token route Windows uses itself when it launches its own assistive tools
//! for a user; it gives the host nothing beyond UIAccess (same user, same
//! integrity level, same session) and changes nothing about who may connect.
//!
//! Every step can fail on a hardened or misconfigured machine. A failure is
//! reported as a [`UiAccessError`] that says what to check, and the service
//! then launches the host WITHOUT the flag: losing Alt+Tab passthrough is
//! acceptable, a host that does not start is not.
//!
//! ## References
//! * `SetTokenInformation` —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-settokeninformation>.
//!   The page does not list per-class requirements. That setting
//!   `TokenUIAccess` needs SeTcbPrivilege ("Act as part of the operating
//!   system") is NOT stated on any Microsoft page we found; it is the kernel's
//!   known behaviour (the call fails with `ERROR_PRIVILEGE_NOT_HELD` without
//!   it), and `plans/checklists/W5.md` confirms it on a device. Writing a token
//!   default needs `TOKEN_ADJUST_DEFAULT` on the handle, which is one reason we
//!   write to our own duplicate.
//! * `TOKEN_INFORMATION_CLASS` / `TokenUIAccess` ("a DWORD value that is
//!   nonzero if the token has the UIAccess flag set"); `TokenSessionId` on the
//!   same page documents the TCB requirement for its own class —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_information_class>.
//! * `GetTokenInformation` —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-gettokeninformation>.
//! * `AdjustTokenPrivileges` (returns success with `ERROR_NOT_ALL_ASSIGNED`
//!   when the token does not hold the privilege) —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-adjusttokenprivileges>;
//!   "Enabling and Disabling Privileges in C++" —
//!   <https://learn.microsoft.com/en-us/windows/win32/secauthz/enabling-and-disabling-privileges-in-c-->.
//! * `DuplicateTokenEx` —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-duplicatetokenex>.
//! * `CreateProcessAsUserW` —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw>.
//! * "Windows Integrity Mechanism Design" → "User Interface Privilege Isolation
//!   (UIPI) and integrity" and "UIAccess for UI automation applications": the
//!   USER subsystem reads the process's PRIMARY TOKEN when the process first
//!   calls into USER and fixes its UI privilege then; a process launched with
//!   UIAccess rights can set the foreground window, drive any window with
//!   `SendInput` and set journal hooks —
//!   <https://learn.microsoft.com/en-us/previous-versions/dotnet/articles/bb625963(v=msdn.10)>.
//!   That the runtime check is the token flag (the manifest only asks AppInfo
//!   to set that flag) follows from this; that the shell's Alt+Tab / Start
//!   handling honours a flag set this way is not spelled out by Microsoft and
//!   is a device-checklist item.
//! * "Security Considerations for Assistive Technologies" (what UIAccess
//!   grants; it never reaches UI at system integrity such as the secure
//!   desktop, which remains the service's own job) —
//!   <https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview>.

use windows::core::HRESULT;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, ERROR_PRIVILEGE_NOT_HELD, HANDLE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW,
    SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenUIAccess, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, SE_TCB_NAME, TOKEN_ACCESS_MASK, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Why the service could not give the host the UIAccess flag. Each message
/// says what to check, because it ends up in the service log of a machine the
/// owner may be debugging remotely.
#[derive(Debug, thiserror::Error)]
pub enum UiAccessError {
    #[error(
        "could not open the service's own token to enable SeTcbPrivilege ({0}); \
         the service must run as LocalSystem"
    )]
    OpenServiceToken(windows::core::Error),
    #[error("could not look up SeTcbPrivilege on this machine ({0})")]
    LookupPrivilege(windows::core::Error),
    #[error(
        "the service account does not hold SeTcbPrivilege (\"Act as part of the operating \
         system\"); the service must run as LocalSystem — check that \
         `sc qc ShareCtrlScreenHost` shows SERVICE_START_NAME LocalSystem"
    )]
    TcbNotHeld,
    #[error("could not enable SeTcbPrivilege on the service token ({0})")]
    EnablePrivilege(windows::core::Error),
    #[error("could not duplicate the signed-in user's token ({0})")]
    DuplicateToken(windows::core::Error),
    #[error("Windows refused to set TokenUIAccess on the user's token ({0})")]
    SetFlag(windows::core::Error),
}

/// Map a `SetTokenInformation(TokenUIAccess)` failure to the error that says
/// what to fix: "privilege not held" means the TCB step did not take effect
/// (the account lacks it), anything else is reported verbatim.
pub(crate) fn classify_set_flag_error(e: windows::core::Error) -> UiAccessError {
    if e.code() == HRESULT::from_win32(ERROR_PRIVILEGE_NOT_HELD.0) {
        UiAccessError::TcbNotHeld
    } else {
        UiAccessError::SetFlag(e)
    }
}

/// A token handle closed on drop, so no early return leaks it (the service runs
/// for weeks and relaunches the host on every session change).
#[derive(Debug)]
pub struct OwnedToken(HANDLE);

impl OwnedToken {
    /// Take ownership of `handle`; it is closed when the value drops. The
    /// caller must own the handle and not close it elsewhere.
    pub fn from_raw(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// The raw handle, valid while `self` lives.
    pub fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedToken {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open/duplicate and is
        // closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Whether `token` carries the UIAccess flag (`GetTokenInformation`,
/// `TokenUIAccess`, a DWORD). The handle needs `TOKEN_QUERY`.
pub fn token_has_uiaccess(token: HANDLE) -> windows::core::Result<bool> {
    let mut ui_access: u32 = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: the buffer is a u32 of the size passed; `ret_len` is a valid out
    // param; the caller guarantees the handle.
    unsafe {
        GetTokenInformation(
            token,
            TokenUIAccess,
            Some(&mut ui_access as *mut u32 as *mut core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            &mut ret_len,
        )?;
    }
    Ok(ui_access != 0)
}

/// Whether THIS process's primary token carries the UIAccess flag, or the
/// error the token query returned.
pub fn query_process_uiaccess() -> windows::core::Result<bool> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle of the current process needs no closing; the
    // token handle is owned (and closed) by `OwnedToken`.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
    let token = OwnedToken(token);
    token_has_uiaccess(token.handle())
}

/// Enable SeTcbPrivilege in the service's own process token. LocalSystem
/// holds it, but a privilege must be ENABLED for the kernel to honour it, and
/// whether it is enabled by default in a service token is not something to rely
/// on. Idempotent; it is left enabled (the service is LocalSystem for its whole
/// life and already relies on TCB for `WTSQueryUserToken`).
fn enable_tcb_privilege() -> Result<(), UiAccessError> {
    let mut own = HANDLE::default();
    // SAFETY: pseudo-handle of the current process; out param initialized.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut own,
        )
    }
    .map_err(UiAccessError::OpenServiceToken)?;
    let own = OwnedToken(own);

    let mut luid = LUID::default();
    // SAFETY: SE_TCB_NAME is a static NUL-terminated string; `luid` is valid.
    unsafe { LookupPrivilegeValueW(None, SE_TCB_NAME, &mut luid) }
        .map_err(UiAccessError::LookupPrivilege)?;

    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `privileges` is a complete one-entry TOKEN_PRIVILEGES; no
    // previous state is requested.
    unsafe { AdjustTokenPrivileges(own.handle(), false, Some(&privileges), 0, None, None) }
        .map_err(UiAccessError::EnablePrivilege)?;
    // WHY GetLastError after a success: AdjustTokenPrivileges reports a
    // privilege the token does not hold as SUCCESS with ERROR_NOT_ALL_ASSIGNED
    // (documented), so the return value alone would hide the real cause.
    // SAFETY: plain thread-local error read, directly after the call.
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(UiAccessError::TcbNotHeld);
    }
    Ok(())
}

/// The access the launch needs on the duplicate: `CreateProcessAsUserW` wants
/// QUERY + DUPLICATE + ASSIGN_PRIMARY, `CreateEnvironmentBlock` wants QUERY +
/// IMPERSONATE + DUPLICATE, and writing `TokenUIAccess` wants ADJUST_DEFAULT.
/// Nothing more (least privilege).
const LAUNCH_TOKEN_ACCESS: TOKEN_ACCESS_MASK = TOKEN_ACCESS_MASK(
    TOKEN_QUERY.0
        | TOKEN_DUPLICATE.0
        | TOKEN_ASSIGN_PRIMARY.0
        | TOKEN_IMPERSONATE.0
        | TOKEN_ADJUST_DEFAULT.0,
);

/// Duplicate `user_token` into a new primary token and try to set the
/// UIAccess flag on the duplicate.
///
/// Returns the duplicate (or `None` when even the duplication failed — the
/// caller then launches with `user_token` unchanged) and the outcome of the
/// flag. WHY a duplicate: the flag must not leak into the token the caller
/// still holds, and `WTSQueryUserToken` does not document which access rights
/// its handle carries, while our duplicate has exactly [`LAUNCH_TOKEN_ACCESS`].
/// The duplicate keeps the session id, user, groups and integrity level of the
/// original, so the launched host is the same user in the same session as it
/// would be without the flag.
pub fn primary_token_with_uiaccess(
    user_token: HANDLE,
) -> (Option<OwnedToken>, Result<(), UiAccessError>) {
    let mut dup = HANDLE::default();
    // SAFETY: the caller guarantees `user_token`; `dup` is an out param owned
    // by `OwnedToken` on success.
    let duplicated = unsafe {
        DuplicateTokenEx(
            user_token,
            LAUNCH_TOKEN_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        )
    };
    if let Err(e) = duplicated {
        return (None, Err(UiAccessError::DuplicateToken(e)));
    }
    let dup = OwnedToken(dup);
    let flagged = enable_tcb_privilege().and_then(|()| set_uiaccess_flag(dup.handle()));
    (Some(dup), flagged)
}

fn set_uiaccess_flag(token: HANDLE) -> Result<(), UiAccessError> {
    let on: u32 = 1;
    // SAFETY: the buffer is a DWORD of the size passed; `token` has
    // TOKEN_ADJUST_DEFAULT (LAUNCH_TOKEN_ACCESS).
    unsafe {
        SetTokenInformation(
            token,
            TokenUIAccess,
            &on as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .map_err(classify_set_flag_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_not_held_points_at_the_service_account() {
        let e = windows::core::Error::from(HRESULT::from_win32(ERROR_PRIVILEGE_NOT_HELD.0));
        let mapped = classify_set_flag_error(e);
        assert!(matches!(mapped, UiAccessError::TcbNotHeld));
        let text = mapped.to_string();
        assert!(text.contains("SeTcbPrivilege"), "{text}");
        assert!(text.contains("LocalSystem"), "{text}");
    }

    #[test]
    fn any_other_refusal_keeps_the_os_error() {
        let e = windows::core::Error::from(HRESULT::from_win32(
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.0,
        ));
        let mapped = classify_set_flag_error(e);
        assert!(matches!(mapped, UiAccessError::SetFlag(_)));
        assert!(mapped.to_string().contains("TokenUIAccess"));
    }

    /// A test run from a shell is not an assistive-technology launch, so the
    /// query must succeed and report no flag. (This is also the path the host's
    /// `--print-uiaccess` takes when the user starts it.)
    #[test]
    fn a_plain_process_has_no_uiaccess() {
        assert!(matches!(query_process_uiaccess(), Ok(false)));
    }

    /// Without TCB (an ordinary test process), duplicating our own token works
    /// but setting the flag is refused — and the refusal is the actionable
    /// error, never a panic. This is the service's fallback path in miniature.
    #[test]
    fn without_tcb_the_flag_is_refused_with_a_reason() {
        let own = {
            let mut t = HANDLE::default();
            // SAFETY: pseudo-handle of the current process; out param valid.
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut t) }
                .expect("open own token");
            OwnedToken(t)
        };
        let (dup, flagged) = primary_token_with_uiaccess(own.handle());
        assert!(dup.is_some(), "duplication of our own token must work");
        let err = flagged.expect_err("an unprivileged process cannot set UIAccess");
        assert!(!err.to_string().is_empty());
    }

    /// The token step end to end, without launching anything: duplicate the
    /// active console user's token, set the flag, read it back, and check the
    /// caller's token is untouched.
    ///
    /// Needs LocalSystem (for `WTSQueryUserToken` and SeTcbPrivilege) and a
    /// signed-in console user, so it is ignored by default. To run it on a
    /// Windows machine: build the test binary normally (the lane), then from an
    /// elevated prompt run that binary as SYSTEM with Sysinternals `psexec -s`
    /// (or a one-shot scheduled task with `/ru SYSTEM`), passing
    /// `--ignored --nocapture` and redirecting its output to a file you can
    /// read afterwards. `plans/checklists/W5.md` has the exact steps.
    #[test]
    #[ignore = "needs the installed service"]
    fn system_can_flag_the_console_users_token() {
        let session =
            crate::session::active_console_session().expect("a signed-in console session");
        let user = OwnedToken(crate::session::user_token(session).expect("run as LocalSystem"));
        let (dup, flagged) = primary_token_with_uiaccess(user.handle());
        let dup = dup.expect("duplicate token");
        if let Err(e) = flagged {
            panic!("flag not set: {e}");
        }
        assert!(matches!(token_has_uiaccess(dup.handle()), Ok(true)));
        assert!(matches!(token_has_uiaccess(user.handle()), Ok(false)));
    }
}
