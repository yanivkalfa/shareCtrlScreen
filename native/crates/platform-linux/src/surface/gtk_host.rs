//! The GTK half of the video surface, shared by X11 and Wayland: find the GTK
//! toplevel behind the raw window handle, follow the content widget's layout,
//! take keys away from WebKit while the remote has them, release held input
//! on focus loss, and hop to the GTK main thread when a caller is elsewhere.
//!
//! `Platform::create_surface` only receives raw handles (no GTK object), and
//! needs none: each GTK toplevel is asked for its native window through public
//! GDK API and matched against the handle.
//!
//! ## Threading
//! GTK objects are main-thread only. Everything that holds one lives in a
//! [`ThreadGuard`] and is touched only inside closures run on the main
//! context. Callers off the main thread (the viewer media thread, the
//! transport thread) queue their work and wait **with a bound** ([`MAIN_WAIT`]):
//! `Engine::end_session` runs on the main thread and joins the session
//! threads, so an unbounded wait from one of them could deadlock the app —
//! the same rule as the macOS surface's main-queue hop.
//!
//! ## References
//! * `gtk_window_list_toplevels`, `gtk_widget_translate_coordinates`,
//!   `GtkWidget::size-allocate`, `::map`, `::unmap`, `::key-press-event`
//!   (emitted on the toplevel before GtkWindow's class handler hands the key
//!   to the focus widget), `::focus-out-event`: <https://docs.gtk.org/gtk3/>
//! * `g_main_context_invoke` (runs inline when the caller owns the context):
//!   <https://docs.gtk.org/glib/method.MainContext.invoke.html>
//! * glib-rs `ThreadGuard`: <https://docs.rs/glib/0.18/glib/thread_guard/struct.ThreadGuard.html>
//! * tao's toplevel: a header bar titlebar plus a `default_vbox` child holding
//!   the wry webview (tao 0.35 `platform_impl/linux/window.rs`).

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use gtk::gdk;
use gtk::glib;
use gtk::glib::thread_guard::ThreadGuard;
use gtk::glib::SignalHandlerId;
use gtk::prelude::*;
use platform_api::SurfaceError;
use surface_wgpu::SurfaceState;

use super::keys;

/// How long an off-main caller waits for the GTK main thread. Long enough for
/// a busy-but-alive UI (a layout pass), short enough that a main thread
/// blocked on joining the caller costs a barely noticeable pause.
const MAIN_WAIT: Duration = Duration::from_millis(500);

/// Run `f` on the GTK main thread: inline when the caller owns the main
/// context (or nobody does), otherwise queued and waited for up to
/// [`MAIN_WAIT`]. If the main thread does not answer in time the work still
/// runs, just later.
pub(crate) fn on_main(what: &'static str, f: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = mpsc::channel();
    glib::MainContext::default().invoke(move || {
        // `invoke` also runs inline on a non-main thread when nobody owns the
        // main context (the GTK loop is not running: shutdown). GTK objects
        // must not be touched there; the work is moot then anyway.
        if gtk::is_initialized_main_thread() {
            f();
        } else {
            tracing::warn!("video surface: GTK main loop not running, {what} skipped");
        }
        // The waiter may have given up already; nothing to do then.
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(MAIN_WAIT).is_err() {
        tracing::warn!("video surface: GTK main thread busy, {what} will apply late");
    }
}

/// What the backend does with the GTK events this module receives. Called on
/// the GTK main thread.
pub(crate) trait HostEvents: Send + Sync + 'static {
    fn state(&self) -> &SurfaceState;
    /// Whether keys go to the remote right now: the surface is shown and a
    /// session with control installed an input sink. Otherwise the page keeps
    /// them (local shortcuts, the toolbar's fields).
    fn wants_keys(&self) -> bool;
    /// The content widget's layout changed: `origin` in the toplevel's
    /// window coordinates and `size` in GTK logical px, `scale` = GDK's
    /// integer window scale.
    fn relayout(&self, toplevel: &gtk::Window, origin: (i32, i32), size: (u32, u32), scale: i32);
    /// The toplevel was mapped (again). GDK creates a new native surface on
    /// every map on Wayland, so the backend may need to re-attach to it.
    fn mapped(&self, _toplevel: &gtk::Window) {}
    /// The toplevel was unmapped.
    fn unmapped(&self) {}
}

/// The GTK toplevel that owns a raw window handle, and the widget the video
/// covers: the toplevel's child (tao's `default_vbox`), i.e. the client area
/// below the header bar — the counterpart of the Win32 client rect.
pub(crate) struct Toplevel {
    pub(crate) window: gtk::Window,
    pub(crate) content: gtk::Widget,
}

/// Find the GTK toplevel whose GDK window satisfies `is_ours`. Call on the
/// GTK main thread.
pub(crate) fn find_toplevel(
    is_ours: impl Fn(&gdk::Window) -> bool,
) -> Result<Toplevel, SurfaceError> {
    if !gtk::is_initialized_main_thread() {
        return Err(SurfaceError::Backend(
            "the video surface must be created on the GTK main (UI) thread".into(),
        ));
    }
    for toplevel in gtk::Window::list_toplevels() {
        let Ok(window) = toplevel.downcast::<gtk::Window>() else {
            continue;
        };
        if window.window().is_some_and(|w| is_ours(&w)) {
            let content = window.child().ok_or_else(|| {
                SurfaceError::Backend("the app window has no content for the video to cover".into())
            })?;
            return Ok(Toplevel { window, content });
        }
    }
    Err(SurfaceError::Backend(
        "no GTK window owns the window handle given to create_surface; create the video \
         surface on the UI thread after the app window is shown"
            .into(),
    ))
}

/// Read the content widget's layout and hand it to the backend. `None` from
/// `translate_coordinates` = not realized yet: the next `size-allocate` brings
/// the real position, and guessing (0,0) would cover the header bar.
fn relayout(events: &dyn HostEvents, top: &Toplevel) {
    let Some(origin) = top.content.translate_coordinates(&top.window, 0, 0) else {
        return;
    };
    let size = (
        top.content.allocated_width().max(1).unsigned_abs(),
        top.content.allocated_height().max(1).unsigned_abs(),
    );
    events.relayout(&top.window, origin, size, top.window.scale_factor());
}

/// A main-thread-only value that may be owned, shared and dropped anywhere:
/// it is only reachable on the GTK main thread ([`get`](Self::get)), and a
/// drop elsewhere is forwarded to the main context.
pub(crate) struct MainBound<T: 'static>(Option<ThreadGuard<T>>);

// SAFETY: the value is only ever reached through `get`, which returns it on
// the thread that created it (the GTK main thread) and nowhere else, so
// sharing the wrapper between threads never shares the value.
unsafe impl<T: 'static> Sync for MainBound<T> {}

impl<T: 'static> MainBound<T> {
    /// Wrap `value`. Call on the GTK main thread.
    pub(crate) fn new(value: T) -> Self {
        Self(Some(ThreadGuard::new(value)))
    }

    /// The value, on the GTK main thread; `None` anywhere else.
    pub(crate) fn get(&self) -> Option<&T> {
        self.0
            .as_ref()
            .filter(|g| g.is_owner())
            .map(ThreadGuard::get_ref)
    }
}

impl<T: 'static> Drop for MainBound<T> {
    fn drop(&mut self) {
        let Some(guard) = self.0.take() else {
            return;
        };
        if guard.is_owner() {
            return;
        }
        // Asynchronous on purpose: at process exit the main loop may already
        // be gone, and a synchronous hop would hang the exit.
        glib::MainContext::default().invoke(move || {
            if !guard.is_owner() {
                // Run inline on this non-main thread because nobody owns the
                // main context (the GTK loop is gone: process exit). The value
                // cannot be touched — or even dropped — here; leak it.
                std::mem::forget(guard);
            }
        });
    }
}

/// Signal connections on the GTK objects, plus a window we own (the Wayland
/// fallback's separate window). Dropping it (on the main thread, see
/// [`MainBound`]) disconnects every handler and destroys the owned window,
/// so no closure keeps the backend alive or intercepts keys afterwards.
struct Connected {
    handlers: Vec<(glib::Object, SignalHandlerId)>,
    owned: Option<gtk::Window>,
}

impl Drop for Connected {
    fn drop(&mut self) {
        for (object, id) in self.handlers.drain(..) {
            object.disconnect(id);
        }
        if let Some(window) = self.owned.take() {
            // SAFETY: the window was created by this surface and nothing else
            // holds a reference that expects it to stay alive; GTK drops its
            // own references as part of destroying it.
            unsafe { window.destroy() };
        }
    }
}

/// Keeps the GTK side wired to a backend (see [`Connected`]).
pub(crate) struct HostLink {
    connected: Arc<MainBound<Connected>>,
}

impl HostLink {
    /// Show or hide the window this link owns (the Wayland fallback window),
    /// from any thread, waiting for the main thread with a bound.
    pub(crate) fn set_owned_visible(&self, visible: bool) {
        let connected = self.connected.clone();
        on_main(if visible { "show" } else { "hide" }, move || {
            let Some(window) = connected.get().and_then(|c| c.owned.as_ref()) else {
                return;
            };
            if visible {
                window.show_all();
                window.present();
            } else {
                window.hide();
            }
        });
    }
}

/// Wire `top`'s signals to `events` and apply the current layout. `owned`:
/// the link owns `top.window` (destroys it with the link). Call on the GTK
/// main thread.
pub(crate) fn connect(top: &Toplevel, events: Arc<dyn HostEvents>, owned: bool) -> HostLink {
    let mut handlers: Vec<(glib::Object, SignalHandlerId)> = Vec::new();
    let window_obj: glib::Object = top.window.clone().upcast();
    let content_obj: glib::Object = top.content.clone().upcast();

    let alloc = {
        let events = events.clone();
        let window = top.window.clone();
        top.content.connect_size_allocate(move |content, _| {
            relayout(
                &*events,
                &Toplevel {
                    window: window.clone(),
                    content: content.clone(),
                },
            );
        })
    };
    handlers.push((content_obj, alloc));
    let scale = {
        let events = events.clone();
        let content = top.content.clone();
        top.window.connect_scale_factor_notify(move |window| {
            relayout(
                &*events,
                &Toplevel {
                    window: window.clone(),
                    content: content.clone(),
                },
            );
        })
    };
    handlers.push((window_obj.clone(), scale));
    let map = {
        let events = events.clone();
        top.window.connect_map(move |window| events.mapped(window))
    };
    handlers.push((window_obj.clone(), map));
    let unmap = {
        let events = events.clone();
        top.window.connect_unmap(move |_| events.unmapped())
    };
    handlers.push((window_obj.clone(), unmap));

    // Keys reach the toplevel's key-press-event before GtkWindow's class
    // handler hands them to the focus widget (WebKit). Stopping here keeps the
    // page from ever seeing them while the remote has the keyboard.
    let on_key = |events: Arc<dyn HostEvents>, down: bool| {
        move |_: &gtk::Window, ev: &gdk::EventKey| {
            if !events.wants_keys() {
                return glib::Propagation::Proceed;
            }
            // Unmapped keys are consumed too: they mean nothing locally while
            // the remote has the keyboard.
            if let Some(code) = keys::dom_code(ev.hardware_keycode()) {
                events.state().input().key(code, down);
            }
            glib::Propagation::Stop
        }
    };
    let press = top
        .window
        .connect_key_press_event(on_key(events.clone(), true));
    handlers.push((window_obj.clone(), press));
    let release = top
        .window
        .connect_key_release_event(on_key(events.clone(), false));
    handlers.push((window_obj.clone(), release));
    // Focus leaving the window mid-combo (Alt+Tab, a click elsewhere): the
    // releases land in another window, so synthesize them.
    let focus = {
        let events = events.clone();
        top.window.connect_focus_out_event(move |_, _| {
            events.state().input().release_all();
            glib::Propagation::Proceed
        })
    };
    handlers.push((window_obj, focus));

    relayout(&*events, top);
    HostLink {
        connected: Arc::new(MainBound::new(Connected {
            handlers,
            owned: owned.then(|| top.window.clone()),
        })),
    }
}
