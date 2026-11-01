//! X11 backend: XTest fake input on the X server the session runs on.
//!
//! XTest events are processed by the server exactly like hardware events from
//! its "XTEST" slave devices, so they reach whichever client the pointer or
//! focus selects, follow the active keyboard layout and trigger server-side
//! key repeat. No permission is involved (plans/02 §3).
//!
//! Coordinates: `XTestFakeMotionEvent` takes root-window coordinates, which
//! span every RandR monitor, so the target rectangle (virtual-desktop pixels,
//! the same space the X11 capture reports) maps directly.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
    InputFocus, StackMode, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use platform_api::Rect;
use protocol::Button;

use super::mapping::{self, Point, WheelSteps};
use super::Backend;
use crate::x11_connect::x11_connect;

/// X11 wheel buttons: 4 up, 5 down, 6 left, 7 right (the convention every
/// toolkit and the X.org input drivers share).
const WHEEL_UP: u8 = 4;
const WHEEL_DOWN: u8 = 5;
const WHEEL_LEFT: u8 = 6;
const WHEEL_RIGHT: u8 = 7;

/// EWMH source indication "pager or other direct user action": window
/// managers' focus-stealing prevention honours it, unlike 1 ("application").
const SOURCE_PAGER: u32 = 2;

/// Bound for the `WM_STATE` search below the top-level window: real frames
/// nest the client one or two levels down, so this only stops a runaway walk.
const CLIENT_SEARCH_LIMIT: usize = 64;

/// Connect to `display` (default `$DISPLAY`), through the abstract socket
/// when the socket file is missing ([`x11_connect`]).
fn open(display: Option<&str>) -> Result<(RustConnection, usize), String> {
    let name = match display {
        Some(d) => d.to_owned(),
        None => std::env::var("DISPLAY")
            .map_err(|_| "DISPLAY is not set for this session".to_owned())?,
    };
    x11_connect(&name).map_err(|e| format!("cannot connect to the X server {name}: {e}"))
}

/// Whether `display` (default `$DISPLAY`) is an X server with XTEST.
pub(super) fn available(display: Option<&str>) -> bool {
    open(display)
        .ok()
        .and_then(|(conn, _)| {
            conn.extension_information(xtest::X11_EXTENSION_NAME)
                .ok()
                .flatten()
        })
        .is_some()
}

pub(super) struct XTest {
    conn: RustConnection,
    root: Window,
    /// What `None` targets map onto: the RandR primary monitor, else the
    /// first monitor, else the whole root window. Read once; a session that
    /// cares about a specific monitor sets the target rectangle.
    primary: Rect,
    atoms: Atoms,
    /// Set after the first connection error so a dead server is reported once,
    /// not at 60 messages a second.
    dead: bool,
}

struct Atoms {
    net_active_window: Atom,
    net_supported: Atom,
    wm_state: Atom,
}

fn x11_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("XTest: {what} failed: {e}")
}

impl XTest {
    /// Connect to `display` (default `$DISPLAY`) and check XTEST is there.
    pub(super) fn connect(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen) = open(display).map_err(|e| format!("XTest: {e}"))?;
        if conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| x11_error("querying XTEST", e))?
            .is_none()
        {
            return Err(
                "XTest: the X server has no XTEST extension; X.org loads it by default \
                        (look for `Option \"XTEST\" \"off\"` in xorg.conf), Xvfb needs \
                        `+extension XTEST`"
                    .into(),
            );
        }
        // Version 2.2 is what X.org has shipped since 2003; asking also proves
        // the extension answers.
        conn.xtest_get_version(2, 2)
            .map_err(|e| x11_error("XTEST version query", e))?
            .reply()
            .map_err(|e| x11_error("XTEST version query", e))?;
        let setup_screen = conn
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| format!("XTest: the X server has no screen {screen}"))?;
        let root = setup_screen.root;
        let root_rect = Rect {
            x: 0,
            y: 0,
            w: u32::from(setup_screen.width_in_pixels),
            h: u32::from(setup_screen.height_in_pixels),
        };
        let primary = primary_monitor(&conn, root).unwrap_or(root_rect);
        let atoms = Atoms {
            net_active_window: intern(&conn, b"_NET_ACTIVE_WINDOW")?,
            net_supported: intern(&conn, b"_NET_SUPPORTED")?,
            wm_state: intern(&conn, b"WM_STATE")?,
        };
        Ok(Self {
            conn,
            root,
            primary,
            atoms,
            dead: false,
        })
    }

    fn fake(&mut self, type_: u8, detail: u8, x: i16, y: i16) {
        if self.dead {
            return;
        }
        let root = if type_ == MOTION_NOTIFY_EVENT {
            self.root
        } else {
            NONE
        };
        // detail 0 on a motion = absolute; time CurrentTime = "now".
        let sent = self
            .conn
            .xtest_fake_input(type_, detail, CURRENT_TIME, root, x, y, 0)
            .map(drop);
        if let Err(e) = sent {
            self.lost(&e);
        }
    }

    fn lost(&mut self, e: &ConnectionError) {
        tracing::warn!("XTest: lost the X server connection: {e}; remote input stops");
        self.dead = true;
    }

    fn click(&mut self, button: u8, times: i32) {
        for _ in 0..times.unsigned_abs() {
            self.fake(BUTTON_PRESS_EVENT, button, 0, 0);
            self.fake(BUTTON_RELEASE_EVENT, button, 0, 0);
        }
    }

    /// The window that should receive focus for the top-level `top`: the
    /// descendant carrying `WM_STATE` (the application's own window under a
    /// reparenting window manager's frame, ICCCM §4.1.3.1), else `top`.
    fn client_window(&self, top: Window) -> Result<Window, String> {
        let mut queue = std::collections::VecDeque::from([top]);
        let mut visited = 0;
        while let Some(w) = queue.pop_front() {
            visited += 1;
            if visited > CLIENT_SEARCH_LIMIT {
                break;
            }
            let state = self
                .conn
                .get_property(false, w, self.atoms.wm_state, AtomEnum::ANY, 0, 0)
                .map_err(|e| x11_error("reading WM_STATE", e))?
                .reply();
            match state {
                Ok(r) if r.type_ != NONE => return Ok(w),
                Ok(_) => {}
                // The window vanished between QueryTree and here: skip it.
                Err(ReplyError::X11Error(_)) => continue,
                Err(e) => return Err(x11_error("reading WM_STATE", e)),
            }
            if let Ok(tree) = self.conn.query_tree(w).map_err(|e| e.to_string())?.reply() {
                queue.extend(tree.children);
            }
        }
        Ok(top)
    }

    /// Whether a running window manager implements `_NET_ACTIVE_WINDOW`.
    fn wm_handles_activation(&self) -> Result<bool, String> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_supported,
                AtomEnum::ATOM,
                0,
                u32::MAX / 4,
            )
            .map_err(|e| x11_error("reading _NET_SUPPORTED", e))?
            .reply()
            .map_err(|e| x11_error("reading _NET_SUPPORTED", e))?;
        Ok(reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|a| a == self.atoms.net_active_window)))
    }

    fn focus(&mut self, at: Point) -> Result<bool, String> {
        let (x, y) = self.root_pixel(at);
        let top = self
            .conn
            .translate_coordinates(self.root, self.root, x, y)
            .map_err(|e| x11_error("finding the window under the point", e))?
            .reply()
            .map_err(|e| x11_error("finding the window under the point", e))?
            .child;
        if top == NONE {
            return Ok(false);
        }
        let client = self.client_window(top)?;
        if self.wm_handles_activation()? {
            // The window manager owns focus and stacking under EWMH; asking it
            // is the only way that survives its focus policy.
            let event = ClientMessageEvent::new(
                32,
                client,
                self.atoms.net_active_window,
                [SOURCE_PAGER, CURRENT_TIME, 0, 0, 0],
            );
            self.conn
                .send_event(
                    false,
                    self.root,
                    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                    event,
                )
                .map_err(|e| x11_error("sending _NET_ACTIVE_WINDOW", e))?;
        } else {
            // No EWMH window manager (bare X server, kiosk): nobody else will
            // move focus, so set it and raise the window ourselves.
            self.conn
                .set_input_focus(InputFocus::PARENT, client, CURRENT_TIME)
                .map_err(|e| x11_error("setting the input focus", e))?;
            self.conn
                .configure_window(top, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
                .map_err(|e| x11_error("raising the window", e))?;
        }
        // A round trip, so the request has been processed (or rejected) by the
        // time the caller types into the window.
        self.conn
            .get_input_focus()
            .map_err(|e| x11_error("syncing with the X server", e))?
            .reply()
            .map_err(|e| x11_error("syncing with the X server", e))?;
        Ok(true)
    }

    fn root_pixel(&self, at: Point) -> (i16, i16) {
        let target = mapping::on_primary(at, Some(self.primary)).target;
        let (x, y) = mapping::pixel_in(target.unwrap_or(self.primary), at.nx, at.ny);
        // The core protocol's coordinates are 16-bit; nothing beyond exists.
        (clamp_i16(x), clamp_i16(y))
    }

    /// Discard what the server sent us: XTest requests have no replies, only
    /// errors (a keycode outside the server's range), which are worth a log
    /// line but nothing else. Reading also keeps them from piling up.
    fn drain_events(&mut self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::Error(e))) => tracing::debug!("XTest: X error {e:?}"),
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => {
                    self.lost(&e);
                    return;
                }
            }
        }
    }
}

fn clamp_i16(v: i32) -> i16 {
    i16::try_from(v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or_default()
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom, String> {
    let what = || format!("interning {}", String::from_utf8_lossy(name));
    Ok(conn
        .intern_atom(false, name)
        .map_err(|e| x11_error(&what(), e))?
        .reply()
        .map_err(|e| x11_error(&what(), e))?
        .atom)
}

/// The RandR 1.5 primary monitor (else the first one), in root coordinates.
/// `None` when RandR is missing or reports nothing (Xvfb without RandR).
pub(super) fn primary_monitor(conn: &RustConnection, root: Window) -> Option<Rect> {
    conn.extension_information(randr::X11_EXTENSION_NAME)
        .ok()
        .flatten()?;
    let monitors = conn
        .randr_get_monitors(root, true)
        .ok()?
        .reply()
        .ok()?
        .monitors;
    choose_primary(monitors.iter().map(|m| {
        let rect = Rect {
            x: i32::from(m.x),
            y: i32::from(m.y),
            w: u32::from(m.width),
            h: u32::from(m.height),
        };
        (rect, m.primary)
    }))
}

/// The monitor flagged primary wherever it sits (it need not be at the root
/// origin, and L2 reports it as the primary display), else the first one.
/// Empty monitors are skipped.
fn choose_primary(monitors: impl Iterator<Item = (Rect, bool)>) -> Option<Rect> {
    let monitors: Vec<(Rect, bool)> = monitors.filter(|(r, _)| r.w > 0 && r.h > 0).collect();
    monitors
        .iter()
        .find(|(_, primary)| *primary)
        .or_else(|| monitors.first())
        .map(|(r, _)| *r)
}

/// The root window's size and the primary monitor of `display` (default
/// `$DISPLAY`), for the uinput backend under X11. `None` without a server.
pub(super) fn desktop_geometry(display: Option<&str>) -> Option<(Rect, Rect)> {
    let (conn, screen) = open(display).ok()?;
    let s = conn.setup().roots.get(screen)?;
    let root = Rect {
        x: 0,
        y: 0,
        w: u32::from(s.width_in_pixels),
        h: u32::from(s.height_in_pixels),
    };
    let primary = primary_monitor(&conn, s.root).unwrap_or(root);
    Some((root, primary))
}

impl Backend for XTest {
    fn pointer(&mut self, at: Point) {
        let (x, y) = self.root_pixel(at);
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y);
    }

    fn button(&mut self, button: Button, down: bool) {
        let type_ = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(type_, mapping::x11_button(button), 0, 0);
    }

    fn wheel(&mut self, steps: WheelSteps) {
        // The core protocol has no wheel: each detent is a click of 4-7.
        let vertical = if steps.notches_y > 0 {
            WHEEL_UP
        } else {
            WHEEL_DOWN
        };
        self.click(vertical, steps.notches_y);
        let horizontal = if steps.notches_x > 0 {
            WHEEL_RIGHT
        } else {
            WHEEL_LEFT
        };
        self.click(horizontal, steps.notches_x);
    }

    fn key(&mut self, evdev: u16, down: bool) {
        let Some(keycode) = mapping::x11_keycode(evdev) else {
            return;
        };
        let type_ = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(type_, keycode, 0, 0);
    }

    fn focus_window_at(&mut self, at: Point) -> bool {
        if self.dead {
            return false;
        }
        match self.focus(at) {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("{e}");
                false
            }
        }
    }

    fn flush(&mut self) {
        if self.dead {
            return;
        }
        if let Err(e) = self.conn.flush() {
            self.lost(&e);
            return;
        }
        self.drain_events();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn the_primary_monitor_is_chosen_even_off_the_origin() {
        let left = rect(0, 0, 1920, 1080);
        let primary = rect(1920, 0, 2560, 1440);
        assert_eq!(
            choose_primary([(left, false), (primary, true)].into_iter()),
            Some(primary)
        );
        assert_eq!(choose_primary([(left, false)].into_iter()), Some(left));
        assert_eq!(
            choose_primary([(rect(5, 5, 0, 0), true), (left, false)].into_iter()),
            Some(left)
        );
        assert_eq!(choose_primary(std::iter::empty()), None);
    }
}
