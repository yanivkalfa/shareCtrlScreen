//! File drops on the X11 video: the target side of XDND, on the video
//! window's own connection and event thread.
//!
//! XDND sources only talk to top-level windows: they find the client window
//! under the pointer and read its `XdndAware`. A child window never hears of
//! a drag — the drop would go to GTK (and WebKit/Tauri) instead of the video.
//! The protocol's answer is `XdndProxy`: while the video is shown and a
//! session accepts drops, the GTK toplevel's `XdndProxy` names our window, so
//! sources send every message to us (with the toplevel as the target). It is
//! removed again on hide, and GTK's own drop handling is back untouched.
//!
//! ## References
//! * XDND protocol v5 (XdndAware, XdndProxy, Enter/Position/Status/Leave/
//!   Drop/Finished, XdndSelection, XdndTypeList, XdndActionCopy):
//!   <https://www.freedesktop.org/wiki/Specifications/XDND/>
//! * ICCCM §2.4–2.7 (ConvertSelection, SelectionNotify, property transfer,
//!   INCR): <https://x.org/releases/X11R7.6/doc/xorg-docs/specs/ICCCM/icccm.html>
//! * Source-side proxy checks: GTK 3 `gdk/x11/gdkdnd-x11.c` (`xdnd_check_dest`),
//!   Qt `qxcbdrag.cpp` (`xdndProxy`).

use std::sync::Mutex;

use platform_api::{FileDrop, SurfaceError};
use surface_wgpu::SurfaceState;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode,
    SelectionNotifyEvent, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::NONE;

use super::dnd::{parse_uri_list, URI_LIST};
use super::lock;

/// The XDND version we speak (and advertise on our window).
const XDND_VERSION: u32 = 5;
/// The most a dropped file list may weigh (in 32-bit units for GetProperty):
/// 4 MiB, far beyond any real list, so a hostile source cannot make us buffer
/// an unbounded property.
const MAX_LIST_UNITS: u32 = 1 << 20;

struct Atoms {
    aware: Atom,
    proxy: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    type_list: Atom,
    action_copy: Atom,
    uri_list: Atom,
    incr: Atom,
    /// Where the converted file list is delivered on our window.
    property: Atom,
}

/// One drag over the video, from XdndEnter to XdndFinished.
#[derive(Debug, Clone, Copy)]
struct Drag {
    source: Window,
    /// The window the source thinks it talks to (the toplevel, via the proxy);
    /// echoed in every reply so stale replies can be told apart.
    target: Window,
    offers_uri_list: bool,
    accepted: bool,
    /// Last drop point, normalized over the video.
    pos: (f64, f64),
    /// XdndDrop arrived; the list is being converted.
    dropping: bool,
}

pub(crate) struct Xdnd {
    atoms: Atoms,
    window: Window,
    toplevel: Window,
    root: Window,
    drag: Mutex<Option<Drag>>,
    /// Whether the toplevel's `XdndProxy` currently names our window.
    proxied: Mutex<bool>,
}

fn x_err(e: impl std::fmt::Display) -> SurfaceError {
    SurfaceError::Backend(format!("X11 drag-and-drop setup failed: {e}"))
}

impl Xdnd {
    /// Intern the atoms and mark `window` as a valid proxy target (the spec
    /// requires the proxy to name itself, and sources read `XdndAware` there).
    pub(crate) fn new(
        conn: &RustConnection,
        window: Window,
        toplevel: Window,
        root: Window,
    ) -> Result<Self, SurfaceError> {
        let names: [&[u8]; 14] = [
            b"XdndAware",
            b"XdndProxy",
            b"XdndEnter",
            b"XdndPosition",
            b"XdndStatus",
            b"XdndLeave",
            b"XdndDrop",
            b"XdndFinished",
            b"XdndSelection",
            b"XdndTypeList",
            b"XdndActionCopy",
            URI_LIST.as_bytes(),
            b"INCR",
            b"_SHARECTRL_VIDEO_DROP",
        ];
        // Pipeline every request before reading any reply: one round trip.
        let cookies = names
            .iter()
            .map(|n| conn.intern_atom(false, n))
            .collect::<Result<Vec<_>, _>>()
            .map_err(x_err)?;
        let mut atoms = Vec::with_capacity(cookies.len());
        for cookie in cookies {
            atoms.push(cookie.reply().map_err(x_err)?.atom);
        }
        let a = |i: usize| atoms[i];
        let atoms = Atoms {
            aware: a(0),
            proxy: a(1),
            enter: a(2),
            position: a(3),
            status: a(4),
            leave: a(5),
            drop: a(6),
            finished: a(7),
            selection: a(8),
            type_list: a(9),
            action_copy: a(10),
            uri_list: a(11),
            incr: a(12),
            property: a(13),
        };
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.aware,
            AtomEnum::ATOM,
            &[XDND_VERSION],
        )
        .and_then(|_| {
            conn.change_property32(
                PropMode::REPLACE,
                window,
                atoms.proxy,
                AtomEnum::WINDOW,
                &[window],
            )
        })
        .map_err(x_err)?;
        Ok(Self {
            atoms,
            window,
            toplevel,
            root,
            drag: Mutex::new(None),
            proxied: Mutex::new(false),
        })
    }

    /// Route drops on the app window to the video (`true`: shown with a drop
    /// sink) or give them back to GTK (`false`).
    pub(crate) fn set_active(&self, conn: &RustConnection, on: bool) {
        let mut proxied = lock(&self.proxied);
        if *proxied == on {
            return;
        }
        let result = if on {
            conn.change_property32(
                PropMode::REPLACE,
                self.toplevel,
                self.atoms.proxy,
                AtomEnum::WINDOW,
                &[self.window],
            )
            .map(|_| ())
        } else {
            conn.delete_property(self.toplevel, self.atoms.proxy)
                .map(|_| ())
        };
        match result.and_then(|()| conn.flush()) {
            Ok(()) => *proxied = on,
            Err(e) => tracing::warn!("video surface: XdndProxy update failed: {e}"),
        }
    }

    /// Handle an XDND event; `false` when it was not one.
    pub(crate) fn handle(
        &self,
        conn: &RustConnection,
        event: &Event,
        state: &SurfaceState,
    ) -> bool {
        match event {
            Event::ClientMessage(m) if m.format == 32 => {
                let d = m.data.as_data32();
                let t = m.type_;
                if t == self.atoms.enter {
                    self.on_enter(conn, m.window, d);
                } else if t == self.atoms.position {
                    self.on_position(conn, d, state);
                } else if t == self.atoms.leave {
                    *lock(&self.drag) = None;
                } else if t == self.atoms.drop {
                    self.on_drop(conn, d);
                } else {
                    return false;
                }
                true
            }
            Event::SelectionNotify(e) if e.requestor == self.window => {
                self.on_converted(conn, e, state);
                true
            }
            _ => false,
        }
    }

    fn on_enter(&self, conn: &RustConnection, target: Window, d: [u32; 5]) {
        let source = d[0];
        let more_than_three = d[1] & 1 != 0;
        let offers_uri_list = if more_than_three {
            conn.get_property(false, source, self.atoms.type_list, AtomEnum::ATOM, 0, 1024)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>()))
                .is_some_and(|types| types.contains(&self.atoms.uri_list))
        } else {
            d[2..5].contains(&self.atoms.uri_list)
        };
        *lock(&self.drag) = Some(Drag {
            source,
            target,
            offers_uri_list,
            accepted: false,
            pos: (0.5, 0.5),
            dropping: false,
        });
    }

    fn on_position(&self, conn: &RustConnection, d: [u32; 5], state: &SurfaceState) {
        let mut guard = lock(&self.drag);
        let Some(drag) = guard.as_mut().filter(|g| g.source == d[0]) else {
            return;
        };
        // data.l[2] = root x << 16 | root y.
        let (rx, ry) = ((d[2] >> 16) as i16, (d[2] & 0xffff) as i16);
        if let Some(r) = conn
            .translate_coordinates(self.root, self.window, rx, ry)
            .ok()
            .and_then(|c| c.reply().ok())
        {
            // Where the user lets go is where the host pastes (the same as
            // the Windows surface).
            drag.pos = state.normalize(f64::from(r.dst_x), f64::from(r.dst_y));
        }
        drag.accepted = drag.offers_uri_list && state.accepts_drops();
        // Bit 0: accept; bit 1: keep sending positions (no "quiet" rectangle).
        let flags = if drag.accepted { 0b11 } else { 0b10 };
        let action = if drag.accepted {
            self.atoms.action_copy
        } else {
            NONE
        };
        let (source, target) = (drag.source, drag.target);
        drop(guard);
        self.send(
            conn,
            source,
            self.atoms.status,
            [target, flags, 0, 0, action],
        );
    }

    fn on_drop(&self, conn: &RustConnection, d: [u32; 5]) {
        let mut guard = lock(&self.drag);
        let Some(drag) = guard.as_mut().filter(|g| g.source == d[0]) else {
            return;
        };
        if !drag.accepted {
            let (source, target) = (drag.source, drag.target);
            *guard = None;
            drop(guard);
            self.finish(conn, source, target, false);
            return;
        }
        drag.dropping = true;
        // data.l[2] = the source's timestamp, required for the conversion.
        let converted = conn.convert_selection(
            self.window,
            self.atoms.selection,
            self.atoms.uri_list,
            self.atoms.property,
            d[2],
        );
        if converted.and_then(|_| conn.flush()).is_err() {
            let (source, target) = (drag.source, drag.target);
            *guard = None;
            drop(guard);
            self.finish(conn, source, target, false);
        }
    }

    fn on_converted(&self, conn: &RustConnection, e: &SelectionNotifyEvent, state: &SurfaceState) {
        let Some(drag) = lock(&self.drag).take().filter(|d| d.dropping) else {
            return;
        };
        let paths = if e.property == NONE {
            Vec::new()
        } else {
            conn.get_property(
                true,
                self.window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                MAX_LIST_UNITS,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            // INCR (a list too large for one property) is refused: no file
            // list a user drags comes near that size.
            .filter(|r| r.type_ != self.atoms.incr)
            .map(|r| parse_uri_list(&r.value))
            .unwrap_or_default()
        };
        let delivered = !paths.is_empty()
            && state.deliver_drop(FileDrop {
                paths,
                nx: drag.pos.0,
                ny: drag.pos.1,
            });
        self.finish(conn, drag.source, drag.target, delivered);
    }

    fn finish(&self, conn: &RustConnection, source: Window, target: Window, ok: bool) {
        let action = if ok { self.atoms.action_copy } else { NONE };
        self.send(
            conn,
            source,
            self.atoms.finished,
            [target, u32::from(ok), action, 0, 0],
        );
    }

    fn send(&self, conn: &RustConnection, to: Window, kind: Atom, data: [u32; 5]) {
        let msg = ClientMessageEvent::new(32, to, kind, data);
        // The source may have died mid-drag; nobody is left to tell.
        if let Err(e) = conn
            .send_event(false, to, EventMask::NO_EVENT, msg)
            .and_then(|_| conn.flush())
        {
            tracing::debug!("video surface: XDND reply failed: {e}");
        }
    }
}
