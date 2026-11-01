//! Wayland shortcut capture: a `zwp_keyboard_shortcuts_inhibitor_v1` on the
//! app's toplevel `wl_surface`, over GDK's own `wl_display` connection.
//!
//! * **The toplevel, not the video.** The inhibitor only acts while its surface
//!   has keyboard focus, and "a sub-surface never has the keyboard focus of any
//!   seat" (wayland.xml), so it names the parent from [`SurfaceHandles`].
//! * **GDK's connection.** Object ids are per connection: the toplevel exists
//!   only on GDK's. We join it as a guest (`Backend::from_foreign_display`) with
//!   our own event queue, dispatched on our own thread, exactly as the video
//!   surface's pointer input does (L5 spike). The guest never closes GDK's
//!   connection.
//! * **A protocol error here kills the app.** It is fatal to the whole
//!   connection, GDK's included. The one error this protocol defines,
//!   `already_inhibited`, is raised for a second inhibitor on the same surface
//!   and seat, so a process-wide claim guarantees we never create two. (GTK 3
//!   creates one of its own only for a keyboard `gdk_seat_grab` on a toplevel,
//!   which neither Tauri nor WebKitGTK does on the app window.)
//! * **Keys are not ours to forward.** With the inhibitor active, the
//!   compositor sends its shortcuts to the focused toplevel as ordinary keys;
//!   GTK hands them to the video surface's key handler like any other key.
//! * **Focus tracking.** A `wl_keyboard` of our own on the seat tells us when
//!   the toplevel has keyboard focus. `install` reports `false` when the
//!   compositor says `inactive` before ever saying `active` while we are
//!   focused: that is a refusal (a user "deny" in GNOME's prompt, or a policy).
//!   A refusal that comes later is logged; an `inactive` after `active` means
//!   the user pressed the compositor's escape combo and is logged too.
//!
//! ## References
//!
//! * keyboard-shortcuts-inhibit-unstable-v1 (`inhibit_shortcuts`,
//!   `already_inhibited`, `active` / `inactive`, focus rules):
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/keyboard-shortcuts-inhibit/keyboard-shortcuts-inhibit-unstable-v1.xml>
//! * wayland.xml (`wl_subsurface` focus rule, `wl_seat.capabilities`,
//!   `wl_keyboard.enter` / `leave` / `keymap`, `wl_display.sync`):
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * libwayland client threading (`prepare_read`, one queue per thread):
//!   <https://wayland.freedesktop.org/docs/html/apb.html>
//! * wayland-backend guest mode (`Backend::from_foreign_display`,
//!   `ObjectId::from_ptr`): <https://docs.rs/wayland-backend/0.3/wayland_backend/sys/client/index.html>
//! * Compositor support: mutter (GNOME, with a permission prompt), KWin,
//!   wlroots / sway (`keyboard_shortcuts_inhibitor`); plans/02-LINUX-FINDINGS.md §4.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1,
};

/// How often `run` looks at the stop flag.
const STOP_POLL: Duration = Duration::from_millis(100);
/// `wl_seat` version we bind: `release` needs 5, nothing newer is used.
const SEAT_VERSION: u32 = 5;

/// Set while an inhibitor of ours exists (see the module docs: a second one on
/// the same surface and seat is a fatal protocol error).
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// Holds [`CLAIMED`]; dropping it lets the next session inhibit again.
struct Claim;

impl Claim {
    fn take() -> Option<Self> {
        CLAIMED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Claim)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED.store(false, Ordering::SeqCst);
    }
}

/// What the compositor said about our inhibitor so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Inhibition {
    /// Neither `active` nor `inactive` yet.
    Pending,
    Active,
    /// `ever_active` = it was active before (the user's escape combo);
    /// otherwise the compositor never granted it.
    Inactive {
        ever_active: bool,
    },
}

/// How `install` reads the state after its round trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Capturing,
    /// Not focused, or the compositor has not answered yet (GNOME asks the
    /// user first); the inhibitor stays and takes effect when granted.
    Waiting,
    Refused,
}

impl Inhibition {
    pub(super) fn on_active(&mut self) {
        *self = Inhibition::Active;
    }

    pub(super) fn on_inactive(&mut self) {
        *self = Inhibition::Inactive {
            ever_active: matches!(
                self,
                Inhibition::Active | Inhibition::Inactive { ever_active: true }
            ),
        };
    }

    pub(super) fn verdict(self, focused: bool) -> Verdict {
        match self {
            Inhibition::Active => Verdict::Capturing,
            Inhibition::Inactive { ever_active: false } if focused => Verdict::Refused,
            _ => Verdict::Waiting,
        }
    }
}

/// Everything the queue's dispatch touches.
struct State {
    toplevel: ObjectId,
    keyboard: Option<WlKeyboard>,
    focused: bool,
    inhibition: Inhibition,
    /// Whether `install` has returned; afterwards transitions are logged.
    installed: bool,
}

/// The installed Wayland capture.
pub(super) struct Inhibitor {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    inhibitor: ZwpKeyboardShortcutsInhibitorV1,
    seat: WlSeat,
    dispatcher: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    _claim: Claim,
}

impl Inhibitor {
    /// Inhibit the compositor's shortcuts for `toplevel`.
    ///
    /// # Safety
    /// `display` must be a live `wl_display` and `toplevel` a live `wl_surface`
    /// on it, both valid until [`shutdown`](Self::shutdown) returns.
    pub(super) unsafe fn install(
        display: NonNull<c_void>,
        toplevel: NonNull<c_void>,
    ) -> Result<Self, String> {
        let claim = Claim::take().ok_or("shortcut capture is already on for this window")?;
        // SAFETY: caller contract. Guest mode never closes GDK's connection.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        // Our own queue: GDK dispatches its default queue on the GTK thread, we
        // dispatch this one on ours (libwayland's prepare_read protocol makes
        // two readers of one fd safe, and GDK's event source uses it).
        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .map_err(|e| format!("cannot read the Wayland globals: {e}"))?;
        let qh = queue.handle();

        let manager: ZwpKeyboardShortcutsInhibitManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|_| {
                "this desktop does not let apps take its keyboard shortcuts \
                 (no zwp_keyboard_shortcuts_inhibit_manager_v1); GNOME, KDE Plasma and \
                 Sway support it"
                    .to_string()
            })?;
        // The first seat, as GDK's default seat: multi-seat desktops are not a
        // remote-desktop viewer's case.
        let seat: WlSeat = globals
            .bind(&qh, 1..=SEAT_VERSION, ())
            .map_err(|_| "this Wayland session has no seat (no keyboard)".to_string())?;

        // SAFETY: caller contract; from_ptr checks the proxy's interface.
        let toplevel =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), toplevel.as_ptr().cast()) }
                .map_err(|e| format!("the app window is not a Wayland surface: {e}"))?;
        let surface = WlSurface::from_id(&conn, toplevel.clone())
            .map_err(|e| format!("the app window is not a Wayland surface: {e}"))?;

        let inhibitor = manager.inhibit_shortcuts(&surface, &seat, &qh, ());
        let mut state = State {
            toplevel,
            keyboard: None,
            focused: false,
            inhibition: Inhibition::Pending,
            installed: false,
        };
        // First round trip: the seat's capabilities (we then ask for its
        // keyboard). Second: the keyboard's enter, and the inhibitor's answer
        // when the compositor gives one immediately.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|e| format!("Wayland connection error: {e}"))?;
        }

        let shutdown = Arc::new(AtomicBool::new(false));
        let mut this = Self {
            conn,
            qh,
            manager,
            inhibitor,
            seat,
            dispatcher: None,
            shutdown,
            _claim: claim,
        };
        // From here on errors drop `this`, which destroys the inhibitor.
        if state.inhibition.verdict(state.focused) == Verdict::Refused {
            return Err(
                "the desktop refused to let this window take its keyboard shortcuts \
                        (on GNOME: Settings → Apps → ShareCtrlScreen → \"Inhibit system keyboard \
                        shortcuts\")"
                    .into(),
            );
        }
        state.installed = true;
        let dispatcher = {
            let shutdown = this.shutdown.clone();
            std::thread::Builder::new()
                .name("shortcuts-wayland".into())
                .spawn(move || dispatch(queue, state, &shutdown))
                .map_err(|e| format!("cannot start the shortcut thread: {e}"))?
        };
        this.dispatcher = Some(dispatcher);
        Ok(this)
    }

    /// The compositor does the work; wait for `stop` (or for the dispatch
    /// thread to end with the connection).
    pub(super) fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst)
            && self.dispatcher.as_ref().is_some_and(|t| !t.is_finished())
        {
            std::thread::park_timeout(STOP_POLL);
        }
    }

    /// Give the shortcuts back to the compositor and stop the dispatch thread.
    /// Idempotent.
    pub(super) fn shutdown(&mut self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inhibitor.destroy();
        self.manager.destroy();
        if self.seat.version() >= 5 {
            self.seat.release();
        }
        // Wake the dispatch thread out of its blocking read: the sync's `done`
        // arrives on our queue.
        self.conn.display().sync(&self.qh, ());
        let _ = self.conn.flush();
        if let Some(t) = self.dispatcher.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn dispatch(mut queue: EventQueue<State>, mut state: State, shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            tracing::warn!("shortcut capture: Wayland dispatch stopped: {e}");
            break;
        }
    }
    if let Some(keyboard) = state.keyboard.take() {
        if keyboard.version() >= 3 {
            keyboard.release();
        }
    }
    let _ = queue.flush();
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals come and go (outputs, hot-plugged seats); the objects we need
        // were bound at install and a removed seat simply stops sending.
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let has_keyboard =
            matches!(capabilities, WEnum::Value(c) if c.contains(wl_seat::Capability::Keyboard));
        match (has_keyboard, state.keyboard.take()) {
            (true, None) => state.keyboard = Some(seat.get_keyboard(qh, ())),
            (true, Some(k)) => state.keyboard = Some(k),
            (false, Some(k)) => {
                if k.version() >= 3 {
                    k.release();
                }
                state.focused = false;
            }
            (false, None) => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only focus matters; key events go to GTK's own keyboard as well, and
        // the keymap fd is closed when the event is dropped.
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                state.focused = surface.id() == state.toplevel;
                if state.installed && state.inhibition.verdict(state.focused) == Verdict::Refused {
                    tracing::warn!(
                        "shortcut capture: the desktop does not let this window take its \
                         keyboard shortcuts; Super and Alt+Tab stay with this computer"
                    );
                }
            }
            wl_keyboard::Event::Leave { surface, .. } if surface.id() == state.toplevel => {
                state.focused = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitManagerV1,
        _: <ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                state.inhibition.on_active();
                if state.installed {
                    tracing::info!("shortcut capture: desktop shortcuts now go to the remote");
                }
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                state.inhibition.on_inactive();
                if !state.installed {
                    return;
                }
                match state.inhibition.verdict(state.focused) {
                    Verdict::Refused => tracing::warn!(
                        "shortcut capture: the desktop refused to let this window take its \
                         keyboard shortcuts; Super and Alt+Tab stay with this computer"
                    ),
                    _ => tracing::info!(
                        "shortcut capture: the desktop took its shortcuts back (its escape \
                         key combination was pressed); focus the session window again or use \
                         the desktop's own way to re-allow it"
                    ),
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The wake-up sync from `shutdown`: the dispatch loop re-checks its flag.
    }
}

#[cfg(test)]
mod tests {
    use wayland_client::globals::GlobalList;
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_client::{delegate_noop, Connection};

    use super::*;

    /// The test's own client: just a surface to inhibit on.
    struct Client;

    impl Dispatch<WlRegistry, GlobalListContents> for Client {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(Client: ignore WlCompositor);
    delegate_noop!(Client: ignore WlSurface);

    fn advertises_inhibit(globals: &GlobalList) -> bool {
        globals.contents().with_list(|list| {
            list.iter()
                .any(|g| g.interface == ZwpKeyboardShortcutsInhibitManagerV1::interface().name)
        })
    }

    /// Install on a real compositor, over a connection the test owns (standing
    /// in for GDK's): `install` succeeds exactly when the compositor offers
    /// the inhibit manager, and the unfocused surface leaves it waiting rather
    /// than refused. Run the ignored tests inside sway, GNOME or KDE (WSLg's
    /// Weston does not offer the protocol, so there `install` must refuse).
    #[test]
    #[ignore = "needs a Wayland compositor (WAYLAND_DISPLAY)"]
    fn install_matches_what_the_compositor_offers() {
        let conn = match Connection::connect_to_env() {
            Ok(c) => c,
            Err(e) => panic!("no Wayland compositor: {e}"),
        };
        let (globals, mut queue) = match registry_queue_init::<Client>(&conn) {
            Ok(g) => g,
            Err(e) => panic!("registry: {e}"),
        };
        let qh = queue.handle();
        let compositor: WlCompositor = match globals.bind(&qh, 1..=4, ()) {
            Ok(c) => c,
            Err(e) => panic!("wl_compositor: {e}"),
        };
        let surface = compositor.create_surface(&qh, ());
        if let Err(e) = queue.roundtrip(&mut Client) {
            panic!("roundtrip: {e}");
        }

        let display = NonNull::new(conn.backend().display_ptr().cast::<c_void>());
        let toplevel = NonNull::new(surface.id().as_ptr().cast::<c_void>());
        let (Some(display), Some(toplevel)) = (display, toplevel) else {
            panic!("the system backend exposes its pointers");
        };
        // SAFETY: both live on `conn`, which outlives the inhibitor below.
        let result = unsafe { Inhibitor::install(display, toplevel) };
        assert_eq!(
            result.is_ok(),
            advertises_inhibit(&globals),
            "install result: {:?}",
            result.as_ref().err()
        );
        if let Ok(mut inhibitor) = result {
            inhibitor.shutdown();
        }
        surface.destroy();
    }
}
