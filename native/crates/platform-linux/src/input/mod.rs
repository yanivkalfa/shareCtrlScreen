//! Remote-control input injection on Linux (plan task L6, decision #4).
//!
//! Four backends, chosen at construction from the session type and what the
//! desktop offers ([`select`]):
//!
//! | Session | Backend | Permission |
//! |---|---|---|
//! | X11 | XTest ([`xtest`]) | none |
//! | Wayland, wlroots (Sway, Hyprland, …) | `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` ([`wlr`]) | none |
//! | Wayland, GNOME 46+ / KDE Plasma 6 | RemoteDesktop portal + libei ([`libei`]) | the portal's consent dialog, remembered through the restore token |
//! | any, opt-in | uinput virtual devices ([`uinput`]) | write access to `/dev/uinput` (the Settings installer, `uinput_access`) |
//!
//! [`LinuxInjector`] is the part every backend shares, so the trait's
//! invariants hold the same way everywhere: it tracks held keys and buttons
//! (and releases exactly those in `release_all`), moves before every button,
//! turns key auto-repeat into a single press, and accumulates wheel deltas into
//! detents. Backends only translate one already-validated event into their
//! protocol.
//!
//! Key auto-repeat: the viewer forwards the browser's repeated key-downs, but
//! every Linux desktop repeats a held key by itself (the X server for XTest and
//! uinput keyboards, Wayland clients from `wl_keyboard.repeat_info`), so a
//! second press of a held key is dropped here rather than doubling the rate.
//!
//! Injected events are tagged by their source device, which is all Linux
//! offers: XTest events come from the server's "XTEST" slave devices, the
//! wlroots, libei and uinput devices carry [`DEVICE_NAME`]. A local shortcut
//! hook that must ignore our own events filters on those.
//!
//! ## References
//! * XTEST extension 2.2: <https://www.x.org/releases/X11R7.7/doc/xextproto/xtest.html>
//! * EWMH `_NET_ACTIVE_WINDOW`: <https://specifications.freedesktop.org/wm-spec/latest/ar01s03.html#id-1.4.10>
//! * ICCCM §4.1.3.1 `WM_STATE` (finding the client window): <https://x.org/releases/X11R7.6/doc/xorg-docs/specs/ICCCM/icccm.html#WM_STATE_Property>
//! * RemoteDesktop portal (`SelectDevices`, `Start`, `ConnectToEIS`, version 2):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
//! * libei / EI protocol: <https://libinput.pages.freedesktop.org/libei/api/index.html>,
//!   <https://gitlab.freedesktop.org/libinput/libei/-/blob/main/proto/protocol.xml>
//! * wlr-virtual-pointer-unstable-v1: <https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1>
//! * virtual-keyboard-unstable-v1: <https://wayland.app/protocols/virtual-keyboard-unstable-v1>
//! * xdg-output-unstable-v1 (logical output geometry): <https://wayland.app/protocols/xdg-output-unstable-v1>
//! * Linux uinput: <https://www.kernel.org/doc/html/latest/input/uinput.html>; event
//!   codes and `REL_WHEEL_HI_RES`: <https://www.kernel.org/doc/html/latest/input/event-codes.html>
//! * plans/02-LINUX-FINDINGS.md §3 (backend matrix), plans/00-DECISIONS.md #4.

mod libei;
mod mapping;
mod uinput;
mod wlr;
mod xkb;
mod xtest;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::path::Path;

use platform_api::{InputError, InputInjector, Rect};
use protocol::{Button, InputMsg};

use crate::SessionType;
use mapping::{Point, WheelAccumulator, WheelSteps};
pub(crate) use uinput::UinputAccess;

/// The name our virtual devices carry (uinput, wlroots, libei sender context),
/// so they can be told apart from real hardware in device lists and filters.
pub(crate) const DEVICE_NAME: &str = "ShareCtrlScreen remote input";

/// What one backend does with one already-validated event. The front end
/// ([`LinuxInjector`]) guarantees: a button or key is only released after it
/// was pressed, never pressed twice in a row, and every button event is
/// preceded by a pointer move to its coordinates.
trait Backend {
    fn pointer(&mut self, at: Point);
    fn button(&mut self, button: Button, down: bool);
    /// Never called with empty steps.
    fn wheel(&mut self, steps: WheelSteps);
    fn key(&mut self, evdev: u16, down: bool);
    /// See [`InputInjector::focus_window_at`]. Only X11 can do this.
    fn focus_window_at(&mut self, _at: Point) -> bool {
        false
    }
    /// End of one dispatched message: push buffered requests to the server.
    fn flush(&mut self);
}

/// The backend kinds, in the order [`select`] prefers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendKind {
    XTest,
    Wlroots,
    Portal,
    Uinput,
}

impl BackendKind {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::XTest => "XTest (X11)",
            Self::Wlroots => "the wlroots virtual pointer and keyboard",
            Self::Portal => "the RemoteDesktop portal (libei)",
            Self::Uinput => "uinput virtual devices",
        }
    }
}

/// What the desktop offers, gathered before choosing ([`select`] is pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub session: SessionType,
    /// X11: the server answers for the XTEST extension.
    pub xtest: bool,
    /// Wayland: both `zwlr_virtual_pointer_manager_v1` and
    /// `zwp_virtual_keyboard_manager_v1` are advertised.
    pub wlr_virtual_input: bool,
    /// Wayland: the RemoteDesktop portal interface version, `None` if absent.
    pub portal_remote_desktop: Option<u32>,
    pub uinput: UinputAccess,
}

/// `ConnectToEIS` (the libei route) arrived with RemoteDesktop version 2;
/// version 1 only has the per-event D-Bus calls, which are too slow for a
/// pointer (one D-Bus round trip per motion).
const MIN_PORTAL_EIS_VERSION: u32 = 2;

/// Why no backend can run, with what the user can do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The only blocker is a permission the user can grant (uinput access).
    pub permission: bool,
    pub message: String,
}

impl From<Refusal> for InputError {
    fn from(r: Refusal) -> Self {
        if r.permission {
            InputError::PermissionDenied(r.message)
        } else {
            InputError::Unavailable(r.message)
        }
    }
}

/// Every usable backend, best first, or why there is none.
///
/// Wayland prefers the wlroots protocols where they exist: they need no
/// dialog and no restore token, so unattended access keeps working (the
/// AnyDesk behaviour); GNOME and KDE do not offer them, so the portal is the
/// route there. uinput is last everywhere: it is opt-in (only the Settings
/// installer or root makes `/dev/uinput` writable) and bypasses the desktop.
pub(crate) fn select(c: &Capabilities) -> Result<Vec<BackendKind>, Refusal> {
    let mut usable = Vec::new();
    match c.session {
        SessionType::X11 => {
            if c.xtest {
                usable.push(BackendKind::XTest);
            }
        }
        SessionType::Wayland => {
            if c.wlr_virtual_input {
                usable.push(BackendKind::Wlroots);
            }
            if c.portal_remote_desktop
                .is_some_and(|v| v >= MIN_PORTAL_EIS_VERSION)
            {
                usable.push(BackendKind::Portal);
            }
        }
    }
    if c.uinput == UinputAccess::Writable {
        usable.push(BackendKind::Uinput);
    }
    if !usable.is_empty() {
        return Ok(usable);
    }
    let desktop = match c.session {
        SessionType::X11 => "the X server has no XTEST extension (X.org loads it by default; \
                             check for `Option \"XTEST\" \"off\"` in xorg.conf)"
            .to_owned(),
        SessionType::Wayland => match c.portal_remote_desktop {
            Some(v) => format!(
                "this desktop's remote-control portal (version {v}) predates libei, which \
                 GNOME 46 / KDE Plasma 6 or newer provide, and it has no wlroots virtual input"
            ),
            None => "this desktop has no remote-control portal (GNOME 46+ and KDE Plasma 6 \
                     have one) and no wlroots virtual-input protocols"
                .to_owned(),
        },
    };
    Err(Refusal {
        permission: matches!(c.uinput, UinputAccess::Denied(_)),
        message: format!(
            "remote control is unavailable: {desktop}. The uinput fallback cannot be used \
             either: {}",
            c.uinput.explain()
        ),
    })
}

/// Probe the live desktop, portal included (one bounded D-Bus round trip).
fn probe(session: SessionType) -> Capabilities {
    let mut caps = probe_local(session);
    if session == SessionType::Wayland {
        caps.portal_remote_desktop = crate::permissions::remote_desktop_version();
    }
    caps
}

/// Everything but the portal, which the permissions report probes itself:
/// local sockets and files only, so it is fast and never blocks on a peer.
pub(crate) fn probe_local(session: SessionType) -> Capabilities {
    let (xtest, wlr_virtual_input) = match session {
        SessionType::X11 => (xtest::available(None), false),
        SessionType::Wayland => (false, wlr::available()),
    };
    Capabilities {
        session,
        xtest,
        wlr_virtual_input,
        portal_remote_desktop: None,
        uinput: uinput::access(),
    }
}

/// [`Platform::create_injector`](platform_api::Platform::create_injector):
/// the first usable backend that starts. Never blocks on a user dialog: the
/// portal backend connects in the background and drops input until the
/// desktop has granted it.
pub(crate) fn create(
    session: SessionType,
    state_dir: &Path,
) -> Result<Box<dyn InputInjector>, InputError> {
    let caps = probe(session);
    let mut failures = Vec::new();
    for kind in select(&caps)? {
        let started: Result<Box<dyn Backend>, String> = match kind {
            BackendKind::XTest => xtest::XTest::connect(None).map(|b| Box::new(b) as _),
            BackendKind::Wlroots => wlr::Wlr::connect_to_env().map(|b| Box::new(b) as _),
            BackendKind::Portal => libei::Libei::start(state_dir).map(|b| Box::new(b) as _),
            BackendKind::Uinput => uinput::Uinput::create(session).map(|b| Box::new(b) as _),
        };
        match started {
            Ok(backend) => {
                tracing::info!("remote control uses {}", kind.describe());
                return Ok(Box::new(LinuxInjector::new(backend)));
            }
            Err(e) => {
                tracing::warn!("{} failed to start: {e}", kind.describe());
                failures.push(e);
            }
        }
    }
    Err(InputError::Unavailable(format!(
        "remote control could not start: {}",
        failures.join("; ")
    )))
}

/// The shared front end over one backend; see the module docs.
pub(crate) struct LinuxInjector {
    backend: Box<dyn Backend>,
    target: Option<Rect>,
    /// evdev codes we pressed and have not released. Ordered so `release_all`
    /// is deterministic (and testable).
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
    wheel: WheelAccumulator,
}

impl LinuxInjector {
    fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend,
            target: None,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            wheel: WheelAccumulator::default(),
        }
    }

    fn point(&self, nx: f64, ny: f64) -> Point {
        Point {
            nx,
            ny,
            target: self.target,
        }
    }

    fn button(&mut self, b: Button, down: bool, nx: f64, ny: f64) {
        // Move first: the preceding move rides the unreliable channel and may
        // have been dropped, and the click must land where it was aimed.
        self.backend.pointer(self.point(nx, ny));
        let changed = if down {
            self.buttons.insert(u8::from(b))
        } else {
            self.buttons.remove(&u8::from(b))
        };
        if changed {
            self.backend.button(b, down);
        }
    }

    fn key(&mut self, code: &str, down: bool) {
        // Unknown codes are ignored, as on every other OS.
        let Some(evdev) = mapping::evdev_for(code) else {
            return;
        };
        let changed = if down {
            self.keys.insert(evdev)
        } else {
            self.keys.remove(&evdev)
        };
        if changed {
            self.backend.key(evdev, down);
        }
    }
}

impl InputInjector for LinuxInjector {
    fn set_target_rect(&mut self, rect: Option<Rect>) {
        self.target = rect.filter(|r| r.w > 0 && r.h > 0);
    }

    fn dispatch(&mut self, msg: &InputMsg) {
        match msg {
            InputMsg::Move { x, y } => self.backend.pointer(self.point(*x, *y)),
            InputMsg::ButtonDown { b, x, y } => self.button(*b, true, *x, *y),
            InputMsg::ButtonUp { b, x, y } => self.button(*b, false, *x, *y),
            InputMsg::Wheel { dx, dy } => {
                let steps = self.wheel.add(*dx, *dy);
                if !steps.is_empty() {
                    self.backend.wheel(steps);
                }
            }
            InputMsg::KeyDown { code } => self.key(code, true),
            InputMsg::KeyUp { code } => self.key(code, false),
        }
        self.backend.flush();
    }

    fn release_all(&mut self) {
        // Desktops do not care in which order held keys come up; a fixed
        // order keeps the result deterministic.
        for evdev in std::mem::take(&mut self.keys).into_iter().rev() {
            self.backend.key(evdev, false);
        }
        // No move: the cursor stays where it is.
        for id in std::mem::take(&mut self.buttons) {
            if let Ok(b) = Button::try_from(id) {
                self.backend.button(b, false);
            }
        }
        self.wheel.reset();
        self.backend.flush();
    }

    fn focus_window_at(&mut self, nx: f64, ny: f64) -> bool {
        let at = self.point(nx, ny);
        self.backend.focus_window_at(at)
    }
}
