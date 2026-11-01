//! File drops on the Wayland video, through **GDK's own** data device.
//!
//! The video `wl_surface`'s user data is the widget-less sink `GdkWindow`
//! (`wl::create_surface_with_user_data`), so when a drag enters the video GDK's
//! `wl_data_device` names the sink: GDK creates the drop context and queues
//! `GDK_DRAG_ENTER` / `MOTION` / `LEAVE` / `DROP_START` for that window. GTK
//! would drop them (no widget), so a GDK event handler takes the events aimed
//! at the sink and runs the drop target there, forwarding everything else to
//! `gtk_main_do_event` unchanged.
//!
//! WHY not a `wl_data_device` of our own: compositors deliver a drag to ONE
//! data device per client — mutter (`meta_wayland_drag_grab_set_focus`),
//! Weston (`weston_drag_set_focus`) and KWin (`dataDevicesForSurface().first()`)
//! all pick one — so a second device would either never see video drops or
//! steal every drag from GTK/WebKit for the whole process. Only wlroots fans a
//! drag out to every device.
//!
//! Everything here runs on the GTK main thread (the event handler is called
//! there); the drop sink is a `Sender`, so delivering never blocks.
//!
//! ## References
//! * `gdk_event_handler_set`, `gtk_main_do_event`:
//!   <https://docs.gtk.org/gdk3/func.event_handler_set.html>,
//!   <https://docs.gtk.org/gtk3/func.main_do_event.html>
//! * GDK 3 Wayland DnD: `data_device_enter/motion/drop` resolve the focus
//!   surface's user data to the destination `GdkWindow` and emit the GDK DnD
//!   events with surface-local coordinates in `x_root`/`y_root`
//!   (`gdk/wayland/gdkdevice-wayland.c`, `gdk/wayland/gdkdnd-wayland.c`):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/tree/gtk-3-24/gdk/wayland>
//! * `gdk_drag_status`, `gdk_drop_finish`, `gdk_drag_get_selection`,
//!   `gdk_selection_convert`, `gdk_selection_property_get` (what GTK's own
//!   `gtk_drag_get_data` / `gtk_drag_finish` do): <https://docs.gtk.org/gdk3/>

use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering;
use std::sync::Weak;

use gtk::gdk;
use gtk::glib::translate::ToGlibPtr;
use platform_api::FileDrop;

use super::super::dnd::{parse_uri_list, URI_LIST};
use super::Shared;

/// One drag over the video, from enter to finish.
struct Drag {
    context: gdk::DragContext,
    offers_list: bool,
    accepted: bool,
    /// Last position on the video, surface-local logical px.
    pos: (f64, f64),
    /// The drop happened (its timestamp); the list is being converted.
    dropping: Option<u32>,
}

/// A sink window whose drags belong to a video surface.
struct Route {
    sink: gdk::Window,
    shared: Weak<Shared>,
    drag: Option<Drag>,
}

thread_local! {
    // Main thread only: GDK calls the event handler there.
    static ROUTES: RefCell<Vec<Route>> = const { RefCell::new(Vec::new()) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Keeps a sink's drops routed to its surface; dropping it (on the main
/// thread, see `gtk_host::MainBound`) removes the route.
pub(crate) struct RouteToken {
    sink: gdk::Window,
}

impl Drop for RouteToken {
    fn drop(&mut self) {
        ROUTES.with(|r| r.borrow_mut().retain(|route| route.sink != self.sink));
    }
}

/// Route drags on `sink` to `shared`'s drop sink. Call on the GTK main thread.
pub(crate) fn route(sink: &gdk::Window, shared: Weak<Shared>) -> RouteToken {
    ROUTES.with(|r| {
        r.borrow_mut().push(Route {
            sink: sink.clone(),
            shared,
            drag: None,
        })
    });
    if !INSTALLED.with(|i| i.replace(true)) {
        // Installed once for the process and never removed: with no route it
        // only forwards, which is exactly GTK's own handler.
        gdk::Event::set_handler(Some(|ev: &mut gdk::Event| {
            if !handle(ev) {
                gtk::main_do_event(ev);
            }
        }));
    }
    RouteToken { sink: sink.clone() }
}

/// Handle `ev` if it is a drag or selection event on a routed sink.
fn handle(ev: &gdk::Event) -> bool {
    use gdk::EventType as T;
    let ty = ev.event_type();
    if !matches!(
        ty,
        T::DragEnter | T::DragMotion | T::DragLeave | T::DropStart | T::SelectionNotify
    ) {
        return false;
    }
    let Some(window) = ev.window() else {
        return false;
    };
    ROUTES.with(|r| {
        let mut routes = r.borrow_mut();
        let Some(route) = routes.iter_mut().find(|route| route.sink == window) else {
            return false;
        };
        if ty == T::SelectionNotify {
            converted(route);
        } else if let Some(dnd) = ev.downcast_ref::<gdk::EventDND>() {
            dnd_event(route, ty, dnd);
        }
        true
    })
}

fn dnd_event(route: &mut Route, ty: gdk::EventType, dnd: &gdk::EventDND) {
    use gdk::EventType as T;
    let time = dnd.time();
    match ty {
        T::DragEnter => {
            let Some(context) = dnd.context() else {
                return;
            };
            let list = gdk::Atom::intern(URI_LIST);
            route.drag = Some(Drag {
                offers_list: context.list_targets().contains(&list),
                context,
                accepted: false,
                pos: (0.0, 0.0),
                dropping: None,
            });
        }
        T::DragMotion => {
            let accepts = route
                .shared
                .upgrade()
                .is_some_and(|s| s.shown.load(Ordering::SeqCst) && s.state.accepts_drops());
            let Some(drag) = &mut route.drag else {
                return;
            };
            // GDK's Wayland backend reports focus-surface-local coordinates
            // here (it has no root coordinates).
            let (x, y) = dnd.root();
            drag.pos = (f64::from(x), f64::from(y));
            drag.accepted = drag.offers_list && accepts;
            let action = if drag.accepted {
                gdk::DragAction::COPY
            } else {
                gdk::DragAction::empty()
            };
            drag.context.drag_status(action, time);
        }
        T::DragLeave => {
            // A leave after the drop is part of finishing it; keep that drag.
            if route.drag.as_ref().is_some_and(|d| d.dropping.is_none()) {
                route.drag = None;
            }
        }
        T::DropStart => {
            let Some(drag) = &mut route.drag else {
                return;
            };
            if !drag.accepted {
                drag.context.drop_finish(false, time);
                route.drag = None;
                return;
            }
            drag.dropping = Some(time);
            let selection = drag.context.drag_get_selection();
            gdk::selection_convert(&route.sink, &selection, &gdk::Atom::intern(URI_LIST), time);
        }
        _ => {}
    }
}

/// The converted file list arrived on the sink: read it, deliver it, finish.
fn converted(route: &mut Route) {
    let Some((drag, time)) = route.drag.take().and_then(|d| d.dropping.map(|t| (d, t))) else {
        return;
    };
    let paths = parse_uri_list(&selection_bytes(&route.sink));
    let delivered = match route.shared.upgrade() {
        Some(s) if !paths.is_empty() => {
            let (px, py) = s.to_full_px(drag.pos.0, drag.pos.1, false);
            let (nx, ny) = s.state.normalize(px, py);
            s.state.deliver_drop(FileDrop { paths, nx, ny })
        }
        _ => false,
    };
    drag.context.drop_finish(delivered, time);
}

/// The selection data GDK stored on `window` for the last conversion.
fn selection_bytes(window: &gdk::Window) -> Vec<u8> {
    let raw: *mut gdk::ffi::GdkWindow = window.to_glib_none().0;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut prop_type: gdk::ffi::GdkAtom = std::ptr::null_mut();
    let mut format: std::ffi::c_int = 0;
    // SAFETY: a live GdkWindow; GDK allocates `data` (NUL-terminated, `len`
    // bytes of payload) or leaves it NULL, and the caller frees it with
    // g_free, as `gdk_selection_property_get` documents.
    let len = unsafe {
        gdk::ffi::gdk_selection_property_get(raw, &mut data, &mut prop_type, &mut format)
    };
    if data.is_null() {
        return Vec::new();
    }
    let bytes = match usize::try_from(len) {
        // SAFETY: GDK guarantees `len` readable bytes at `data`.
        Ok(n) if n > 0 => unsafe { std::slice::from_raw_parts(data, n) }.to_vec(),
        _ => Vec::new(),
    };
    // SAFETY: allocated by GDK with g_malloc, freed exactly once.
    unsafe { gtk::glib::ffi::g_free(data.cast()) };
    bytes
}
