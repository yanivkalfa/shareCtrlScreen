//! The GTK half: find the toplevel that owns the window handle, follow the
//! content widget's allocation, and take keys away from WebKit while the video
//! is shown. Everything here runs on the GTK thread.
//!
//! `Platform::create_surface` only receives raw handles (no GTK object), and it
//! needs none: the GTK toplevel is found by asking each GTK toplevel for its
//! `wl_surface` through public GDK API and matching the handle's pointer.
//!
//! ## References
//! * `gtk_window_list_toplevels`, `gtk_widget_translate_coordinates`,
//!   `GtkWidget::size-allocate`, `key-press-event` (emitted on the toplevel
//!   before GtkWindow's own handler propagates the key to the focus widget):
//!   <https://docs.gtk.org/gtk3/>
//! * `gdk_wayland_window_get_wl_surface`, `gdk_window_new` (GDK_INPUT_ONLY):
//!   <https://docs.gtk.org/gdk3-wayland/> and <https://docs.gtk.org/gdk3/ctor.Window.new.html>
//! * `gtk_main_do_event` drops events on windows without a widget
//!   (gtk/gtkmain.c, "if (!event_widget)"):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gtk/gtkmain.c>
//! * tao's Wayland toplevel: a GtkHeaderBar titlebar plus a `default_vbox` child
//!   holding the wry webview (tao 0.35 `platform_impl/linux/window.rs`).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use gtk::gdk;
use gtk::glib;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;

use crate::error::SpikeError;
use crate::surface::Shared;

/// Key under which the sink window is stored on the toplevel.
const SINK_KEY: &str = "sharectrl-video-sink";

pub struct GtkParts {
    pub toplevel: gtk::Window,
    /// The widget the video covers: the toplevel's child (tao's `default_vbox`),
    /// i.e. the client area below the CSD header bar — the counterpart of the
    /// Win32 client rect.
    pub content: gtk::Widget,
}

/// Find the GTK toplevel whose `wl_surface` is `surface`.
pub fn find(surface: NonNull<c_void>) -> Result<GtkParts, SpikeError> {
    let display = gdk::Display::default().ok_or(SpikeError::NoGtkWindow)?;
    if !display.backend().is_wayland() {
        return Err(SpikeError::NotWayland("non-Wayland GDK"));
    }
    for toplevel in gtk::Window::list_toplevels() {
        let Ok(toplevel) = toplevel.downcast::<gtk::Window>() else {
            continue;
        };
        let Some(window) = toplevel.window() else {
            continue;
        };
        if wl_surface_of(&window) == Some(surface) {
            let content = toplevel.child().ok_or(SpikeError::NoContent)?;
            return Ok(GtkParts { toplevel, content });
        }
    }
    Err(SpikeError::NoGtkWindow)
}

fn wl_surface_of(window: &gdk::Window) -> Option<NonNull<c_void>> {
    let ptr: *mut gdk::ffi::GdkWindow = window.to_glib_none().0;
    // SAFETY: a live GdkWindow of the Wayland backend (checked by the caller);
    // returns its wl_surface or NULL before realize.
    let surface = unsafe { gdk_wayland_sys::gdk_wayland_window_get_wl_surface(ptr.cast()) };
    NonNull::new(surface)
}

/// The never-shown, widget-less GdkWindow whose pointer becomes the video
/// surface's `wl_surface` user data (why: `wl::create_surface_with_user_data`).
///
/// It is stored on the toplevel, so it lives exactly as long as the toplevel
/// object; the toplevel's `wl_surface` (our parent, which unmaps our tree when
/// destroyed) goes away at unrealize, before that.
pub fn create_sink(toplevel: &gtk::Window) -> Result<NonNull<c_void>, SpikeError> {
    let attrs = gdk::WindowAttr {
        window_type: gdk::WindowType::Toplevel,
        wclass: gdk::WindowWindowClass::InputOnly,
        width: 1,
        height: 1,
        // No events selected: GDK filters everything aimed at it before GTK.
        event_mask: gdk::EventMask::empty(),
        ..gdk::WindowAttr::default()
    };
    let sink = gdk::Window::new(None, &attrs);
    let raw: *mut gdk::ffi::GdkWindow = sink.to_glib_none().0;
    let ptr = NonNull::new(raw)
        .ok_or_else(|| SpikeError::Wayland("gdk_window_new returned NULL".into()))?;
    // SAFETY: the value type matches every later access (none — it is only kept
    // alive); glib drops it when the toplevel is finalized. The object, and so
    // `ptr`, stays valid until then.
    unsafe { toplevel.set_data(SINK_KEY, sink) };
    Ok(ptr.cast::<c_void>())
}

/// Wire the GTK signals to the shared surface state and apply the current
/// allocation.
pub fn connect(parts: &GtkParts, shared: Arc<Shared>) {
    let toplevel = parts.toplevel.clone();

    let relayout = {
        let shared = shared.clone();
        let toplevel = toplevel.clone();
        move |content: &gtk::Widget| {
            // Toplevel GdkWindow coordinates are wl_surface-local coordinates on
            // GDK's Wayland backend (the CSD shadow is inside both).
            // None = not realized yet; the next size-allocate brings the
            // real position, and guessing (0,0) would cover the header bar.
            let Some(origin) = content.translate_coordinates(&toplevel, 0, 0) else {
                return;
            };
            let size = (
                content.allocated_width().max(1) as u32,
                content.allocated_height().max(1) as u32,
            );
            if shared.update_allocation(origin, size, toplevel.scale_factor()) {
                // The area's position is GTK's (double-buffered) state; make GTK
                // commit so it applies even if nothing else needs repainting.
                toplevel.queue_draw();
            }
        }
    };
    relayout(&parts.content);
    // The new subsurface itself is also GTK state ("adding a sub-surface becomes
    // visible on the next time the state of the parent surface is applied").
    toplevel.queue_draw();

    let on_alloc = relayout.clone();
    parts
        .content
        .connect_size_allocate(move |w, _| on_alloc(w.upcast_ref()));
    let content = parts.content.clone();
    toplevel.connect_scale_factor_notify(move |_| relayout(&content));

    // Keys reach the toplevel's key-press-event before GtkWindow's class handler
    // hands them to the focus widget (WebKit). Stopping here keeps the page from
    // ever seeing them while the video is shown.
    let on_key = |shared: Arc<Shared>, down: bool| {
        move |_: &gtk::Window, ev: &gdk::EventKey| {
            if !shared.intercepts_keys() {
                return glib::Propagation::Proceed;
            }
            if let Some(evdev) = crate::keys::evdev_from_gdk(ev.hardware_keycode()) {
                shared.key(evdev, down);
            }
            glib::Propagation::Stop
        }
    };
    toplevel.connect_key_press_event(on_key(shared.clone(), true));
    toplevel.connect_key_release_event(on_key(shared.clone(), false));
    // Focus leaving the window mid-combo: the releases land elsewhere.
    toplevel.connect_focus_out_event(move |_, _| {
        shared.release_all_held();
        glib::Propagation::Proceed
    });
}
