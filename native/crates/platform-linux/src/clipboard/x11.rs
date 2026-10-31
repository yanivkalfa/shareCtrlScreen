//! Owns the X11 `CLIPBOARD` selection to serve a file list.
//!
//! X11 has no clipboard storage: the owner of the selection answers every
//! paste request itself, so the selection exists exactly as long as this owner's
//! connection and serving thread do. [`X11Owner`] is that pair; dropping it
//! releases the selection (when we still hold it) and stops the thread.
//!
//! Not implemented, deliberately: the `INCR` protocol for payloads larger than
//! one request (a file list that big is refused up front instead), `MULTIPLE`
//! (rare, refused per ICCCM), and handing the selection to a clipboard manager
//! on exit (the list names files that only exist on this machine's session, so
//! outliving it would only paste dangling paths).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, NONE};

use super::offer::FileOffer;

/// Request header plus slack; a `ChangeProperty` must fit in one request.
const REQUEST_OVERHEAD: usize = 64;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    wake: Atom,
    /// Each served target with its payload.
    served: Vec<(Atom, Vec<u8>)>,
}

pub(super) struct X11Owner {
    conn: Arc<RustConnection>,
    window: Window,
    clipboard: Atom,
    wake: Atom,
    /// Server time at which we took ownership; needed to release it without
    /// clobbering a newer owner (the server ignores a stale-timestamp release).
    time: u32,
    /// Cleared by the serving thread on `SelectionClear` (someone else copied).
    owned: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl X11Owner {
    /// Take the selection and start serving `offer`. Fails (with a reason for
    /// the log) when there is no X server, the payload cannot be served, or
    /// another client wins the race for the selection.
    pub(super) fn acquire(offer: &FileOffer) -> Result<Self, String> {
        let (conn, screen) =
            RustConnection::connect(None).map_err(|e| format!("cannot connect to X11: {e}"))?;
        let conn = Arc::new(conn);
        let io = |e: x11rb::errors::ReplyOrIdError| format!("X11 request failed: {e}");
        let max_payload = conn
            .maximum_request_bytes()
            .saturating_sub(REQUEST_OVERHEAD);
        if offer.targets().iter().any(|(_, d)| d.len() > max_payload) {
            return Err("file list too large to serve without INCR".into());
        }

        let atoms = intern_atoms(&conn, offer).map_err(io)?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().map_err(io)?;
        // An unmapped 1x1 InputOnly window: we need an identity to own the
        // selection and to receive requests, never anything on screen.
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| format!("X11 request failed: {e}"))?;

        let time = server_time(&conn, window, atoms.wake)?;
        conn.set_selection_owner(window, atoms.clipboard, time)
            .map_err(|e| format!("X11 request failed: {e}"))?;
        // SetSelectionOwner has no reply and fails silently when another client
        // set the selection with a later timestamp; ask who owns it now.
        let owner = conn
            .get_selection_owner(atoms.clipboard)
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply()
            .map_err(|e| format!("X11 request failed: {e}"))?
            .owner;
        if owner != window {
            return Err("lost the race for the CLIPBOARD selection".into());
        }

        let owned = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (clipboard, wake) = (atoms.clipboard, atoms.wake);
        let thread = {
            let (conn, owned, shutdown) = (conn.clone(), owned.clone(), shutdown.clone());
            std::thread::Builder::new()
                .name("clipboard-x11".into())
                .spawn(move || serve(&conn, &atoms, time, &owned, &shutdown))
                .map_err(|e| format!("cannot start the clipboard thread: {e}"))?
        };
        Ok(Self {
            conn,
            window,
            clipboard,
            wake,
            time,
            owned,
            shutdown,
            thread: Some(thread),
        })
    }

    /// Whether our file list is still what a paste would get.
    pub(super) fn is_owned(&self) -> bool {
        self.owned.load(Ordering::SeqCst)
    }
}

impl Drop for X11Owner {
    fn drop(&mut self) {
        // Release only what is still ours. A stale timestamp makes the server
        // ignore the request if someone took the selection since, so even a lost
        // race here cannot wipe another application's copy.
        if self.owned.load(Ordering::SeqCst) {
            let _ = self
                .conn
                .set_selection_owner(NONE, self.clipboard, self.time);
        }
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the thread out of its blocking wait with a message to our own
        // window; the shutdown flag is checked on every event.
        let wake = ClientMessageEvent::new(32, self.window, self.wake, [0u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.window, EventMask::NO_EVENT, wake);
        let _ = self.conn.flush();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn intern_atoms(
    conn: &RustConnection,
    offer: &FileOffer,
) -> Result<Atoms, x11rb::errors::ReplyOrIdError> {
    let intern = |name: &str| conn.intern_atom(false, name.as_bytes());
    // Pipeline every request before reading any reply: one round trip in total.
    let clipboard = intern("CLIPBOARD")?;
    let targets = intern("TARGETS")?;
    let timestamp = intern("TIMESTAMP")?;
    let wake = intern("_SHARECTRL_CLIPBOARD_WAKE")?;
    let served = offer
        .targets()
        .into_iter()
        .map(|(name, data)| Ok((intern(name)?, data.to_vec())))
        .collect::<Result<Vec<_>, x11rb::errors::ConnectionError>>()?;
    Ok(Atoms {
        clipboard: clipboard.reply()?.atom,
        targets: targets.reply()?.atom,
        timestamp: timestamp.reply()?.atom,
        wake: wake.reply()?.atom,
        served: served
            .into_iter()
            .map(|(cookie, data)| Ok((cookie.reply()?.atom, data)))
            .collect::<Result<_, x11rb::errors::ReplyError>>()?,
    })
}

/// The X server's current time. ICCCM requires a real timestamp to acquire a
/// selection (`CurrentTime` makes ownership transfers racy); the only way to
/// learn it is to provoke an event that carries one.
fn server_time(conn: &RustConnection, window: Window, scratch: Atom) -> Result<u32, String> {
    conn.change_property8(PropMode::APPEND, window, scratch, AtomEnum::STRING, &[])
        .map_err(|e| format!("X11 request failed: {e}"))?;
    conn.flush().map_err(|e| format!("X11 flush failed: {e}"))?;
    loop {
        match conn.wait_for_event() {
            Ok(Event::PropertyNotify(e)) if e.window == window => return Ok(e.time),
            Ok(_) => {}
            Err(e) => return Err(format!("X11 connection lost: {e}")),
        }
    }
}

/// Answer paste requests until we lose the selection or are told to stop.
fn serve(
    conn: &RustConnection,
    atoms: &Atoms,
    time: u32,
    owned: &AtomicBool,
    shutdown: &AtomicBool,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match conn.wait_for_event() {
            Ok(Event::SelectionRequest(req)) => reply_to(conn, atoms, time, &req),
            Ok(Event::SelectionClear(_)) => {
                // Someone else copied: our list is no longer the clipboard and
                // nothing is left to serve.
                owned.store(false, Ordering::SeqCst);
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("clipboard X11 connection ended: {e}");
                owned.store(false, Ordering::SeqCst);
                return;
            }
        }
    }
}

fn reply_to(conn: &RustConnection, atoms: &Atoms, time: u32, req: &SelectionRequestEvent) {
    // ICCCM 2.2: an obsolete client passes property None and expects the target
    // atom to be used as the property.
    let property = if req.property == NONE {
        req.target
    } else {
        req.property
    };
    let served = req.selection == atoms.clipboard && write_target(conn, atoms, time, req, property);
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: if served { property } else { NONE },
    };
    // The requestor may have died since asking; nobody is left to tell.
    let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    let _ = conn.flush();
}

/// Store the answer for `req.target` on the requestor's window.
fn write_target(
    conn: &RustConnection,
    atoms: &Atoms,
    time: u32,
    req: &SelectionRequestEvent,
    property: Atom,
) -> bool {
    let (window, target) = (req.requestor, req.target);
    let stored = if target == atoms.targets {
        let mut list = vec![atoms.targets, atoms.timestamp];
        list.extend(atoms.served.iter().map(|(a, _)| *a));
        conn.change_property32(PropMode::REPLACE, window, property, AtomEnum::ATOM, &list)
    } else if target == atoms.timestamp {
        conn.change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::INTEGER,
            &[time],
        )
    } else if let Some((_, data)) = atoms.served.iter().find(|(a, _)| *a == target) {
        // Non-text MIME targets are answered with the target atom as the type.
        conn.change_property8(PropMode::REPLACE, window, property, target, data)
    } else {
        return false;
    };
    stored.is_ok()
}
