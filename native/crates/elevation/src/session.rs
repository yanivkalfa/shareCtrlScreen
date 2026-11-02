//! Session detection, user-token acquisition, and launching the engine into the
//! active user session (Plan 04 §8b). Runs in the LocalSystem service context.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use crate::uiaccess::{primary_token_with_uiaccess, UiAccessError};
use crate::wide;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("no active console session")]
    NoSession,
}

/// The active console session id, or `None` if nobody is logged in at the
/// physical console (0xFFFFFFFF). Distinguishes console from RDP by using the
/// *console* session specifically (§8b).
pub fn active_console_session() -> Option<u32> {
    // SAFETY: no arguments; returns 0xFFFFFFFF if there is no session.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == 0xFFFF_FFFF {
        None
    } else {
        Some(id)
    }
}

/// The primary access token of the user in `session_id` (`WTSQueryUserToken`,
/// requires LocalSystem + `SE_TCB_NAME`). Caller owns the handle.
pub fn user_token(session_id: u32) -> Result<HANDLE, SessionError> {
    let mut token = HANDLE::default();
    // SAFETY: out param initialized; token owned by caller on success.
    unsafe { WTSQueryUserToken(session_id, &mut token)? };
    Ok(token)
}

/// A host the service started.
#[derive(Debug)]
pub struct Launched {
    pub pid: u32,
    /// `Ok` when the host was started with the UIAccess token flag; otherwise
    /// why not. The host was started either way.
    pub ui_access: Result<(), UiAccessError>,
}

/// Launch `exe_path` (with `args`) into the user's session on
/// `winsta0\\default`, inheriting the user's environment (§8b
/// `CreateProcessAsUser` hand-off). The service calls this to start the engine
/// and re-calls it whenever the active session changes. `token` stays owned by
/// the caller and is not modified.
///
/// The process is created from a duplicate of `token` carrying the UIAccess
/// flag (W5, [`crate::uiaccess`]) so remote Alt+Tab and the Windows key reach
/// the shell. INVARIANT: a failure of that step never stops the launch — the
/// host then starts exactly as before, without the flag, and
/// [`Launched::ui_access`] says why. Command line, desktop, environment block,
/// working directory (inherited from the service, as before) and creation flags
/// are the same with or without the flag.
pub fn launch_in_session(
    token: HANDLE,
    exe_path: &str,
    args: &str,
) -> Result<Launched, SessionError> {
    let (flagged_token, ui_access) = primary_token_with_uiaccess(token);
    // The duplicate is used even when only the flag failed: it is the same
    // user/session/groups as `token`, just without UIAccess. Only when the
    // duplication itself failed does the launch fall back to `token`.
    let launch_token = flagged_token.as_ref().map_or(token, |t| t.handle());

    let mut desktop = wide("winsta0\\default");
    let mut cmdline = wide(&format!("\"{exe_path}\" {args}"));

    // Build the user's environment block so the engine sees their profile.
    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: launch_token is a valid primary token (the caller's, or our
    // duplicate, alive until the end of this function).
    let have_env = unsafe { CreateEnvironmentBlock(&mut env, Some(launch_token), false) }.is_ok();

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    // SAFETY: all pointers valid for the call; cmdline is a writable buffer as
    // CreateProcessAsUserW requires.
    let result = unsafe {
        CreateProcessAsUserW(
            Some(launch_token),
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | NORMAL_PRIORITY_CLASS,
            if have_env { Some(env) } else { None },
            None,
            &si,
            &mut pi,
        )
    };

    if have_env && !env.is_null() {
        // SAFETY: env came from CreateEnvironmentBlock.
        let _ = unsafe { DestroyEnvironmentBlock(env) };
    }
    result?;

    // Close the handles we don't keep; the process runs independently.
    // SAFETY: valid handles from a successful CreateProcessAsUserW.
    unsafe {
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(Launched {
        pid: pi.dwProcessId,
        ui_access,
    })
}
