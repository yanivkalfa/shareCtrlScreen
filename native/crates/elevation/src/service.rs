//! Windows service install / start / stop / uninstall / query (Plan 04 §8b,
//! §10; task W4). The service runs as **LocalSystem** and is registered once
//! (needs admin) — by the NSIS installer's hooks (`native/app/installer-hooks.nsh`)
//! or by hand with `sharectrl-service.exe --install-service`. A user-mode
//! service installs and runs **unsigned** — the only cost of skipping
//! code-signing is a first-install SmartScreen prompt (§11.4). The service does
//! not host UI itself (Microsoft warns against it); it launches the host app
//! into the user session (see [`crate::session`]).
//!
//! Every operation here is idempotent, because the installer runs them on fresh
//! installs, upgrades and re-installs alike:
//!   * [`install`] creates the service, or re-points an existing one at this
//!     binary (an upgrade, or a manual install that moved to Program Files),
//!     then starts it and waits until it reports RUNNING;
//!   * [`stop`] is a no-op for a missing or stopped service, and otherwise
//!     returns only once the service's PROCESS has exited — the SCM reports
//!     STOPPED before the process is gone, and the installer replaces / deletes
//!     the executable right after;
//!   * [`uninstall`] stops first (the SCM only removes a service once it has
//!     stopped and every handle is closed) and treats "not installed" as done.
//!
//! ## References
//!
//! * Starting a service (StartService, then poll QueryServiceStatusEx until
//!   RUNNING; ERROR_SERVICE_ALREADY_RUNNING):
//!   <https://learn.microsoft.com/windows/win32/services/starting-a-service>
//! * Stopping a service (ControlService(SERVICE_CONTROL_STOP), poll until
//!   STOPPED, honour STOP_PENDING; ERROR_SERVICE_NOT_ACTIVE):
//!   <https://learn.microsoft.com/windows/win32/services/stopping-a-service>
//! * ControlService: <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-controlservice>
//! * CreateServiceW (ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE):
//!   <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-createservicew>
//! * ChangeServiceConfigW (SERVICE_NO_CHANGE):
//!   <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-changeserviceconfigw>
//! * DeleteService ("marks a service for deletion … the service is not deleted
//!   until all open handles … are closed and the service is not running"):
//!   <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-deleteservice>
//! * QueryServiceStatusEx / SERVICE_STATUS_PROCESS (dwProcessId):
//!   <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-queryservicestatusex>
//! * QueryServiceConfigW (two-call sizing, dwStartType):
//!   <https://learn.microsoft.com/windows/win32/api/winsvc/nf-winsvc-queryserviceconfigw>
//! * Service security and access rights (what a standard user may query):
//!   <https://learn.microsoft.com/windows/win32/services/service-security-and-access-rights>

use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE,
    ERROR_SERVICE_NOT_ACTIVE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QueryServiceStatusEx, StartServiceW,
    QUERY_SERVICE_CONFIGW, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SC_STATUS_PROCESS_INFO, SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONTROL_STOP,
    SERVICE_ERROR_NORMAL, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::wide;

pub const SERVICE_NAME: &str = "ShareCtrlScreenHost";
const DISPLAY_NAME: &str = "ShareCtrlScreen Host Service";

/// `DELETE` standard access right (winnt.h), needed by `DeleteService`.
const DELETE: u32 = 0x0001_0000;

/// How long a start or a stop may take before we report a timeout. The service
/// polls its stop flag every 2 s (`service/src/main.rs`, `session_loop`), so a
/// healthy stop takes at most ~2 s; 30 s leaves room for a loaded machine
/// without hanging an installer forever.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);
/// After STOPPED, how long the service process may take to exit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{step} failed: {source}")]
    Win {
        step: &'static str,
        source: windows::core::Error,
    },
    #[error(
        "{step} was refused: managing the service needs administrator rights \
         (run from a terminal opened as administrator, or re-run the installer)"
    )]
    NeedsAdmin { step: &'static str },
    #[error(
        "the service is marked for deletion by an earlier uninstall and cannot be \
         re-created yet: close the Services console (services.msc) and any program \
         querying it, or restart Windows, then try again"
    )]
    MarkedForDeletion,
    #[error(
        "the service did not reach the {want} state within {secs} s (last state: {last}); \
         see the Windows Event Log (System, source Service Control Manager)"
    )]
    Timeout {
        want: &'static str,
        last: &'static str,
        secs: u64,
    },
    #[error(
        "the service stopped while starting (Windows exit code {code}); see the \
         Windows Event Log (System, source Service Control Manager)"
    )]
    StoppedWhileStarting { code: u32 },
    #[error(
        "the service process (pid {pid}) did not exit within {secs} s after stopping; \
         end it in Task Manager and try again"
    )]
    ProcessLingers { pid: u32, secs: u64 },
}

impl ServiceError {
    /// Attach the step name, mapping "access denied" to the actionable
    /// [`ServiceError::NeedsAdmin`].
    fn at(step: &'static str) -> impl Fn(windows::core::Error) -> ServiceError {
        move |source| {
            if is(&source, ERROR_ACCESS_DENIED) {
                ServiceError::NeedsAdmin { step }
            } else {
                ServiceError::Win { step, source }
            }
        }
    }
}

fn is(e: &windows::core::Error, code: WIN32_ERROR) -> bool {
    e.code() == code.to_hresult()
}

/// The service's run state as the SCM reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    /// Paused / continue-pending: the service never accepts pause, so this is
    /// only reachable through a foreign tool; treated as "present, not running".
    Other,
}

impl ServiceState {
    fn from_scm(s: SERVICE_STATUS_CURRENT_STATE) -> Self {
        match s {
            SERVICE_STOPPED => Self::Stopped,
            SERVICE_START_PENDING => Self::StartPending,
            SERVICE_STOP_PENDING => Self::StopPending,
            SERVICE_RUNNING => Self::Running,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::Stopped => "stopped",
            Self::StartPending => "start pending",
            Self::StopPending => "stop pending",
            Self::Running => "running",
            Self::Other => "paused",
        }
    }
}

/// What [`query`] found: the run state and whether Windows starts the service
/// at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub auto_start: bool,
}

impl ServiceStatus {
    /// Whether the service, not the per-user Run key, is what brings the host
    /// up at sign-in (W4 "exactly one host instance").
    ///
    /// True when the service is running (or starting) now, or is installed
    /// with automatic start even if it is momentarily stopped. The second half
    /// matters: an auto-start service that is stopped right now (an admin
    /// stopped it, or the installer is mid-upgrade) is running again at the
    /// next boot, and a Run entry recreated in that window would start a
    /// second host at the next sign-in, racing the service's.
    pub fn owns_sign_in(&self) -> bool {
        match self.state {
            ServiceState::NotInstalled => false,
            ServiceState::Running | ServiceState::StartPending => true,
            ServiceState::Stopped | ServiceState::StopPending | ServiceState::Other => {
                self.auto_start
            }
        }
    }
}

/// Result of [`install`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// A new service was registered.
    Created,
    /// The service existed; its binary path and start type were updated.
    Updated,
}

/// Result of [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    NotInstalled,
    WasNotRunning,
    Stopped,
}

/// Result of [`uninstall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstalled {
    NotInstalled,
    Removed,
}

struct ScmHandle(SC_HANDLE);
impl Drop for ScmHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: valid SCM/service handle, closed exactly once here.
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn open_scm(access: u32, step: &'static str) -> Result<ScmHandle, ServiceError> {
    // SAFETY: local SCM, default database; the handle is owned by ScmHandle.
    let h = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map_err(ServiceError::at(step))?;
    Ok(ScmHandle(h))
}

/// Open our service, or `None` when it is not installed.
fn open_service(
    scm: &ScmHandle,
    access: u32,
    step: &'static str,
) -> Result<Option<ScmHandle>, ServiceError> {
    let name = wide(SERVICE_NAME);
    // SAFETY: valid SCM handle + NUL-terminated name that outlives the call.
    match unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), access) } {
        Ok(h) => Ok(Some(ScmHandle(h))),
        Err(e) if is(&e, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
        Err(e) => Err(ServiceError::at(step)(e)),
    }
}

fn status_process(svc: &ScmHandle) -> Result<SERVICE_STATUS_PROCESS, ServiceError> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    // SAFETY: the byte view covers exactly `status`, a plain-old-data repr(C)
    // struct that is the documented output of SC_STATUS_PROCESS_INFO; the view
    // does not outlive `status`.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    // SAFETY: valid service handle opened with SERVICE_QUERY_STATUS.
    unsafe { QueryServiceStatusEx(svc.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed) }
        .map_err(ServiceError::at("query service status"))?;
    Ok(status)
}

fn is_auto_start(svc: &ScmHandle) -> Result<bool, ServiceError> {
    let step = "query service configuration";
    let mut needed = 0u32;
    // SAFETY: sizing call with no buffer; fails with ERROR_INSUFFICIENT_BUFFER
    // and reports the size in `needed`.
    match unsafe { QueryServiceConfigW(svc.0, None, 0, &mut needed) } {
        Ok(()) => {}
        Err(e) if is(&e, ERROR_INSUFFICIENT_BUFFER) => {}
        Err(e) => return Err(ServiceError::at(step)(e)),
    }
    // u64 elements: QUERY_SERVICE_CONFIGW holds pointers, so the buffer must be
    // pointer-aligned; a Vec<u8> would not guarantee that.
    let words = (needed as usize).div_ceil(8).max(1);
    let mut buf = vec![0u64; words];
    let cfg = buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: `buf` is at least `needed` bytes and 8-byte aligned; the SCM
    // writes the struct followed by the strings it points to.
    unsafe { QueryServiceConfigW(svc.0, Some(cfg), (words * 8) as u32, &mut needed) }
        .map_err(ServiceError::at(step))?;
    // SAFETY: filled by the successful call above; `buf` is still alive.
    let start = unsafe { (*cfg).dwStartType };
    Ok(start == SERVICE_AUTO_START)
}

/// The service's current state. Needs no administrator rights: the default
/// service security descriptor grants every authenticated user
/// SERVICE_QUERY_STATUS and SERVICE_QUERY_CONFIG, which is why the app (a
/// standard user) can call this to decide about its own autostart entry.
pub fn query() -> Result<ServiceStatus, ServiceError> {
    let scm = open_scm(SC_MANAGER_CONNECT, "connect to the service manager")?;
    let Some(svc) = open_service(
        &scm,
        SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
        "open the service",
    )?
    else {
        return Ok(ServiceStatus {
            state: ServiceState::NotInstalled,
            auto_start: false,
        });
    };
    let state = ServiceState::from_scm(status_process(&svc)?.dwCurrentState);
    Ok(ServiceStatus {
        state,
        auto_start: is_auto_start(&svc)?,
    })
}

/// Poll until the service reaches `want`, or time out.
fn wait_for(
    svc: &ScmHandle,
    want: ServiceState,
    timeout: Duration,
) -> Result<SERVICE_STATUS_PROCESS, ServiceError> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = status_process(svc)?;
        let state = ServiceState::from_scm(status.dwCurrentState);
        if state == want {
            return Ok(status);
        }
        // A start that falls back to STOPPED will never reach RUNNING; report
        // at once instead of burning the whole timeout.
        if want == ServiceState::Running && state == ServiceState::Stopped {
            return Err(ServiceError::StoppedWhileStarting {
                code: status.dwWin32ExitCode,
            });
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::Timeout {
                want: want.label(),
                last: state.label(),
                secs: timeout.as_secs(),
            });
        }
        std::thread::sleep(POLL);
    }
}

/// Wait for process `pid` to exit. A pid that can no longer be opened has
/// already exited (OpenProcess fails for a dead pid), which is success.
fn wait_process_exit(pid: u32) -> Result<(), ServiceError> {
    if pid == 0 {
        return Ok(());
    }
    // SAFETY: SYNCHRONIZE-only handle, closed below on every path.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return Ok(());
    };
    // SAFETY: valid process handle opened with SYNCHRONIZE.
    let waited = unsafe { WaitForSingleObject(process, EXIT_TIMEOUT.as_millis() as u32) };
    // SAFETY: our handle from OpenProcess.
    let _ = unsafe { CloseHandle(process) };
    if waited == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(ServiceError::ProcessLingers {
            pid,
            secs: EXIT_TIMEOUT.as_secs(),
        })
    }
}

fn start_handle(svc: &ScmHandle) -> Result<(), ServiceError> {
    // SAFETY: valid handle opened with SERVICE_START; no arguments.
    match unsafe { StartServiceW(svc.0, None) } {
        Ok(()) => {}
        // Idempotent: an already running service is what we want.
        Err(e) if is(&e, ERROR_SERVICE_ALREADY_RUNNING) => {}
        Err(e) => return Err(ServiceError::at("start the service")(e)),
    }
    wait_for(svc, ServiceState::Running, STATE_TIMEOUT)?;
    tracing::info!("service '{SERVICE_NAME}' running");
    Ok(())
}

fn stop_handle(svc: &ScmHandle) -> Result<Stopped, ServiceError> {
    let status = status_process(svc)?;
    let pid = status.dwProcessId;
    match ServiceState::from_scm(status.dwCurrentState) {
        ServiceState::Stopped => return Ok(Stopped::WasNotRunning),
        // Someone else already asked it to stop: just wait, as Microsoft's
        // "Stopping a service" sample does.
        ServiceState::StopPending => {}
        _ => {
            let mut raw = SERVICE_STATUS::default();
            // SAFETY: valid handle opened with SERVICE_STOP; `raw` is an out param.
            match unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut raw) } {
                Ok(()) => {}
                Err(e) if is(&e, ERROR_SERVICE_NOT_ACTIVE) => return Ok(Stopped::WasNotRunning),
                Err(e) => return Err(ServiceError::at("stop the service")(e)),
            }
        }
    }
    wait_for(svc, ServiceState::Stopped, STATE_TIMEOUT)?;
    // STOPPED is reported by the service itself just before its process
    // returns from the dispatcher; the executable stays locked until the
    // process is really gone, and the installer overwrites / deletes it next.
    wait_process_exit(pid)?;
    tracing::info!("service '{SERVICE_NAME}' stopped");
    Ok(Stopped::Stopped)
}

/// Register the service as LocalSystem, auto-start, pointing at `exe_path`
/// (the service binary), then start it and wait until it runs. An existing
/// registration is re-pointed at `exe_path` instead (upgrade, or a move from a
/// manual location into Program Files); a running one keeps running and is
/// restarted by the caller only if it stopped it first. Requires admin.
pub fn install(exe_path: &str) -> Result<Installed, ServiceError> {
    let scm = open_scm(
        SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        "connect to the service manager",
    )?;
    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);
    // Quoted: the path is under "C:\Program Files\…", and an unquoted service
    // path with spaces is the classic unquoted-service-path privilege bug.
    let bin = wide(&format!("\"{exe_path}\" --run-service"));
    let access = SERVICE_START | SERVICE_QUERY_STATUS | SERVICE_CHANGE_CONFIG;

    // SAFETY: all wide strings NUL-terminated and outlive the call.
    let created = unsafe {
        CreateServiceW(
            scm.0,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            access,
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
        )
    };
    let (svc, outcome) = match created {
        Ok(h) => (ScmHandle(h), Installed::Created),
        Err(e) if is(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => {
            return Err(ServiceError::MarkedForDeletion)
        }
        Err(e) if is(&e, ERROR_SERVICE_EXISTS) => {
            let Some(svc) = open_service(&scm, access, "open the existing service")? else {
                // Deleted between the two calls; nothing sensible to retry
                // against, so report it as the pending-delete case it is.
                return Err(ServiceError::MarkedForDeletion);
            };
            // SAFETY: valid handle with SERVICE_CHANGE_CONFIG; every string
            // outlives the call. Null strings mean "no change", so the account
            // stays LocalSystem and no dependencies are added.
            unsafe {
                ChangeServiceConfigW(
                    svc.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(bin.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR(display.as_ptr()),
                )
            }
            .map_err(ServiceError::at("update the existing service"))?;
            (svc, Installed::Updated)
        }
        Err(e) => return Err(ServiceError::at("register the service")(e)),
    };
    tracing::info!("service '{SERVICE_NAME}' registered (LocalSystem, auto-start): {exe_path}");
    start_handle(&svc)?;
    Ok(outcome)
}

/// Start the installed service and wait until it runs. Idempotent.
pub fn start() -> Result<(), ServiceError> {
    let scm = open_scm(SC_MANAGER_CONNECT, "connect to the service manager")?;
    let Some(svc) = open_service(
        &scm,
        SERVICE_START | SERVICE_QUERY_STATUS,
        "open the service",
    )?
    else {
        return Err(ServiceError::Win {
            step: "start the service",
            source: ERROR_SERVICE_DOES_NOT_EXIST.to_hresult().into(),
        });
    };
    start_handle(&svc)
}

/// Stop the service if it is installed and running, and wait until its
/// process has exited. A missing or stopped service is success.
pub fn stop() -> Result<Stopped, ServiceError> {
    let scm = open_scm(SC_MANAGER_CONNECT, "connect to the service manager")?;
    let Some(svc) = open_service(
        &scm,
        SERVICE_STOP | SERVICE_QUERY_STATUS,
        "open the service",
    )?
    else {
        return Ok(Stopped::NotInstalled);
    };
    stop_handle(&svc)
}

/// Stop, then remove the service (requires admin). Not installed is success.
pub fn uninstall() -> Result<Uninstalled, ServiceError> {
    let scm = open_scm(SC_MANAGER_CONNECT, "connect to the service manager")?;
    let Some(svc) = open_service(
        &scm,
        SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
        "open the service",
    )?
    else {
        return Ok(Uninstalled::NotInstalled);
    };
    // Stop first: DeleteService on a running service only marks it, and the
    // SCM keeps it (and its locked executable) until it stops.
    stop_handle(&svc)?;
    // SAFETY: valid handle opened with DELETE.
    match unsafe { DeleteService(svc.0) } {
        Ok(()) => {}
        // A previous uninstall already marked it; once our handle closes it goes.
        Err(e) if is(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => {}
        Err(e) => return Err(ServiceError::at("remove the service")(e)),
    }
    tracing::info!("service '{SERVICE_NAME}' removed");
    Ok(Uninstalled::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: ServiceState, auto_start: bool) -> ServiceStatus {
        ServiceStatus { state, auto_start }
    }

    #[test]
    fn a_running_service_owns_sign_in_whatever_its_start_type() {
        assert!(status(ServiceState::Running, true).owns_sign_in());
        assert!(status(ServiceState::Running, false).owns_sign_in());
        assert!(status(ServiceState::StartPending, false).owns_sign_in());
    }

    #[test]
    fn a_stopped_auto_start_service_still_owns_sign_in() {
        // It runs again at the next boot; a Run entry would race it.
        assert!(status(ServiceState::Stopped, true).owns_sign_in());
        assert!(status(ServiceState::StopPending, true).owns_sign_in());
    }

    #[test]
    fn a_stopped_manual_or_disabled_service_does_not() {
        assert!(!status(ServiceState::Stopped, false).owns_sign_in());
        assert!(!status(ServiceState::Other, false).owns_sign_in());
    }

    #[test]
    fn a_missing_service_never_owns_sign_in() {
        assert!(!status(ServiceState::NotInstalled, false).owns_sign_in());
        assert!(!status(ServiceState::NotInstalled, true).owns_sign_in());
    }

    #[test]
    fn scm_states_map_one_to_one() {
        assert_eq!(
            ServiceState::from_scm(SERVICE_STOPPED),
            ServiceState::Stopped
        );
        assert_eq!(
            ServiceState::from_scm(SERVICE_RUNNING),
            ServiceState::Running
        );
        assert_eq!(
            ServiceState::from_scm(SERVICE_START_PENDING),
            ServiceState::StartPending
        );
        assert_eq!(
            ServiceState::from_scm(SERVICE_STOP_PENDING),
            ServiceState::StopPending
        );
    }

    #[test]
    fn access_denied_becomes_the_admin_hint() {
        let e = ServiceError::at("register the service")(ERROR_ACCESS_DENIED.to_hresult().into());
        assert!(matches!(e, ServiceError::NeedsAdmin { .. }));
        assert!(e.to_string().contains("administrator"), "{e}");
    }

    #[test]
    fn query_works_without_admin() {
        // Runs as a standard user in CI and in the lane: must not need rights
        // the app does not have. Our service is normally absent there.
        let s = query().expect("query needs no admin rights");
        if s.state == ServiceState::NotInstalled {
            assert!(!s.owns_sign_in());
        }
    }
}
