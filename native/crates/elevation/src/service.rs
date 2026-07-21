//! Windows service install / uninstall (Plan 04 §8b, §10). The service runs as
//! **LocalSystem** and is registered once (needs admin) with
//! `--install-service`. A user-mode service installs and runs **unsigned** — the
//! only cost of skipping code-signing is a first-install SmartScreen prompt
//! (§11.4). The service does not host UI itself (Microsoft warns against it); it
//! launches the engine into the user session (see [`crate::session`]).

use windows::core::PCWSTR;
use windows::Win32::System::Services::SC_HANDLE;
use windows::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL,
    SERVICE_WIN32_OWN_PROCESS,
};

use crate::wide;

pub const SERVICE_NAME: &str = "ShareCtrlScreenHost";
const DISPLAY_NAME: &str = "ShareCtrlScreen Host Service";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("must be run elevated (admin) to manage the service")]
    NeedsAdmin,
}

struct ScmHandle(SC_HANDLE);
impl Drop for ScmHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: valid SCM/service handle.
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }
}

/// Register the service as LocalSystem, auto-start, pointing at `exe_path`
/// (the service binary). Requires admin (§8b: admin once, at install).
pub fn install(exe_path: &str) -> Result<(), ServiceError> {
    // SAFETY: opens the SCM with full access (admin required).
    let scm = ScmHandle(unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)?
    });

    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);
    let bin = wide(&format!("\"{exe_path}\" --run-service"));

    // SAFETY: all wide strings NUL-terminated and outlive the call.
    let svc = unsafe {
        CreateServiceW(
            scm.0,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            // lpServiceStartName = NULL ⇒ LocalSystem.
            PCWSTR::null(),
            PCWSTR::null(),
        )?
    };
    // SAFETY: valid service handle.
    let _ = unsafe { CloseServiceHandle(svc) };
    tracing::info!("service '{SERVICE_NAME}' installed (LocalSystem, auto-start)");
    Ok(())
}

/// Remove the service (requires admin).
pub fn uninstall() -> Result<(), ServiceError> {
    // SAFETY: opens the SCM with full access.
    let scm = ScmHandle(unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)?
    });
    let name = wide(SERVICE_NAME);
    // SAFETY: valid SCM handle + NUL-terminated name.
    let svc = unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS)? };
    // SAFETY: valid service handle.
    unsafe {
        DeleteService(svc)?;
        let _ = CloseServiceHandle(svc);
    }
    tracing::info!("service '{SERVICE_NAME}' removed");
    Ok(())
}
