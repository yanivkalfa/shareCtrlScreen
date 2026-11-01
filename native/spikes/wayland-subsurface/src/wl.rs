//! The Wayland objects behind the video surface, created on GDK's own
//! `wl_display` connection (a subsurface must belong to the same client as its
//! parent, so a second connection is not an option).
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
//! `area` exists so that every geometry change of `video` — its viewport (video
//! state) and its position (area state) — can be applied atomically by a commit
//! WE own, without waiting for GTK to commit the toplevel. Only area's own
//! position lives in GTK's state; it changes only on relayout, when GTK commits
//! anyway.
//!
//! ## References
//! * wl_subsurface (mapping rules, sync/desync, "initially added as the top-most
//!   in the stack", "never has the keyboard focus"), wl_surface.set_input_region:
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * libwayland client API (`wl_proxy_marshal_array_constructor_versioned`,
//!   `wl_proxy_set_user_data`, multi-threaded `prepare_read`):
//!   <https://wayland.freedesktop.org/docs/html/apb.html>
//! * wayland-backend `Backend::from_foreign_display`, `ObjectId::from_ptr`
//!   (guest mode on a C-owned connection): <https://docs.rs/wayland-backend/0.3/wayland_backend/sys/client/index.html>
//! * GDK 3 reads `wl_surface_get_user_data()` of every surface a seat event names
//!   and treats it as a `GdkWindow` (pointer/keyboard/DnD/tablet check
//!   `GDK_IS_WINDOW` first; **touch does not**, `gdk_wayland_seat_add_touch`):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gdk/wayland/gdkdevice-wayland.c>
//! * GStreamer gtkwaylandsink / GstWlWindow (area + video subsurfaces, sync dance
//!   for atomic resizes): <https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwlwindow.c>

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::ptr::NonNull;

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

use crate::error::SpikeError;
use crate::input::InputState;

/// Raw libwayland pointers handed to wgpu and to the wl_surface user data.
#[derive(Debug, Clone, Copy)]
pub struct RawPtrs {
    pub display: NonNull<c_void>,
    pub video_surface: NonNull<c_void>,
}

// SAFETY: these are libwayland object pointers. libwayland-client serializes
// requests on a display with its own mutex, so sending from any thread is
// allowed; the objects live for the whole process (GDK's display is never
// closed while the app runs, and the video surface is never destroyed — the
// same lifetime as the Windows child HWND).
unsafe impl Send for RawPtrs {}
// SAFETY: as above — only copied and passed to thread-safe C APIs.
unsafe impl Sync for RawPtrs {}

/// The objects every later operation needs.
pub struct WlObjects {
    pub conn: Connection,
    pub raw: RawPtrs,
    pub area: WlSurface,
    pub area_sub: WlSubsurface,
    pub transparent: WlBuffer,
    pub video: WlSurface,
    pub video_sub: WlSubsurface,
    pub video_viewport: WpViewport,
}

/// Everything `create` produces: the shared objects plus what only the input
/// thread uses.
pub struct Created {
    pub objects: WlObjects,
    pub queue: EventQueue<InputState>,
    pub seat: WlSeat,
}

/// Build the surface tree under `parent` (GTK's toplevel `wl_surface`).
///
/// # Safety
/// `display` must be GDK's live `wl_display` and `parent` a live `wl_surface`
/// on it, both outliving the process' use of the video surface; `sink` must be
/// a `GdkWindow` that outlives `parent` (see `gtk_side::create_sink`).
pub unsafe fn create(
    display: NonNull<c_void>,
    parent: NonNull<c_void>,
    sink: NonNull<c_void>,
) -> Result<Created, SpikeError> {
    // SAFETY: the caller guarantees a live GDK display. Guest mode never closes
    // GDK's connection.
    let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    // Our own queue: GDK dispatches its default queue on the GTK thread, we
    // dispatch this one on the input thread (libwayland's prepare_read protocol
    // makes two readers of one fd safe, and GDK's event source uses it).
    let (globals, queue) = registry_queue_init::<InputState>(&conn)
        .map_err(|e| SpikeError::Wayland(format!("registry: {e}")))?;
    let qh = queue.handle();

    // damage_buffer (v4) is what wgpu/Mesa and our area buffer use.
    let compositor: WlCompositor = bind(&globals, &qh, "wl_compositor", 4, 6)?;
    let subcompositor: WlSubcompositor = bind(&globals, &qh, "wl_subcompositor", 1, 1)?;
    let viewporter: WpViewporter = bind(&globals, &qh, "wp_viewporter", 1, 1)?;
    let shm: WlShm = bind(&globals, &qh, "wl_shm", 1, 1)?;
    // v5 adds wl_pointer.frame/axis_discrete, v8 axis_value120. v1 still works
    // (no frames: wheel is flushed per event).
    let seat: WlSeat = bind(&globals, &qh, "wl_seat", 1, 8)?;
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    // SAFETY: `parent` is a live wl_surface on this display (caller contract);
    // the id stays valid as long as GDK keeps the toplevel realized.
    let parent = unsafe { foreign_surface(&conn, parent) }?;

    let area = compositor.create_surface(&qh, ());
    let area_sub = subcompositor.get_subsurface(&area, &parent, &qh, ());
    // Desync: area's own commits (show/hide, the atomic layout dance) must take
    // effect without waiting for GTK to commit the toplevel.
    area_sub.set_desync();
    // Pointer, touch and drag-and-drop focus follow the input region, so an
    // empty one guarantees the area never becomes the focus of any seat device —
    // it is invisible plumbing and GDK never sees it named in an event.
    let empty = compositor.create_region(&qh, ());
    area.set_input_region(Some(&empty));
    empty.destroy();
    // The area is 1×1 logical: its children are not clipped to it, and a
    // 1-pixel transparent quad costs the compositor nothing to blend.
    let area_viewport = viewporter.get_viewport(&area, &qh, ());
    area_viewport.set_destination(1, 1);
    let transparent = transparent_pixel(&shm, &qh)?;

    // SAFETY: compositor is a live proxy on our queue; sink outlives parent.
    let video_ptr = unsafe { create_surface_with_user_data(&compositor, sink) }?;
    // SAFETY: just created, valid until process exit (never destroyed).
    let video = unsafe { foreign_surface(&conn, video_ptr) }?;
    let video_sub = subcompositor.get_subsurface(&video, &area, &qh, ());
    // Desync: wgpu presents on its own thread at the stream's rate; the frames
    // must not wait for any other surface's commit.
    video_sub.set_desync();
    let video_viewport = viewporter.get_viewport(&video, &qh, ());
    // The video is opaque (letterbox bars are drawn black), which lets the
    // compositor skip blending and painting what lies beneath.
    let opaque = compositor.create_region(&qh, ());
    opaque.add(0, 0, i32::MAX, i32::MAX);
    video.set_opaque_region(Some(&opaque));
    opaque.destroy();
    // The scale object reports on our queue (input thread). wayland-rs proxies
    // are handles: dropping ours does not destroy the protocol object.
    let fractional: Option<WpFractionalScaleV1> = fractional_mgr
        .as_ref()
        .map(|m| m.get_fractional_scale(&video, &qh, ()));

    // A new sub-surface is stacked top-most among its siblings and the parent
    // (wl_subsurface.place_above doc), i.e. above everything GTK draws. Calling
    // place_above(parent) would instead put it just above the parent but BELOW
    // any other subsurface of the toplevel, so we deliberately do not.

    conn.flush()
        .map_err(|e| SpikeError::Wayland(format!("flush: {e}")))?;
    tracing::info!(
        "wayland: globals ok (compositor v{}, seat v{}, fractional-scale {})",
        compositor.version(),
        seat.version(),
        if fractional.is_some() { "yes" } else { "no" }
    );
    Ok(Created {
        objects: WlObjects {
            conn,
            raw: RawPtrs {
                display,
                video_surface: video_ptr,
            },
            area,
            area_sub,
            transparent,
            video,
            video_sub,
            video_viewport,
        },
        queue,
        seat,
    })
}

fn bind<I>(
    globals: &GlobalList,
    qh: &QueueHandle<InputState>,
    interface: &'static str,
    min: u32,
    max: u32,
) -> Result<I, SpikeError>
where
    I: Proxy + 'static,
    InputState: wayland_client::Dispatch<I, ()>,
{
    globals
        .bind(qh, min..=max, ())
        .map_err(|_| SpikeError::MissingGlobal {
            interface,
            version: min,
        })
}

/// Wrap a C-owned `wl_surface` as a wayland-client proxy (requests only; its
/// events stay with whoever owns it).
///
/// # Safety
/// `ptr` must be a live `wl_surface` proxy on `conn`'s display.
unsafe fn foreign_surface(
    conn: &Connection,
    ptr: NonNull<c_void>,
) -> Result<WlSurface, SpikeError> {
    // SAFETY: caller contract; from_ptr checks the interface name.
    let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr.as_ptr().cast::<wl_proxy>()) }
        .map_err(|e| SpikeError::Wayland(format!("foreign wl_surface: {e}")))?;
    WlSurface::from_id(conn, id)
        .map_err(|e| SpikeError::Wayland(format!("foreign wl_surface: {e}")))
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
/// surface's ability to receive input (it is unmapped with its GTK parent).
unsafe fn create_surface_with_user_data(
    compositor: &WlCompositor,
    sink: NonNull<c_void>,
) -> Result<NonNull<c_void>, SpikeError> {
    let interface = WlSurface::interface()
        .c_ptr
        .ok_or_else(|| SpikeError::Wayland("wl_surface has no C interface".into()))?;
    let factory = compositor.id().as_ptr();
    if factory.is_null() {
        return Err(SpikeError::Wayland("wl_compositor proxy is dead".into()));
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
            wl_compositor::REQ_CREATE_SURFACE_OPCODE as u32,
            args.as_mut_ptr(),
            interface as *const _,
            compositor.version()
        )
    };
    let proxy = NonNull::new(proxy)
        .ok_or_else(|| SpikeError::Wayland("wl_compositor.create_surface failed".into()))?;
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
/// Made from a file in `XDG_RUNTIME_DIR` (the tmpfs every Wayland session has)
/// with std only: we never map it — the compositor does — and a zero-filled
/// file is already the transparent pixel (premultiplied 0,0,0,0).
fn transparent_pixel(shm: &WlShm, qh: &QueueHandle<InputState>) -> Result<WlBuffer, SpikeError> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(SpikeError::NoRuntimeDir)?;
    let path = PathBuf::from(dir).join(format!("sharectrl-video-{}.shm", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(SpikeError::Shm)?;
    // Unlink immediately: the fd keeps it alive for the compositor, and nothing
    // is left behind if we crash.
    let unlinked = std::fs::remove_file(&path);
    file.set_len(4).map_err(SpikeError::Shm)?;
    unlinked.map_err(SpikeError::Shm)?;
    let pool = shm.create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    // The buffer keeps the pool's memory; the pool object itself is done.
    pool.destroy();
    Ok(buffer)
}
