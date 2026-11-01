//! The Wayland objects behind the video surface, created on GDK's own
//! `wl_display` connection (a subsurface must belong to the same client as
//! its parent, so a second connection is not an option).
//!
//! Surface tree (the gtkwaylandsink layout):
//!
//! ```text
//! GTK toplevel wl_surface   (GDK's; WebKitGTK paints into it)
//! └── area   (ours, desync)   1×1 transparent buffer, EMPTY input region,
//!     │                        positioned at the content widget's origin
//!     └── video (ours, desync) wgpu swapchain, viewport-scaled, full input
//! ```
//!
//! `area` exists so that every geometry change of `video` — its viewport
//! (video state) and its position (area state) — can be applied atomically by
//! a commit WE own, without waiting for GTK to commit the toplevel. Only
//! area's own position lives in GTK's state; it changes only on relayout,
//! when GTK commits anyway.
//!
//! `area`'s `wl_subsurface` is the only object tied to GTK's surface, and it
//! is re-created whenever GTK maps the toplevel again: GDK 3 destroys a
//! toplevel's `wl_surface` on every hide and makes a new one on show
//! (`gdk_wayland_window_hide_surface`). `area` and `video` themselves — and so
//! the presenter's swapchain — live as long as the video surface.
//!
//! ## References
//! * wl_subsurface (mapping rules, sync/desync, "initially added as the
//!   top-most in the stack", "never has the keyboard focus", inert when the
//!   parent is destroyed), wl_surface.set_input_region:
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * libwayland client API (`wl_proxy_marshal_array_constructor_versioned`,
//!   `wl_proxy_set_user_data`, multi-threaded `prepare_read`):
//!   <https://wayland.freedesktop.org/docs/html/apb.html>
//! * wayland-backend 0.3 `Backend::from_foreign_display`, `ObjectId::from_ptr`
//!   (guest mode on a C-owned connection; destroying an external object sends
//!   the destructor and frees only the proxy): `wayland-backend-0.3.17/src/sys/client_impl/mod.rs`
//! * GDK 3 reads `wl_surface_get_user_data()` of every surface a seat event
//!   names and treats it as a `GdkWindow` (pointer/keyboard/DnD/tablet check
//!   `GDK_IS_WINDOW` first; **touch does not**, `gdk_wayland_seat_add_touch`);
//!   hide/show destroy and re-create the toplevel's `wl_surface`:
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c>,
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkwindow-wayland.c>
//! * GStreamer GstWlWindow (area + video subsurfaces):
//!   <https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwlwindow.c>

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::ptr::NonNull;

use platform_api::SurfaceError;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalList};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::{self, WlCompositor};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_sys::client::{wayland_client_handle, wl_proxy};
use wayland_sys::common::wl_argument;
use wayland_sys::ffi_dispatch;

use super::input::Dispatcher;

/// Why the tree could not be built. `NoViewporter` selects the decision-#3
/// fallback window; everything else is a plain failure.
#[derive(Debug)]
pub(crate) enum BuildError {
    NoViewporter,
    Failed(SurfaceError),
}

impl From<SurfaceError> for BuildError {
    fn from(e: SurfaceError) -> Self {
        BuildError::Failed(e)
    }
}

impl From<BuildError> for SurfaceError {
    fn from(e: BuildError) -> Self {
        match e {
            BuildError::NoViewporter => SurfaceError::Backend(
                "the compositor does not offer wp_viewporter, which the in-window video needs"
                    .into(),
            ),
            BuildError::Failed(e) => e,
        }
    }
}

fn wl_err(what: impl std::fmt::Display) -> SurfaceError {
    SurfaceError::Backend(format!("Wayland: {what}"))
}

/// The objects every later operation needs. Created once per video surface.
pub(crate) struct WlObjects {
    pub(crate) conn: Connection,
    /// GDK's `wl_display` and our video `wl_surface`, for wgpu.
    pub(crate) display: NonNull<c_void>,
    pub(crate) video_ptr: NonNull<c_void>,
    pub(crate) subcompositor: WlSubcompositor,
    pub(crate) area: WlSurface,
    pub(crate) area_viewport: Option<WpViewport>,
    pub(crate) transparent: WlBuffer,
    pub(crate) video: WlSurface,
    pub(crate) video_sub: WlSubsurface,
    pub(crate) video_viewport: Option<WpViewport>,
    /// Reports the video's preferred scale on our queue (input thread).
    pub(crate) fractional: Option<WpFractionalScaleV1>,
}

// SAFETY: `display` and `video_ptr` are libwayland object pointers.
// libwayland-client serializes requests on a display with its own mutex, so
// sending from any thread is allowed; the display lives for the process (GDK
// never closes it while the app runs) and the video surface until `Drop` of
// the owning `Shared`, after the presenter (which holds that `Shared`).
unsafe impl Send for WlObjects {}
// SAFETY: as above — the pointers are only copied and passed to thread-safe C APIs.
unsafe impl Sync for WlObjects {}

/// Everything `create` produces: the shared objects plus what only the input
/// thread uses.
pub(crate) struct Created {
    pub(crate) objects: WlObjects,
    pub(crate) queue: EventQueue<Dispatcher>,
    pub(crate) seat: WlSeat,
}

/// Build the surface tree (not yet attached to any parent).
/// `require_viewporter`: fail with [`BuildError::NoViewporter`] when the
/// compositor has no `wp_viewporter` (the in-window layout needs it for the
/// toolbar crop and fractional scaling).
///
/// # Safety
/// `display` must be GDK's live `wl_display`, outliving the video surface;
/// `sink` must be a `GdkWindow` that outlives the video `wl_surface`'s ability
/// to receive input (see `super::create_sink`).
pub(crate) unsafe fn create(
    display: NonNull<c_void>,
    sink: NonNull<c_void>,
    require_viewporter: bool,
) -> Result<Created, BuildError> {
    // SAFETY: the caller guarantees a live GDK display. Guest mode never
    // closes GDK's connection.
    let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    // Our own queue: GDK dispatches its default queue on the GTK thread, we
    // dispatch this one on the input thread (libwayland's prepare_read
    // protocol makes two readers of one fd safe, and GDK's event source uses it).
    let (globals, queue) =
        registry_queue_init::<Dispatcher>(&conn).map_err(|e| wl_err(format!("registry: {e}")))?;
    let qh = queue.handle();

    // damage_buffer (v4) is what wgpu/Mesa and our area buffer use.
    let compositor: WlCompositor = bind(&globals, &qh, "wl_compositor", 4, 6)?;
    let subcompositor: WlSubcompositor = bind(&globals, &qh, "wl_subcompositor", 1, 1)?;
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    if require_viewporter && viewporter.is_none() {
        return Err(BuildError::NoViewporter);
    }
    let shm: WlShm = bind(&globals, &qh, "wl_shm", 1, 1)?;
    // v5 adds wl_pointer.frame/axis_discrete, v8 axis_value120. v1 still
    // works (no frames: wheel is flushed per event).
    let seat: WlSeat = bind(&globals, &qh, "wl_seat", 1, 8)?;
    // A fractional scale is only usable with a viewport (the surface size is
    // the viewport destination, not buffer / buffer_scale).
    let fractional_manager: Option<WpFractionalScaleManagerV1> = viewporter
        .as_ref()
        .and_then(|_| globals.bind(&qh, 1..=1, ()).ok());

    let area = compositor.create_surface(&qh, ());
    // Pointer, touch and drag-and-drop focus follow the input region, so an
    // empty one guarantees the area never becomes the focus of any seat
    // device — it is invisible plumbing and GDK never sees it named in an event.
    let empty = compositor.create_region(&qh, ());
    area.set_input_region(Some(&empty));
    empty.destroy();
    // The area is 1×1 logical (its 1×1 buffer at scale 1, pinned by the
    // viewport where there is one): its children are not clipped to it, and
    // a 1-pixel transparent quad costs the compositor nothing to blend.
    let area_viewport = viewporter.as_ref().map(|v| {
        let vp = v.get_viewport(&area, &qh, ());
        vp.set_destination(1, 1);
        vp
    });
    let transparent = transparent_pixel(&shm, &qh)?;

    // SAFETY: compositor is a live proxy on our queue; sink outlives the
    // surface's input (caller contract).
    let video_ptr = unsafe { create_surface_with_user_data(&compositor, sink) }?;
    // SAFETY: just created, valid until `Shared` destroys it.
    let video = unsafe { foreign_surface(&conn, video_ptr) }?;
    let video_sub = subcompositor.get_subsurface(&video, &area, &qh, ());
    // Desync: wgpu presents on its own thread at the stream's rate; the
    // frames must not wait for any other surface's commit.
    video_sub.set_desync();
    let video_viewport = viewporter.as_ref().map(|v| v.get_viewport(&video, &qh, ()));
    // The video is opaque (letterbox bars are drawn black), which lets the
    // compositor skip blending and painting what lies beneath.
    let opaque = compositor.create_region(&qh, ());
    opaque.add(0, 0, i32::MAX, i32::MAX);
    video.set_opaque_region(Some(&opaque));
    opaque.destroy();
    let fractional = fractional_manager.map(|m| {
        let scale = m.get_fractional_scale(&video, &qh, ());
        // The per-surface object outlives its manager.
        m.destroy();
        scale
    });

    conn.flush().map_err(|e| wl_err(format!("flush: {e}")))?;
    tracing::info!(
        "wayland: globals ok (compositor v{}, seat v{}, viewporter {}, fractional-scale {})",
        compositor.version(),
        seat.version(),
        if viewporter.is_some() { "yes" } else { "no" },
        if fractional.is_some() { "yes" } else { "no" }
    );
    Ok(Created {
        objects: WlObjects {
            conn,
            display,
            video_ptr,
            subcompositor,
            area,
            area_viewport,
            transparent,
            video,
            video_sub,
            video_viewport,
            fractional,
        },
        queue,
        seat,
    })
}

fn bind<I>(
    globals: &GlobalList,
    qh: &QueueHandle<Dispatcher>,
    interface: &'static str,
    min: u32,
    max: u32,
) -> Result<I, SurfaceError>
where
    I: Proxy + 'static,
    Dispatcher: wayland_client::Dispatch<I, ()>,
{
    globals.bind(qh, min..=max, ()).map_err(|_| {
        SurfaceError::Backend(format!(
            "the compositor does not offer {interface} v{min}+, which the in-window video needs"
        ))
    })
}

/// Wrap a C-owned `wl_surface` as a wayland-client proxy (requests only; its
/// events stay with whoever owns it).
///
/// # Safety
/// `ptr` must be a live `wl_surface` proxy on `conn`'s display.
pub(crate) unsafe fn foreign_surface(
    conn: &Connection,
    ptr: NonNull<c_void>,
) -> Result<WlSurface, SurfaceError> {
    // SAFETY: caller contract; from_ptr checks the interface name.
    let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr.as_ptr().cast::<wl_proxy>()) }
        .map_err(|e| wl_err(format!("foreign wl_surface: {e}")))?;
    WlSurface::from_id(conn, id).map_err(|e| wl_err(format!("foreign wl_surface: {e}")))
}

/// `wl_compositor.create_surface` through libwayland directly, so the new
/// proxy's user data is ours to set.
///
/// WHY: GDK 3 casts the user data of any surface named in a `wl_touch.down` to
/// a `GdkWindow` and refs it WITHOUT a type check. A wayland-rs managed proxy
/// stores a Rust struct there, which GDK would then corrupt. We store a real,
/// never-shown `GdkWindow` with no widget (the "sink"): GDK's checks pass, its
/// events land on a window GTK drops (`gtk_main_do_event` ignores windows
/// without a widget), and GDK keeps managing the cursor over the video.
///
/// # Safety
/// `compositor` must be live; `sink` must be a `GdkWindow` that outlives the
/// surface's ability to receive input.
unsafe fn create_surface_with_user_data(
    compositor: &WlCompositor,
    sink: NonNull<c_void>,
) -> Result<NonNull<c_void>, SurfaceError> {
    let interface = WlSurface::interface()
        .c_ptr
        .ok_or_else(|| wl_err("wl_surface has no C interface"))?;
    let factory = compositor.id().as_ptr();
    if factory.is_null() {
        return Err(wl_err("wl_compositor proxy is dead"));
    }
    // One argument: the new_id placeholder libwayland fills in.
    let mut args = [wl_argument { n: 0 }];
    // SAFETY: a live wl_compositor proxy, the create_surface opcode, its single
    // new_id argument and the matching interface/version — exactly what the
    // generated C stub `wl_compositor_create_surface` passes.
    let proxy = unsafe {
        ffi_dispatch!(
            wayland_client_handle(),
            wl_proxy_marshal_array_constructor_versioned,
            factory,
            u32::from(wl_compositor::REQ_CREATE_SURFACE_OPCODE),
            args.as_mut_ptr(),
            interface as *const _,
            compositor.version()
        )
    };
    let proxy = NonNull::new(proxy).ok_or_else(|| wl_err("wl_compositor.create_surface failed"))?;
    // SAFETY: our fresh proxy; no listener is attached, so libwayland never
    // calls anything with this pointer — only GDK reads it (see WHY above).
    unsafe {
        ffi_dispatch!(
            wayland_client_handle(),
            wl_proxy_set_user_data,
            proxy.as_ptr(),
            sink.as_ptr()
        )
    };
    Ok(proxy.cast())
}

/// A 1×1 fully transparent ARGB8888 `wl_shm` buffer.
///
/// Made from a file in `XDG_RUNTIME_DIR` (the tmpfs every Wayland session
/// has) with std only: we never map it — the compositor does — and a
/// zero-filled file is already the transparent pixel (premultiplied 0,0,0,0).
fn transparent_pixel(shm: &WlShm, qh: &QueueHandle<Dispatcher>) -> Result<WlBuffer, SurfaceError> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        SurfaceError::Backend(
            "XDG_RUNTIME_DIR is not set, so this is not a complete Wayland session".into(),
        )
    })?;
    let shm_err = |e: std::io::Error| {
        SurfaceError::Backend(format!(
            "could not create the video surface's 1x1 buffer in XDG_RUNTIME_DIR: {e}"
        ))
    };
    let path = PathBuf::from(dir).join(format!(
        "sharectrl-video-{}-{}.shm",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(shm_err)?;
    // Unlink immediately: the fd keeps it alive for the compositor, and
    // nothing is left behind if we crash.
    let unlinked = std::fs::remove_file(&path);
    file.set_len(4).map_err(shm_err)?;
    unlinked.map_err(shm_err)?;
    let pool = shm.create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    // The buffer keeps the pool's memory; the pool object itself is done.
    pool.destroy();
    Ok(buffer)
}
