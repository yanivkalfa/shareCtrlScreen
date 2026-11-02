//! Exactly one host per Windows session when the SYSTEM service is installed
//! (task W4).
//!
//! The service launches the host into the active session with
//! `--autostart --service-host` (`service/src/main.rs`). That instance is the
//! one that matters: it is the machine's always-on host. Any other launch in
//! the same session (a double-click, the Start-menu shortcut, the installer's
//! "Run ShareCtrlScreen" box, a leftover per-user Run entry) must not start a
//! second host beside it: two hosts would both register with signaling and
//! fight over capture and input. The rule:
//!
//! | this launch                     | a service host runs here      | none   |
//! |---------------------------------|-------------------------------|--------|
//! | the service (`--service-host`)  | exit quietly (a duplicate)    | run, claim the marker |
//! | sign-in (`--autostart`)         | exit quietly                  | run    |
//! | the user                        | raise the host's window, exit | run    |
//!
//! Without the service nothing changes: no marker exists, every launch runs.
//!
//! Mechanism: the service host creates a named auto-reset event,
//! [`MARKER_NAME`], in the session's `Local\` namespace and keeps the handle
//! for its whole life, so the object exists exactly as long as that process
//! does (a kernel object is destroyed when its last handle closes, including
//! at process exit: no stale lock after a crash). Another launch opens it: if
//! that succeeds the host is running, and setting the event asks the host to
//! bring its window up. `Local\` is per session, so the hosts of users signed
//! in at the same time (fast user switching) never see each other's marker.
//!
//! ## References
//!
//! * Kernel object namespaces (`Local\`, per session):
//!   <https://learn.microsoft.com/windows/win32/termserv/kernel-object-namespaces>
//! * CreateEventW (ERROR_ALREADY_EXISTS when the named object exists):
//!   <https://learn.microsoft.com/windows/win32/api/synchapi/nf-synchapi-createeventw>
//! * OpenEventW / EVENT_MODIFY_STATE:
//!   <https://learn.microsoft.com/windows/win32/api/synchapi/nf-synchapi-openeventw>,
//!   <https://learn.microsoft.com/windows/win32/sync/synchronization-object-security-and-access-rights>
//! * Object lifetime (destroyed when the last handle is closed):
//!   <https://learn.microsoft.com/windows/win32/sysinfo/kernel-objects>
//! * AllowSetForegroundWindow (the launching process lends its foreground
//!   right to the host): <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-allowsetforegroundwindow>

use elevation::SERVICE_HOST_FLAG;
use platform_api::AUTOSTART_FLAG;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// The service host's marker. `Local\`: per session (see the module docs).
pub const MARKER_NAME: &str = r"Local\ShareCtrlScreen.ServiceHost";

/// Who started this process, from its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// The SYSTEM service, into the active session.
    ServiceHost,
    /// The per-user Run entry at sign-in.
    SignIn,
    /// Anything else: the user, a shortcut, the installer's finish page.
    User,
}

impl Launch {
    /// `--service-host` wins over `--autostart`: the service passes both
    /// (its instance is also the sign-in instance and starts minimized).
    pub fn from_args<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut launch = Launch::User;
        for a in args {
            let a = a.as_ref();
            if a == SERVICE_HOST_FLAG {
                return Launch::ServiceHost;
            }
            if a == AUTOSTART_FLAG {
                launch = Launch::SignIn;
            }
        }
        launch
    }
}

/// What a launch does, given whether a service host already runs in this
/// session. Pure: the table in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// Exit without touching the running host (it is not the user asking).
    ExitQuietly,
    /// Ask the running host to show its window, then exit.
    RaiseAndExit,
}

pub fn decide(launch: Launch, service_host_running: bool) -> Decision {
    match (launch, service_host_running) {
        (_, false) => Decision::Run,
        (Launch::ServiceHost | Launch::SignIn, true) => Decision::ExitQuietly,
        (Launch::User, true) => Decision::RaiseAndExit,
    }
}

/// The outcome of [`gate`] for `main`.
pub enum Gate {
    /// Start the app. `marker` is set for the service host: hand it to
    /// [`ServiceHostMarker::listen`] once the window exists. `note` is a line
    /// worth logging (a marker that could not be created).
    Run {
        marker: Option<ServiceHostMarker>,
        note: Option<String>,
    },
    /// Do not start; log `reason` and exit with success.
    Exit { reason: String },
}

/// Apply the one-host rule to this process. Call first thing in `main`,
/// before logging opens `app.log` (the running host owns that file).
pub fn gate<I, S>(args: I) -> Gate
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    gate_on(MARKER_NAME, Launch::from_args(args))
}

/// [`gate`] against a given marker name (tests use their own names, so they
/// neither see a real host on the machine nor each other).
fn gate_on(name: &str, launch: Launch) -> Gate {
    let mut marker = None;
    let mut note = None;
    let mut open = None;
    let present = if launch == Launch::ServiceHost {
        match ServiceHostMarker::claim(name) {
            Ok(Some(m)) => {
                marker = Some(m);
                false
            }
            Ok(None) => true,
            // Running without the marker is still a working host; the only
            // loss is that other launches cannot see it.
            Err(e) => {
                note = Some(format!(
                    "service host marker {name} not created ({e}); a user launch in \
                     this session will not hand over to this host"
                ));
                false
            }
        }
    } else {
        open = open_marker(name);
        open.is_some()
    };
    match decide(launch, present) {
        Decision::Run => Gate::Run { marker, note },
        Decision::ExitQuietly => Gate::Exit {
            reason: if launch == Launch::ServiceHost {
                "a service-launched host is already running in this session; this \
                 duplicate launch exits"
                    .into()
            } else {
                "the ShareCtrlScreen service already runs the host in this session and \
                 starts it at sign-in; this sign-in launch exits"
                    .into()
            },
        },
        Decision::RaiseAndExit => Gate::Exit {
            reason: if open.is_some_and(raise) {
                "the ShareCtrlScreen service already runs the host in this session; \
                 asked it to show its window, and this launch exits"
                    .into()
            } else {
                "the ShareCtrlScreen service already runs the host in this session, but \
                 it could not be asked to show its window; this launch exits"
                    .into()
            },
        },
    }
}

/// An owned kernel handle, closed on drop.
struct OwnedHandle(HANDLE);

// SAFETY: a kernel object handle is a process-wide value, valid from any
// thread; the event APIs used on it are thread-safe.
unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: our handle, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_marker(name: &str) -> Option<OwnedHandle> {
    let name = wide(name);
    // SAFETY: NUL-terminated name that outlives the call; only the right to
    // set the event is asked for.
    unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) }
        .ok()
        .map(OwnedHandle)
}

/// Ask the host behind `marker` to show its window. True when the request was
/// delivered.
fn raise(marker: OwnedHandle) -> bool {
    // This process was started by the user, so it may set the foreground
    // window; lend that right on, or Windows only flashes the host's taskbar
    // button. A failure (we were not in the foreground either) is harmless.
    // SAFETY: plain call, no pointers.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    // SAFETY: valid handle opened with EVENT_MODIFY_STATE.
    unsafe { SetEvent(marker.0) }.is_ok()
}

/// The service host's claim on [`MARKER_NAME`]; alive for the process's life.
pub struct ServiceHostMarker(OwnedHandle);

impl ServiceHostMarker {
    /// Create the marker. `Ok(None)` when another process already holds it.
    fn claim(name: &str) -> Result<Option<Self>, windows::core::Error> {
        let name = wide(name);
        // Auto-reset, initially clear. Default security: the host runs with
        // the signed-in user's token, so the object belongs to that user and
        // that user's other launches may open it.
        // SAFETY: NUL-terminated name that outlives the call; default security.
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }?;
        // SAFETY: read immediately after the call it describes.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle(handle);
        if existed {
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }

    /// Run `on_raise` each time another launch asks for the window. The
    /// thread owns the marker handle, so the marker lives until the process
    /// exits. A request made before this is called is not lost: the event
    /// stays set until the thread first waits on it.
    pub fn listen<F>(self, on_raise: F)
    where
        F: Fn() + Send + 'static,
    {
        let spawned = std::thread::Builder::new()
            .name("service-host-marker".into())
            .spawn(move || {
                let marker = self;
                loop {
                    // SAFETY: valid event handle owned by this thread.
                    let waited = unsafe { WaitForSingleObject(marker.0 .0, INFINITE) };
                    if waited != WAIT_OBJECT_0 {
                        tracing::warn!(
                            "service host marker wait failed ({:#x}); window raise requests \
                             from other launches are ignored from now on",
                            waited.0
                        );
                        // Leak the handle on purpose: the marker (and so the
                        // one-host rule) must outlive this thread.
                        std::mem::forget(marker);
                        return;
                    }
                    tracing::info!("another launch asked for the window");
                    on_raise();
                }
            });
        if let Err(e) = spawned {
            // The closure (and the marker in it) is dropped with the error,
            // so other launches stop seeing this host; say so.
            tracing::warn!(
                "service host marker thread not started ({e}); the one-host rule is off"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A marker name of the test's own, unique per process and test.
    fn test_name(test: &str) -> String {
        format!(r"Local\ShareCtrlScreen.Test.{}.{test}", std::process::id())
    }

    #[test]
    fn launch_is_read_from_the_flags() {
        assert_eq!(Launch::from_args(["sharectrl.exe"]), Launch::User);
        assert_eq!(
            Launch::from_args(["sharectrl.exe", AUTOSTART_FLAG]),
            Launch::SignIn
        );
        assert_eq!(
            Launch::from_args(["sharectrl.exe", AUTOSTART_FLAG, SERVICE_HOST_FLAG]),
            Launch::ServiceHost
        );
        assert_eq!(
            Launch::from_args(["sharectrl.exe", SERVICE_HOST_FLAG, AUTOSTART_FLAG]),
            Launch::ServiceHost
        );
    }

    #[test]
    fn without_a_service_host_every_launch_runs() {
        for l in [Launch::ServiceHost, Launch::SignIn, Launch::User] {
            assert_eq!(decide(l, false), Decision::Run, "{l:?}");
        }
    }

    #[test]
    fn with_a_service_host_only_the_user_raises_it() {
        assert_eq!(decide(Launch::User, true), Decision::RaiseAndExit);
        assert_eq!(decide(Launch::SignIn, true), Decision::ExitQuietly);
        assert_eq!(decide(Launch::ServiceHost, true), Decision::ExitQuietly);
    }

    #[test]
    fn the_first_service_host_claims_and_later_launches_defer_to_it() {
        let name = test_name("claim");
        let Gate::Run {
            marker: Some(marker),
            note: None,
        } = gate_on(&name, Launch::ServiceHost)
        else {
            panic!("the first service host must claim the marker");
        };
        assert!(matches!(
            gate_on(&name, Launch::ServiceHost),
            Gate::Exit { .. }
        ));
        assert!(matches!(gate_on(&name, Launch::SignIn), Gate::Exit { .. }));
        drop(marker);
        // The object dies with its last handle: no stale lock.
        assert!(open_marker(&name).is_none(), "marker outlived its owner");
        assert!(matches!(
            gate_on(&name, Launch::User),
            Gate::Run { marker: None, .. }
        ));
    }

    #[test]
    fn a_user_launch_raises_the_host_window() {
        let name = test_name("raise");
        let Gate::Run {
            marker: Some(marker),
            ..
        } = gate_on(&name, Launch::ServiceHost)
        else {
            panic!("the first service host must claim the marker");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        marker.listen(move || {
            let _ = tx.send(());
        });
        let Gate::Exit { reason } = gate_on(&name, Launch::User) else {
            panic!("a user launch must hand over to the host");
        };
        assert!(reason.contains("asked it to show its window"), "{reason}");
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the host saw the raise request");
    }

    #[test]
    fn without_the_service_nothing_changes() {
        let name = test_name("absent");
        for l in [Launch::SignIn, Launch::User] {
            assert!(matches!(
                gate_on(&name, l),
                Gate::Run {
                    marker: None,
                    note: None
                }
            ));
        }
    }
}
