//! The SYSTEM Windows service binary (Plan 04 §4, §8b). Thin: it wires the
//! `elevation` primitives into the SCM dispatcher and the session-follow launch
//! loop. It does NOT host UI (Microsoft warns against it) — it launches the
//! engine into the active user session and relaunches on session change.
//!
//! Usage:
//!   sharectrl-service --install-service   (admin, once)
//!   sharectrl-service --uninstall-service (admin)
//!   sharectrl-service --run-service       (invoked by the SCM)

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--install-service" => {
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_default();
            match elevation::service::install(&exe) {
                Ok(()) => println!("installed"),
                Err(e) => eprintln!("install failed: {e}"),
            }
        }
        "--uninstall-service" => match elevation::service::uninstall() {
            Ok(()) => println!("uninstalled"),
            Err(e) => eprintln!("uninstall failed: {e}"),
        },
        "--run-service" => run_dispatcher(),
        _ => {
            eprintln!(
                "usage: sharectrl-service [--install-service|--uninstall-service|--run-service]"
            );
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

mod dispatcher {
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
    use windows::Win32::System::Services::{
        RegisterServiceCtrlHandlerW, SetServiceStatus, StartServiceCtrlDispatcherW,
        SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
        SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
        SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };

    static mut STATUS_HANDLE: Option<SERVICE_STATUS_HANDLE> = None;

    fn set_state(state: SERVICE_STATUS_CURRENT_STATE, accept_stop: bool) {
        // SAFETY: STATUS_HANDLE is set once in service_main before any control
        // callback can fire; single service instance.
        let handle = unsafe { STATUS_HANDLE };
        let Some(handle) = handle else { return };
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if accept_stop { SERVICE_ACCEPT_STOP } else { 0 },
            dwWin32ExitCode: NO_ERROR.0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 0,
        };
        // SAFETY: valid status handle + fully-initialized status.
        let _ = unsafe { SetServiceStatus(handle, &status) };
    }

    unsafe extern "system" fn handler(control: u32) {
        if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
            STOP.store(true, Ordering::SeqCst);
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
        let name = elevation_service_name_wide();
        // SAFETY: name NUL-terminated; handler is a valid fn pointer.
        let handle = unsafe {
            RegisterServiceCtrlHandlerW(windows::core::PCWSTR(name.as_ptr()), Some(handler))
        };
        let Ok(handle) = handle else { return };
        unsafe { STATUS_HANDLE = Some(handle) };

        set_state(SERVICE_RUNNING, true);
        super::session_loop();
        set_state(SERVICE_STOPPED, false);
    }

    fn elevation_service_name_wide() -> Vec<u16> {
        elevation::service::SERVICE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Enter the SCM dispatcher; blocks until the service stops.
    pub fn run() {
        let mut name = elevation_service_name_wide();
        let table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: PWSTR(name.as_mut_ptr()),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW::default(),
        ];
        // SAFETY: null-terminated table with a valid entry point.
        let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
        if ok.is_err() {
            // Not started by the SCM (e.g. run from a console). Fall back to a
            // direct loop so `--run-service` still works for local testing.
            let _ = ERROR_CALL_NOT_IMPLEMENTED;
            super::session_loop();
        }
    }
}

fn run_dispatcher() {
    dispatcher::run();
}

/// Follow the active session and keep the engine running there (§8b). Relaunch
/// whenever the active console session changes (login, fast-user-switch, RDP).
fn session_loop() {
    let engine_exe = engine_path();
    let mut last_session: Option<u32> = None;

    while !STOP.load(Ordering::SeqCst) {
        match elevation::active_console_session() {
            Some(session) if Some(session) != last_session => {
                match elevation::user_token(session) {
                    Ok(token) => {
                        match elevation::launch_in_session(token, &engine_exe, "--engine") {
                            Ok(pid) => {
                                tracing::info!("launched engine pid={pid} in session {session}");
                                last_session = Some(session);
                            }
                            Err(e) => tracing::warn!("launch failed: {e}"),
                        }
                    }
                    Err(e) => tracing::warn!("user token unavailable for session {session}: {e}"),
                }
            }
            Some(_) => {}
            None => last_session = None, // logged out; relaunch on next login
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Resolve the engine/app executable next to the service binary.
fn engine_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sharectrl.exe")))
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "sharectrl.exe".to_string())
}
