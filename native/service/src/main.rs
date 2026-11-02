//! The SYSTEM Windows service binary (Plan 04 §4, §8b). Thin: it wires the
//! `elevation` primitives into the SCM dispatcher and the session-follow launch
//! loop. It does NOT host UI (Microsoft warns against it) — it launches the
//! host app into the active user session and relaunches on session change.
//!
//! The NSIS installer ships this binary next to `sharectrl.exe` and drives it
//! from its hooks (`native/app/installer-hooks.nsh`, task W4); the same
//! commands serve a manual install of the standalone release asset.
//!
//! Usage (all but `--run-service` need an elevated terminal):
//!   sharectrl-service --install-service   register (or re-point) at this exe, start
//!   sharectrl-service --start-service     start, wait until running
//!   sharectrl-service --stop-service      stop, wait until the process exits
//!   sharectrl-service --uninstall-service stop, then remove
//!   sharectrl-service --run-service       (invoked by the SCM)

// The service is a Windows concept (SCM, sessions, the secure desktop). On other
// platforms the binary only explains that, so the workspace builds everywhere.
#[cfg(not(windows))]
fn main() {
    eprintln!("sharectrl-service is Windows-only; nothing to do on this platform");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    windows_service::main();
}

#[cfg(windows)]
mod windows_service {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use elevation::service::{Installed, Stopped, Uninstalled, SERVICE_NAME};

    const USAGE: &str = "usage: sharectrl-service \
        [--install-service|--start-service|--stop-service|--uninstall-service|--run-service]";

    /// The management commands print one line saying what happened and exit 0,
    /// or print the reason and exit 1, because the NSIS installer hooks
    /// (`native/app/installer-hooks.nsh`) run them through `nsExec::ExecToLog`:
    /// the line lands in the installer's details pane and the exit code decides
    /// whether the install goes on. Unknown arguments exit 2.
    pub fn main() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();

        let arg = std::env::args().nth(1).unwrap_or_default();
        let outcome = match arg.as_str() {
            "--install-service" => install(),
            "--start-service" => elevation::service::start()
                .map(|()| format!("service '{SERVICE_NAME}' is running"))
                .map_err(|e| e.to_string()),
            "--stop-service" => elevation::service::stop()
                .map(|s| match s {
                    Stopped::NotInstalled => format!("service '{SERVICE_NAME}' is not installed"),
                    Stopped::WasNotRunning => format!("service '{SERVICE_NAME}' was not running"),
                    Stopped::Stopped => format!("service '{SERVICE_NAME}' stopped"),
                })
                .map_err(|e| e.to_string()),
            "--uninstall-service" => elevation::service::uninstall()
                .map(|u| match u {
                    Uninstalled::NotInstalled => {
                        format!("service '{SERVICE_NAME}' was not installed")
                    }
                    Uninstalled::Removed => format!("service '{SERVICE_NAME}' stopped and removed"),
                })
                .map_err(|e| e.to_string()),
            "--run-service" => {
                run_dispatcher();
                return;
            }
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        };
        match outcome {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("sharectrl-service {arg}: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Register (or re-point) the service at THIS executable and start it. The
    /// service launches `sharectrl.exe` from its own folder (`engine_path`),
    /// so this binary must sit next to the app before it is installed.
    fn install() -> Result<String, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot determine this program's path: {e}"))?;
        let exe = exe
            .to_str()
            .ok_or_else(|| format!("the path {} is not valid Unicode", exe.display()))?
            .to_string();
        let app = engine_path();
        if !std::path::Path::new(&app).is_file() {
            return Err(format!(
                "{app} not found: put sharectrl-service.exe in the same folder as \
                 sharectrl.exe (the service starts the app from its own folder)"
            ));
        }
        let what = match elevation::service::install(&exe).map_err(|e| e.to_string())? {
            Installed::Created => "registered",
            Installed::Updated => "updated",
        };
        Ok(format!(
            "service '{SERVICE_NAME}' {what} ({exe}) and running"
        ))
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

    /// The host's command-line arguments. `--autostart`: the service instance
    /// IS the sign-in autostart (the app removes its own Run entry while the
    /// service is installed, task W4), so it comes up minimized exactly like
    /// the Run-key instance did. `--service-host`: marks it as the session's
    /// one host, so other launches in the session defer to it
    /// (`platform_windows::instance`).
    fn host_args() -> String {
        format!(
            "{} {}",
            platform_api::AUTOSTART_FLAG,
            elevation::SERVICE_HOST_FLAG
        )
    }

    /// Follow the active session and keep the engine running there (§8b). Relaunch
    /// whenever the active console session changes (login, fast-user-switch, RDP).
    fn session_loop() {
        let engine_exe = engine_path();
        let args = host_args();
        let mut last_session: Option<u32> = None;

        while !STOP.load(Ordering::SeqCst) {
            match elevation::active_console_session() {
                Some(session) if Some(session) != last_session => {
                    match elevation::user_token(session) {
                        Ok(token) => {
                            let launched = elevation::launch_in_session(token, &engine_exe, &args);
                            // The token is ours (WTSQueryUserToken); the child
                            // has its own copy once CreateProcessAsUser returns.
                            // SAFETY: valid handle, closed exactly once.
                            let _ = unsafe { windows::Win32::Foundation::CloseHandle(token) };
                            match launched {
                                Ok(pid) => {
                                    tracing::info!(
                                        "launched engine pid={pid} in session {session}"
                                    );
                                    last_session = Some(session);
                                }
                                Err(e) => tracing::warn!("launch failed: {e}"),
                            }
                        }
                        Err(e) => {
                            tracing::warn!("user token unavailable for session {session}: {e}")
                        }
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_host_is_launched_as_the_sign_in_service_host() {
            let args = host_args();
            let flags: Vec<&str> = args.split(' ').collect();
            assert_eq!(flags, ["--autostart", "--service-host"]);
            assert_eq!(flags[0], platform_api::AUTOSTART_FLAG);
            assert_eq!(flags[1], elevation::SERVICE_HOST_FLAG);
        }

        #[test]
        fn the_installer_hooks_match_this_binary() {
            // The NSIS hooks name the service and call these commands; a rename
            // on either side must fail here, not on a user's machine.
            let hooks = include_str!("../../app/installer-hooks.nsh");
            assert!(
                hooks.contains(&format!("!define SCT_SERVICE_NAME \"{SERVICE_NAME}\"")),
                "installer-hooks.nsh names another service"
            );
            for cmd in ["--stop-service", "--install-service", "--uninstall-service"] {
                assert!(
                    hooks.contains(cmd),
                    "installer-hooks.nsh does not call {cmd}"
                );
                assert!(USAGE.contains(cmd), "{cmd} is not a command of this binary");
            }
        }

        #[test]
        fn the_host_is_the_app_next_to_the_service() {
            let path = std::path::PathBuf::from(engine_path());
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some("sharectrl.exe")
            );
            let here = std::env::current_exe().expect("test exe path");
            assert_eq!(path.parent(), here.parent());
        }
    }
}
