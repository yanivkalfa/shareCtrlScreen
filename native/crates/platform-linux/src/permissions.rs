//! Permission preflight for the Linux host: what the desktop will let us do, and the
//! portal flow that asks for it.
//!
//! * X11 has no permission concept: capture (XShm) and input (XTest) just work, so
//!   both report `NotApplicable`.
//! * Wayland routes everything through `xdg-desktop-portal`. The probe asks the portal
//!   which interfaces it implements (a desktop without `RemoteDesktop`, such as
//!   Hyprland or Weston/WSLg, can never inject input through it) and whether a restore
//!   token from an earlier consent is stored. The probe never shows UI.
//! * [`request`] runs just enough of the ScreenCast (+ RemoteDesktop) handshake for the
//!   user to answer the consent dialog and for the portal to hand back a restore token,
//!   stores it for the capture task, and closes the session again.
//!
//! The decision logic ([`assess`]) is a pure function of a [`Probe`], so it is tested
//! without D-Bus, a desktop, or any device.
//!
//! ## References
//! * ScreenCast portal (`persist_mode`, `restore_token`, interface `version` >= 4):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
//! * RemoteDesktop portal (`SelectDevices`, combined sessions, `version` >= 2 for libei):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
//! * `ashpd` ScreenCast / RemoteDesktop wrappers: <https://docs.rs/ashpd/0.13.13/>
//! * `zbus` properties proxy: <https://docs.rs/zbus/5/zbus/fdo/struct.PropertiesProxy.html>
//! * plans/02-LINUX-FINDINGS.md §1 (restore tokens), §3 (input matrix), §4 (compositors).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ashpd::zbus::{self, fdo, names::InterfaceName};
use platform_api::{OsPermission, PermissionReport, PermissionState};

use crate::portal_session::{self, block_on, runtime, Failure, Scope};
use crate::portal_token::{self, StoredToken};
use crate::SessionType;

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";

/// Restore tokens (`persist_mode`) arrived with ScreenCast interface version 4.
const MIN_RESTORE_TOKEN_VERSION: u32 = 4;

/// A portal that does not answer promptly is as good as absent for a preflight;
/// the D-Bus default (25 s) would freeze the settings screen.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const NO_REMOTE_CONTROL_NOTE: &str =
    "this desktop has no remote-control portal; enable the uinput fallback in Settings";

/// Where a portal interface stands on the session bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interface {
    Present {
        version: u32,
    },
    /// The portal answered but does not implement this interface.
    Absent,
}

/// The portal service as seen over D-Bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Portal {
    /// No session bus, no `xdg-desktop-portal`, or no answer in time.
    Unreachable,
    Reachable {
        screencast: Interface,
        remote_desktop: Interface,
    },
}

/// Hardware-video prerequisites. Informational only: a missing GPU or libva is not a
/// permission, the software codec takes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Vaapi {
    render_node: bool,
    libva: bool,
}

/// Everything the assessment needs, gathered up front so [`assess`] stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    session: SessionType,
    portal: Portal,
    token: Option<StoredToken>,
    uinput_writable: bool,
    vaapi: Vaapi,
}

/// Current permission state. Fast and UI-free; on Wayland it makes one bounded
/// round-trip to the portal.
pub(crate) fn report(session: SessionType, state_dir: &Path) -> PermissionReport {
    let wayland = session == SessionType::Wayland;
    let probe = Probe {
        session,
        portal: if wayland {
            block_on(probe_portal(), PROBE_TIMEOUT).unwrap_or(Portal::Unreachable)
        } else {
            Portal::Unreachable
        },
        token: if wayland {
            portal_token::read(&portal_token::path_in(state_dir))
        } else {
            None
        },
        uinput_writable: wayland && uinput_writable(),
        vaapi: probe_vaapi(),
    };
    assess(&probe)
}

fn assess(probe: &Probe) -> PermissionReport {
    let mut report = match probe.session {
        SessionType::X11 => {
            let mut r = PermissionReport::all_not_applicable();
            r.notes
                .push("X11 session: screen capture and input injection need no permission.".into());
            r
        }
        SessionType::Wayland => assess_wayland(probe),
    };
    report.notes.push(vaapi_note(probe.vaapi));
    report
}

fn assess_wayland(probe: &Probe) -> PermissionReport {
    let mut report = PermissionReport::all_not_applicable();
    let (screencast, remote_desktop) = match probe.portal {
        Portal::Unreachable => {
            report.screen_capture = PermissionState::Denied;
            report.notes.push(
                "no desktop portal is reachable (is xdg-desktop-portal running?); \
                 screen sharing on Wayland needs it"
                    .into(),
            );
            (Interface::Absent, Interface::Absent)
        }
        Portal::Reachable {
            screencast,
            remote_desktop,
        } => (screencast, remote_desktop),
    };

    if probe.portal != Portal::Unreachable {
        report.screen_capture = match screencast {
            Interface::Absent => {
                report
                    .notes
                    .push("this desktop has no screen-capture portal".into());
                PermissionState::Denied
            }
            Interface::Present { version } if version < MIN_RESTORE_TOKEN_VERSION => {
                report.notes.push(format!(
                    "the screen-capture portal (version {version}) cannot remember consent; \
                     the dialog appears on every connection"
                ));
                PermissionState::NotDetermined
            }
            Interface::Present { .. } if probe.token.is_some() => PermissionState::Granted,
            Interface::Present { .. } => PermissionState::NotDetermined,
        };
    }

    report.input_injection = match remote_desktop {
        Interface::Present { .. } => {
            if probe.token.as_ref().is_some_and(|t| t.covers_input) {
                PermissionState::Granted
            } else {
                PermissionState::NotDetermined
            }
        }
        // No portal route to input: the opt-in uinput device is the only way in.
        Interface::Absent if probe.uinput_writable => {
            report
                .notes
                .push("remote control uses /dev/uinput (no remote-control portal here)".into());
            PermissionState::Granted
        }
        Interface::Absent => {
            report.notes.push(NO_REMOTE_CONTROL_NOTE.into());
            PermissionState::Denied
        }
    };
    report
}

fn vaapi_note(v: Vaapi) -> String {
    match (v.render_node, v.libva) {
        (true, true) => "hardware video (VA-API): GPU render node and libva found".into(),
        (true, false) => "hardware video (VA-API): libva is not installed; \
                          the software codec will be used"
            .into(),
        (false, _) => "hardware video (VA-API): no GPU render node; \
                       the software codec will be used"
            .into(),
    }
}

/// Run the consent flow for `p` in the background; the UI re-polls [`report`].
/// X11 and `InputMonitoring` have nothing to ask.
pub(crate) fn request(session: SessionType, state_dir: &Path, p: OsPermission) {
    let want_input = match (session, p) {
        (SessionType::Wayland, OsPermission::ScreenCapture) => false,
        (SessionType::Wayland, OsPermission::InputInjection) => true,
        _ => return,
    };
    // A second click while the dialog is open must not stack another dialog.
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        tracing::debug!("a portal consent request is already open");
        return;
    }
    let path = portal_token::path_in(state_dir);
    let Some(rt) = runtime() else {
        IN_FLIGHT.store(false, Ordering::Release);
        return;
    };
    rt.spawn(async move {
        match run_flow(want_input, &path).await {
            Ok(()) => {}
            Err(e) if e.is_cancel() => {
                tracing::info!("portal consent dismissed by the user");
            }
            Err(e) => tracing::warn!("portal consent flow failed: {e}"),
        }
        IN_FLIGHT.store(false, Ordering::Release);
    });
}

/// ScreenCast (+ RemoteDesktop when input is wanted) up to `Start`, then store the
/// restore token and close. The capture session (`portal_capture`) restores it.
async fn run_flow(want_input: bool, token_path: &Path) -> Result<(), Failure> {
    let existing = portal_token::read(token_path);
    if !want_input && existing.as_ref().is_some_and(|t| t.covers_input) {
        // A combined token already restores capture; a screen-only one would
        // overwrite it and silently drop remote control.
        return Ok(());
    }
    let previous = previous_token(want_input, existing.as_ref());
    let scope = if want_input {
        Scope::ScreenAndInput
    } else {
        Scope::Screen
    };
    // No bound, never abandoned: the settings screen polls in the background while
    // the user decides.
    let granted = portal_session::start(scope, previous, None, std::future::pending()).await?;
    let stored = granted.token.clone();
    granted.session.close().await;
    // Only a token handed to `Start` is spent; a screen-only token kept back from a
    // combined request is still valid if the portal returns no replacement.
    portal_token::replace(token_path, previous.is_some(), stored.as_ref());
    Ok(())
}

/// The stored token to hand back so a still-valid grant is renewed without a dialog.
/// Only one of the same scope: a screen-only token cannot restore devices.
fn previous_token(want_input: bool, existing: Option<&StoredToken>) -> Option<&str> {
    existing
        .filter(|t| !want_input || t.covers_input)
        .map(|t| t.token.as_str())
}

async fn probe_portal() -> Portal {
    let connection = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("no D-Bus session bus: {e}");
            return Portal::Unreachable;
        }
    };
    let screencast = interface(&connection, SCREENCAST_INTERFACE).await;
    let remote_desktop = interface(&connection, REMOTE_DESKTOP_INTERFACE).await;
    match (screencast, remote_desktop) {
        (Some(screencast), Some(remote_desktop)) => Portal::Reachable {
            screencast,
            remote_desktop,
        },
        _ => Portal::Unreachable,
    }
}

/// `None` = the portal service itself could not be queried.
async fn interface(connection: &zbus::Connection, name: &'static str) -> Option<Interface> {
    match read_version(connection, name).await {
        Ok(version) => Some(Interface::Present { version }),
        // The portal front end answers `InvalidArgs` for an interface it does not
        // implement (ashpd relies on the same behaviour); a backend-less desktop
        // such as Weston/WSLg never reaches this far (no service at all).
        Err(fdo::Error::UnknownInterface(_))
        | Err(fdo::Error::UnknownProperty(_))
        | Err(fdo::Error::InvalidArgs(_)) => Some(Interface::Absent),
        Err(e) => {
            tracing::debug!("portal query for {name} failed: {e}");
            None
        }
    }
}

async fn read_version(connection: &zbus::Connection, name: &'static str) -> fdo::Result<u32> {
    let interface = InterfaceName::try_from(name).map_err(zbus::Error::from)?;
    let properties = fdo::PropertiesProxy::builder(connection)
        .destination(PORTAL_BUS_NAME)?
        .path(PORTAL_PATH)?
        .build()
        .await?;
    let value = properties.get(interface, "version").await?;
    u32::try_from(value).map_err(|e| zbus::Error::from(e).into())
}

/// Opening `/dev/uinput` for writing creates nothing; it only proves the udev rule
/// (or root) lets this user create virtual input devices.
fn uinput_writable() -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok()
}

fn probe_vaapi() -> Vaapi {
    let render_node = std::fs::read_dir("/dev/dri")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
        .unwrap_or(false);
    // SAFETY: libva has no load-time side effects beyond symbol resolution; the handle
    // is dropped immediately and nothing is called through it.
    let libva = unsafe { libloading::Library::new("libva.so.2") }.is_ok();
    Vaapi { render_node, libva }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_VAAPI: Vaapi = Vaapi {
        render_node: false,
        libva: false,
    };

    fn present(version: u32) -> Interface {
        Interface::Present { version }
    }

    fn wayland(portal: Portal) -> Probe {
        Probe {
            session: SessionType::Wayland,
            portal,
            token: None,
            uinput_writable: false,
            vaapi: NO_VAAPI,
        }
    }

    fn gnome() -> Portal {
        Portal::Reachable {
            screencast: present(5),
            remote_desktop: present(2),
        }
    }

    fn token(covers_input: bool) -> Option<StoredToken> {
        Some(StoredToken {
            token: "t".into(),
            covers_input,
        })
    }

    #[test]
    fn x11_needs_no_permissions() {
        let mut p = wayland(Portal::Unreachable);
        p.session = SessionType::X11;
        let r = assess(&p);
        assert_eq!(r.screen_capture, PermissionState::NotApplicable);
        assert_eq!(r.input_injection, PermissionState::NotApplicable);
        assert_eq!(r.input_monitoring, PermissionState::NotApplicable);
        assert!(r.notes.iter().any(|n| n.contains("X11")));
    }

    #[test]
    fn wayland_without_a_stored_token_is_not_determined() {
        let r = assess(&wayland(gnome()));
        assert_eq!(r.screen_capture, PermissionState::NotDetermined);
        assert_eq!(r.input_injection, PermissionState::NotDetermined);
    }

    #[test]
    fn a_combined_token_grants_both() {
        let mut p = wayland(gnome());
        p.token = token(true);
        let r = assess(&p);
        assert_eq!(r.screen_capture, PermissionState::Granted);
        assert_eq!(r.input_injection, PermissionState::Granted);
    }

    #[test]
    fn a_screen_only_token_does_not_grant_input() {
        let mut p = wayland(gnome());
        p.token = token(false);
        let r = assess(&p);
        assert_eq!(r.screen_capture, PermissionState::Granted);
        assert_eq!(r.input_injection, PermissionState::NotDetermined);
    }

    #[test]
    fn old_screencast_portal_cannot_remember_consent() {
        let mut p = wayland(Portal::Reachable {
            screencast: present(3),
            remote_desktop: present(1),
        });
        p.token = token(true);
        let r = assess(&p);
        assert_eq!(r.screen_capture, PermissionState::NotDetermined);
        assert!(r.notes.iter().any(|n| n.contains("version 3")));
    }

    #[test]
    fn hyprland_has_no_remote_control_portal() {
        let r = assess(&wayland(Portal::Reachable {
            screencast: present(5),
            remote_desktop: Interface::Absent,
        }));
        assert_eq!(r.screen_capture, PermissionState::NotDetermined);
        assert_eq!(r.input_injection, PermissionState::Denied);
        assert!(r.notes.contains(&NO_REMOTE_CONTROL_NOTE.to_owned()));
    }

    #[test]
    fn writable_uinput_stands_in_for_a_missing_remote_control_portal() {
        let mut p = wayland(Portal::Reachable {
            screencast: present(5),
            remote_desktop: Interface::Absent,
        });
        p.uinput_writable = true;
        let r = assess(&p);
        assert_eq!(r.input_injection, PermissionState::Granted);
        assert!(!r.notes.contains(&NO_REMOTE_CONTROL_NOTE.to_owned()));
    }

    #[test]
    fn writable_uinput_does_not_override_a_working_portal() {
        let mut p = wayland(gnome());
        p.uinput_writable = true;
        assert_eq!(assess(&p).input_injection, PermissionState::NotDetermined);
    }

    #[test]
    fn no_portal_at_all_is_denied_with_both_explanations() {
        let r = assess(&wayland(Portal::Unreachable));
        assert_eq!(r.screen_capture, PermissionState::Denied);
        assert_eq!(r.input_injection, PermissionState::Denied);
        assert!(r.notes.iter().any(|n| n.contains("portal")));
        assert!(r.notes.contains(&NO_REMOTE_CONTROL_NOTE.to_owned()));
    }

    #[test]
    fn missing_screencast_interface_is_denied() {
        let r = assess(&wayland(Portal::Reachable {
            screencast: Interface::Absent,
            remote_desktop: present(2),
        }));
        assert_eq!(r.screen_capture, PermissionState::Denied);
    }

    #[test]
    fn vaapi_is_a_note_never_a_permission() {
        for (render_node, libva, needle) in [
            (true, true, "found"),
            (true, false, "libva is not installed"),
            (false, true, "no GPU render node"),
            (false, false, "no GPU render node"),
        ] {
            let mut p = wayland(gnome());
            p.vaapi = Vaapi { render_node, libva };
            let r = assess(&p);
            assert!(r.notes.last().unwrap().contains(needle), "{r:?}");
            assert_eq!(r.screen_capture, PermissionState::NotDetermined);
        }
    }

    #[test]
    fn screen_only_request_never_replaces_a_combined_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = portal_token::path_in(dir.path());
        let combined = StoredToken {
            token: "keep".into(),
            covers_input: true,
        };
        portal_token::write(&path, &combined).unwrap();
        // Returns before touching D-Bus, so this runs without a portal.
        runtime().unwrap().block_on(run_flow(false, &path)).unwrap();
        assert_eq!(portal_token::read(&path), Some(combined));
    }

    #[test]
    fn a_combined_request_keeps_back_and_so_never_spends_a_screen_only_token() {
        let screen_only = token(false).unwrap();
        let combined = token(true).unwrap();
        // Combined request: a screen-only token cannot restore devices, so it is not
        // handed over; `replace` is then told it was not spent and keeps it when the
        // portal returns no replacement (the user unticked "remember").
        assert_eq!(previous_token(true, Some(&screen_only)), None);
        assert_eq!(previous_token(true, Some(&combined)), Some("t"));
        assert_eq!(previous_token(false, Some(&screen_only)), Some("t"));
        assert_eq!(previous_token(true, None), None);

        let dir = tempfile::tempdir().unwrap();
        let path = portal_token::path_in(dir.path());
        portal_token::write(&path, &screen_only).unwrap();
        let previous = previous_token(true, Some(&screen_only));
        portal_token::replace(&path, previous.is_some(), None);
        assert_eq!(portal_token::read(&path), Some(screen_only));
    }

    /// WSLg runs Weston: a Wayland session with no portal service. Skips itself on a
    /// desktop that does have one (those paths are in plans/checklists/L12.md).
    #[test]
    fn weston_without_a_portal_reports_the_no_portal_path() {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        let Some(portal) = block_on(probe_portal(), PROBE_TIMEOUT) else {
            panic!("the portal probe must finish within its timeout");
        };
        if portal != Portal::Unreachable {
            eprintln!("skipped: this Wayland desktop has a portal");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = report(SessionType::Wayland, dir.path());
        assert_eq!(r.screen_capture, PermissionState::Denied);
        assert!(matches!(
            r.input_injection,
            PermissionState::Denied | PermissionState::Granted
        ));
        assert!(r.notes.iter().any(|n| n.contains("portal")));
    }

    #[test]
    #[ignore = "needs a GNOME/KDE desktop"]
    fn real_portal_reports_versions() {
        let portal = block_on(probe_portal(), PROBE_TIMEOUT).unwrap();
        assert!(matches!(portal, Portal::Reachable { .. }), "{portal:?}");
        let dir = tempfile::tempdir().unwrap();
        let r = report(SessionType::Wayland, dir.path());
        assert_eq!(r.screen_capture, PermissionState::NotDetermined);
    }
}
