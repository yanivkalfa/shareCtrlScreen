//! The SYSTEM Windows service binary (Plan 04 §4, §8b). Thin: it wires the
//! `elevation` primitives into the SCM dispatcher and the session-follow launch
//! loop. It does NOT host UI (Microsoft warns against it) — it launches the
//! host app into the active user session, relaunches on session change and on
//! a sign-in, and relaunches a host that dies (`supervise`).
//!
//! The SCM's control handler (`HandlerEx`, U2) accepts stop, shutdown and
//! session change. It only records what it was told and wakes the loop
//! (`events::Inbox`); every decision, launch and log line stays on the loop's
//! thread. The loop waits for that wake-up with a 2 s timeout, so the poll of
//! the active console session stays as the fallback (a sign-in that starts
//! before the service is fully loaded is never notified).
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
//!
//! ## References
//! - RegisterServiceCtrlHandlerExW: <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-registerservicectrlhandlerexw>
//!   — "must be called before the first SetServiceStatus call".
//! - HandlerEx: <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nc-winsvc-lphandler_function_ex>
//!   — the control codes, return values and "avoid operations that might
//!   block, such as taking a lock" (no logging in the handler: the log writer
//!   is behind a mutex); see `events.rs` for the quotes.
//! - Service control handler function: <https://learn.microsoft.com/en-us/windows/win32/services/service-control-handler-function>
//!   — "The control handler should simply call SetServiceStatus with the
//!   SERVICE_STOP_PENDING message and return"; "must return within 30
//!   seconds"; "about 20 seconds" at shutdown.
//! - Writing a control handler: <https://learn.microsoft.com/en-us/windows/win32/services/writing-a-control-handler-function>
//!   — "it should report the service status only if handling the control
//!   code causes the service status to change".
//! - SERVICE_STATUS: <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_status>
//!   — `SERVICE_ACCEPT_STOP` 0x1, `SERVICE_ACCEPT_SHUTDOWN` 0x4,
//!   `SERVICE_ACCEPT_SESSIONCHANGE` 0x80; `dwCheckPoint` and `dwWaitHint`
//!   apply only to a pending state. SHUTDOWN is accepted although the page
//!   reserves it for services that must clean up: the loop then ends before
//!   the system does instead of being killed mid-launch, and from U3 the
//!   service must end its agent at shutdown (plan 10a §3.7).
//! - SetServiceStatus: <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-setservicestatus>
//!   — "Do not register to accept controls while the status is
//!   SERVICE_START_PENDING or the service can crash" (the service reports
//!   RUNNING with its accept mask directly, as before).
//! - WTSQueryUserToken: <https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken>
//!   — LocalSystem with SE_TCB_NAME; the error when nobody is signed in is
//!   not documented, so any failure counts as "no user yet".
//! - WTSGetActiveConsoleSessionId: <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-wtsgetactiveconsolesessionid>
//!   — 0xFFFFFFFF "if the physical console session is in the process of
//!   being attached or detached".

// The relaunch policy and the SCM's messages are plain logic, built (and
// tested) on every OS; only the Windows service uses them.
#[cfg_attr(not(windows), allow(dead_code))]
mod events;
#[cfg_attr(not(windows), allow(dead_code))]
mod supervise;

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
    use std::time::{Duration, Instant};

    use crate::events::{Control, Inbox, SessionEvent};
    use crate::supervise::{EventEffect, Exit, Supervisor};

    use elevation::service::{Installed, Stopped, Uninstalled, SERVICE_NAME};

    const USAGE: &str = "usage: sharectrl-service \
        [--install-service|--start-service|--stop-service|--uninstall-service|--run-service]";

    /// The management commands print one line saying what happened and exit 0,
    /// or print the reason and exit 1, because the NSIS installer hooks
    /// (`native/app/installer-hooks.nsh`) run them through `nsExec::ExecToLog`:
    /// the line lands in the installer's details pane and the exit code decides
    /// whether the install goes on. Unknown arguments exit 2.
    pub fn main() {
        let arg = std::env::args().nth(1).unwrap_or_default();
        init_logging(arg == "--run-service");
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

    /// Set up logging for this invocation (W5). `--run-service` has no console
    /// under the SCM, so it logs to `sharectrl-service.log` next to the service
    /// binary (truncated at each service start) at info — this is where the
    /// per-launch line, including why a host did not get UIAccess, is recorded.
    /// WHY there and not under ProgramData: the file is opened by SYSTEM, and
    /// ProgramData lets standard users pre-create entries (a planted link would
    /// turn the truncate into an arbitrary-file overwrite); the install folder
    /// under Program Files is writable only by admins. If the file cannot be
    /// created it falls back to stdout. The management commands
    /// (install/start/stop/uninstall) carry their result on stdout/stderr for
    /// the NSIS details pane, so they stay quiet (`RUST_LOG` only), as before.
    fn init_logging(service_run: bool) {
        if !service_run {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .init();
            return;
        }
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let log_file = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("sharectrl-service.log")))
            .and_then(|path| std::fs::File::create(path).ok());
        match log_file {
            Some(file) => tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_env_filter(filter)
                .init(),
            None => tracing_subscriber::fmt().with_env_filter(filter).init(),
        }
    }

    /// The control handler's hand-off to the loop (stop requests and session
    /// events). A static because the handler is a plain function the SCM
    /// calls; it lives for the whole process, so the handler can never see it
    /// gone, however late a control arrives.
    static INBOX: Inbox = Inbox::new();

    /// How long the loop waits for a wake-up before it polls anyway.
    const POLL: Duration = Duration::from_secs(2);

    mod dispatcher {
        use super::*;
        use std::sync::atomic::{AtomicPtr, Ordering};
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::NO_ERROR;
        use windows::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
        use windows::Win32::System::Services::{
            RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
            SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
            SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
            SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
        };

        /// The controls the running service accepts.
        pub(super) const ACCEPTED: u32 =
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE;

        /// How long the SCM should allow between STOP_PENDING and STOPPED.
        /// The loop wakes at once and only drops the host's handle (the host
        /// keeps running), so this is generous.
        const STOP_WAIT_HINT_MS: u32 = 3_000;

        /// The status handle from RegisterServiceCtrlHandlerExW, null until
        /// then. An atomic (not a `static mut`) because the control handler
        /// reads it on the dispatcher thread while service_main's thread
        /// stored it: the Release store happens before the handler can be
        /// sent an accepted control (accepts are reported only afterwards).
        static STATUS_HANDLE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

        /// Report `state` to the SCM. A no-op before the handler is
        /// registered (the console fallback has no status to report).
        fn set_state(
            state: SERVICE_STATUS_CURRENT_STATE,
            accepted: u32,
            wait_hint_ms: u32,
        ) -> windows::core::Result<()> {
            let handle = STATUS_HANDLE.load(Ordering::Acquire);
            if handle.is_null() {
                return Ok(());
            }
            let pending = state == SERVICE_STOP_PENDING;
            let status = SERVICE_STATUS {
                dwServiceType: SERVICE_WIN32_OWN_PROCESS,
                dwCurrentState: state,
                dwControlsAccepted: accepted,
                dwWin32ExitCode: NO_ERROR.0,
                dwServiceSpecificExitCode: 0,
                // Valid only for a pending state, zero otherwise
                // (SERVICE_STATUS docs). The stop has one step.
                dwCheckPoint: u32::from(pending),
                dwWaitHint: if pending { wait_hint_ms } else { 0 },
            };
            // SAFETY: the handle came from RegisterServiceCtrlHandlerExW and
            // stays valid for the process; `status` is fully initialised.
            unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(handle), &status) }
        }

        /// HandlerEx, on the SCM's dispatcher thread. It never blocks, takes
        /// a lock, allocates or logs (the log writer is behind a mutex): it
        /// reports STOP_PENDING for a stop, records a session event, wakes the
        /// loop, and returns.
        unsafe extern "system" fn handler(
            control: u32,
            event_type: u32,
            event_data: *mut core::ffi::c_void,
            _context: *mut core::ffi::c_void,
        ) -> u32 {
            let control = Control::from_code(control);
            match control {
                Control::Stop | Control::Shutdown => {
                    // The status changes, so it is reported here (the docs'
                    // pattern). A failure cannot be logged from here; the
                    // loop's STOPPED report follows within moments anyway.
                    let _ = set_state(SERVICE_STOP_PENDING, 0, STOP_WAIT_HINT_MS);
                    INBOX.request_stop();
                }
                Control::SessionChange => {
                    // SAFETY: for SERVICE_CONTROL_SESSIONCHANGE the SCM passes
                    // a WTSSESSION_NOTIFICATION, valid for this call.
                    match unsafe { notification(event_type, event_data) } {
                        Some(event) => INBOX.post(event),
                        None => INBOX.note_lost(),
                    }
                }
                Control::Interrogate | Control::Other(_) => {}
            }
            control.reply()
        }

        /// Read a session-change notification, or `None` if there is none or
        /// it is shorter than its documented size.
        ///
        /// # Safety
        /// `data` is null or points to a WTSSESSION_NOTIFICATION (or at least
        /// to its `cbSize` field followed by `cbSize` bytes) valid for reads.
        unsafe fn notification(
            event_type: u32,
            data: *mut core::ffi::c_void,
        ) -> Option<SessionEvent> {
            let p = data.cast::<WTSSESSION_NOTIFICATION>().cast_const();
            if p.is_null() {
                return None;
            }
            // SAFETY: per the contract, `cbSize` is readable. read_unaligned:
            // nothing documents the SCM's buffer alignment.
            let cb_size = unsafe { std::ptr::addr_of!((*p).cbSize).read_unaligned() };
            if !SessionEvent::notification_complete(cb_size) {
                return None;
            }
            // SAFETY: cbSize says the whole structure is there.
            let session = unsafe { std::ptr::addr_of!((*p).dwSessionId).read_unaligned() };
            Some(SessionEvent::decode(event_type, session))
        }

        unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
            // The loop runs on this thread. It becomes the inbox's waiter
            // BEFORE the handler exists, so no wake-up can be lost (events.rs).
            if !INBOX.set_waiter() {
                tracing::warn!(
                    "the session loop could not register for wake-ups; it polls every {}s",
                    POLL.as_secs()
                );
            }
            let name = elevation_service_name_wide();
            // SAFETY: name is NUL-terminated and outlives the call; handler
            // is a valid HandlerEx; no context pointer is used.
            let handle = unsafe {
                RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(handler), None)
            };
            let handle = match handle {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::error!(
                        "cannot register the service control handler ({e}); the service \
                         cannot run. Reinstall it with --install-service from an elevated \
                         terminal"
                    );
                    return;
                }
            };
            STATUS_HANDLE.store(handle.0, Ordering::Release);

            if let Err(e) = set_state(SERVICE_RUNNING, ACCEPTED, 0) {
                tracing::error!("cannot report the service as running: {e}");
            }
            super::session_loop();
            if let Err(e) = set_state(SERVICE_STOPPED, 0, 0) {
                tracing::error!("cannot report the service as stopped: {e}");
            }
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
            if let Err(e) = ok {
                // Not started by the SCM (e.g. run from a console). Fall back to a
                // direct loop so `--run-service` still works for local testing.
                // No handler exists here: no session events, no stop (Ctrl+C
                // ends the process); the loop polls.
                tracing::info!(
                    "not started by the service control manager ({e}); running the \
                     session loop in this console, polling every {}s",
                    POLL.as_secs()
                );
                INBOX.set_waiter();
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

    /// Follow the active session and keep the engine running there (§8b).
    /// Launch whenever the active console session changes (login,
    /// fast-user-switch, RDP) or a user signs in to it, and launch again when
    /// the host this service started dies. A host that exits cleanly (Quit,
    /// an update, a duplicate stepping aside) stays down; [`crate::supervise`]
    /// has the policy. Each pass runs when the control handler wakes the loop
    /// (a stop, a session event) or after [`POLL`], whichever is first; a pass
    /// is idempotent, so an extra one costs nothing.
    fn session_loop() {
        let engine_exe = engine_path();
        let args = host_args();
        let mut policy = Supervisor::new();
        // The host this service started in the current session, while running.
        let mut host: Option<elevation::HostProcess> = None;

        while !INBOX.stop_requested() {
            if let Some(code) = host.as_ref().and_then(elevation::HostProcess::exit_code) {
                host = None;
                match policy.exited(code, Instant::now()) {
                    Exit::Clean => tracing::info!(
                        "the host exited cleanly (Quit, an update, or another host already \
                         runs); not relaunching until the session changes or its user signs \
                         out and back in"
                    ),
                    Exit::Abnormal { code, retry_in } => tracing::warn!(
                        "the host exited abnormally (exit code {code:#x}); relaunching in {}s",
                        retry_in.as_secs()
                    ),
                }
            }
            let active = elevation::active_console_session();
            policy.follow(active);
            take_events(&mut policy);
            check_user(&mut policy);
            if let Some(session) = policy.poll(active, Instant::now()) {
                match launch(&engine_exe, &args, session) {
                    Ok(launched) => {
                        log_launch(&launched, session);
                        host = Some(launched.process);
                        policy.launched(Instant::now());
                    }
                    Err(e) => {
                        let retry_in = policy.launch_failed(Instant::now());
                        tracing::warn!("{e}; retrying in {}s", retry_in.as_secs());
                    }
                }
            }
            // Only the current session's running host is watched. After a
            // session change (sign-out, user switch) the previous host's
            // handle is dropped here, so its exit is never mistaken for a
            // crash of this session's host.
            if !policy.watching() {
                host = None;
            }
            INBOX.wait(POLL);
        }
        tracing::info!("the service is stopping; the host keeps running in its session");
    }

    /// Apply the session events the control handler recorded since the last
    /// pass. One line each for the active console session; events for other
    /// sessions (RDP, other users) only at debug level.
    fn take_events(policy: &mut Supervisor) {
        let drained = INBOX.drain();
        if drained.lost > 0 {
            tracing::warn!(
                "{} session notification(s) could not be recorded; the {}s poll covers them",
                drained.lost,
                POLL.as_secs()
            );
        }
        for event in drained.events {
            let (what, session) = (event.change.name(), event.session);
            match policy.session_event(event) {
                EventEffect::OtherSession => tracing::debug!(
                    "session event: {what} in session {session} (not the active console \
                     session; ignored)"
                ),
                EventEffect::UserLeft => tracing::info!(
                    "session event: {what} in console session {session}; no host until \
                     someone signs in"
                ),
                EventEffect::Noted => {
                    tracing::info!("session event: {what} in console session {session}")
                }
            }
        }
    }

    /// While no host runs, ask for the console user's token — the first step
    /// of a launch — to learn whether someone is signed in. Any failure means
    /// "nobody yet": the error for that case is not documented.
    fn check_user(policy: &mut Supervisor) {
        if !policy.wants_user_check() {
            return;
        }
        let Some(session) = policy.session() else {
            return;
        };
        match elevation::user_token(session) {
            Ok(token) => {
                // Closed at once: the launch asks for its own.
                drop(elevation::OwnedToken::from_raw(token));
                if let Some(arrived) = policy.user_arrived(Instant::now()) {
                    match arrived.skipped {
                        Some(d) => tracing::info!(
                            "a user signed in to console session {session}; launching the \
                             host now instead of in {}s",
                            // Rounded up, so a skip under a second reads
                            // "1s", never "0s".
                            d.as_millis().div_ceil(1000)
                        ),
                        None => tracing::info!(
                            "a user signed in to console session {session}; launching the host"
                        ),
                    }
                }
            }
            Err(e) => {
                if policy.user_left() {
                    tracing::info!(
                        "no user token for console session {session} ({e}): nobody is signed \
                         in there yet (or this process is not running as LocalSystem). The \
                         host starts as soon as someone signs in"
                    );
                }
            }
        }
    }

    /// Start the host in `session` as that session's user.
    fn launch(engine_exe: &str, args: &str, session: u32) -> Result<elevation::Launched, String> {
        let token = elevation::user_token(session)
            .map_err(|e| format!("user token unavailable for session {session}: {e}"))?;
        // Owned so the handle is closed after every launch attempt — this loop
        // runs for the life of the machine. The child has its own copy once
        // CreateProcessAsUser returns.
        let token = elevation::OwnedToken::from_raw(token);
        // W4 launch args (`--autostart --service-host`); the UIAccess flag is
        // set inside launch_in_session (W5).
        elevation::launch_in_session(token.handle(), engine_exe, args)
            .map_err(|e| format!("launch failed: {e}"))
    }

    /// One line per launch, saying whether the host got the UIAccess flag and,
    /// if not, the exact reason (W5 fallback: the host runs anyway, but remote
    /// Alt+Tab and the Windows key will not reach the shell).
    fn log_launch(launched: &elevation::Launched, session: u32) {
        let pid = launched.pid;
        match &launched.ui_access {
            Ok(()) => {
                tracing::info!("launched engine pid={pid} in session {session} with UIAccess")
            }
            Err(e) => tracing::warn!(
                "launched engine pid={pid} in session {session} WITHOUT UIAccess \
                 (remote Alt+Tab and the Windows key will not reach the host's shell): {e}"
            ),
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
        fn the_plain_codes_match_the_windows_headers() {
            use crate::events::*;
            use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
            use windows::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
            use windows::Win32::System::Services::{
                SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE,
                SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
            };
            assert_eq!(CONTROL_STOP, SERVICE_CONTROL_STOP);
            assert_eq!(CONTROL_SHUTDOWN, SERVICE_CONTROL_SHUTDOWN);
            assert_eq!(CONTROL_INTERROGATE, SERVICE_CONTROL_INTERROGATE);
            assert_eq!(CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SESSIONCHANGE);
            assert_eq!(crate::events::NO_ERROR, NO_ERROR.0);
            assert_eq!(
                crate::events::ERROR_CALL_NOT_IMPLEMENTED,
                ERROR_CALL_NOT_IMPLEMENTED.0
            );
            assert_eq!(
                NOTIFICATION_SIZE as usize,
                std::mem::size_of::<WTSSESSION_NOTIFICATION>()
            );
            assert_eq!(dispatcher::ACCEPTED, 0x1 | 0x4 | 0x80);

            use windows::Win32::UI::WindowsAndMessaging as wm;
            for (ours, theirs) in [
                (SessionChange::ConsoleConnect, wm::WTS_CONSOLE_CONNECT),
                (SessionChange::ConsoleDisconnect, wm::WTS_CONSOLE_DISCONNECT),
                (SessionChange::RemoteConnect, wm::WTS_REMOTE_CONNECT),
                (SessionChange::RemoteDisconnect, wm::WTS_REMOTE_DISCONNECT),
                (SessionChange::Logon, wm::WTS_SESSION_LOGON),
                (SessionChange::Logoff, wm::WTS_SESSION_LOGOFF),
                (SessionChange::Lock, wm::WTS_SESSION_LOCK),
                (SessionChange::Unlock, wm::WTS_SESSION_UNLOCK),
                (SessionChange::RemoteControl, wm::WTS_SESSION_REMOTE_CONTROL),
            ] {
                assert_eq!(ours.code(), theirs, "{ours:?}");
                assert_eq!(SessionChange::from_code(theirs), ours);
            }
        }

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
