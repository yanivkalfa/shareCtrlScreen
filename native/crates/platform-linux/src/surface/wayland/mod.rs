//! `VideoSurface` on Wayland: a `wl_subsurface` tree above WebKitGTK inside
//! the GTK3 toplevel — the L5-spike design (`plans/spikes/L5-wayland-subsurface.md`),
//! the Wayland counterpart of the Windows child HWND.
//!
//! Two layouts, one code path:
//! * **In the app window** (the default): the tree hangs off the app's GTK
//!   toplevel and covers its content widget; the hover-bar reveal crops the
//!   video with its viewport.
//! * **Separate window** (decision #3, pre-authorized fallback): the same tree
//!   in an undecorated window of its own, transient for the app window, with
//!   no toolbar to reveal. Chosen when the compositor has no `wp_viewporter`
//!   (the crop and fractional scaling need it), or when the user sets
//!   `SHARECTRL_VIDEO_WINDOW=separate` (a compositor that fails to hide nested
//!   subsurfaces with their parent, like WSLg's Weston — spike finding 1).
//!
//! Threads that touch the surfaces, and how they are kept apart:
//! * GTK thread — creation, relayout (`size-allocate`), (re)attachment on map,
//!   keys (`gtk_host`).
//! * input thread — pointer events, hover reveal (`input`).
//! * render (viewer media) thread — wgpu present (`presenter`).
//! * any thread — `show` / `hide` (the UI command thread, the media thread).
//!
//! Every request that changes the video surfaces' state, and every commit of
//! them, happens with the **surface lock** (`Shared::locked`) held. Mesa/wgpu
//! sends attach + damage + commit inside one `present`, under that same lock
//! (`Target::commit`), so our own commits (reveal, relayout) can never land
//! between Mesa's attach and its commit and publish half a frame.
//!
//! ## References
//! * `VideoSurface` / `Presenter` contract: `platform-api/src/surface.rs`.
//! * wl_subsurface sync/desync and cached state, wl_surface.attach(NULL)
//!   unmapping, "a sub-surface is hidden if the parent becomes hidden":
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * GstWlWindow resize: set_sync → commit child → commit parent → set_desync
//!   (`gst_wl_window_update_borders` / `gst_wl_window_resize_video_surface`):
//!   <https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwlwindow.c>
//! * `gdk_wayland_window_get_wl_surface`, `gdk_window_new` (GDK_INPUT_ONLY):
//!   <https://docs.gtk.org/gdk3-wayland/>, <https://docs.gtk.org/gdk3/ctor.Window.new.html>
//! * `gtk_main_do_event` drops events on windows without a widget
//!   (gtk/gtkmain.c, "if (!event_widget)"):
//!   <https://gitlab.gnome.org/GNOME/gtk/-/blob/gtk-3-24/gtk/gtkmain.c>

mod gdk_drop;
mod geometry;
mod input;
mod wl;

use std::any::Any;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use gtk::gdk;
use gtk::glib;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use platform_api::{
    FileDrop, Presenter, RawDisplayHandle, RawWindowHandle, SurfaceError, SurfaceHandles,
    VideoInput, VideoSurface,
};
use raw_window_handle::{WaylandDisplayHandle, WaylandWindowHandle};
use surface_wgpu::reveal::Reveal;
use surface_wgpu::SurfaceState;
use wayland_backend::client::ObjectId;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::{Proxy, QueueHandle};

use self::geometry::{buffer_scale_for, Geometry, VideoLayout, SCALE_DENOMINATOR};
use self::input::Dispatcher;
use self::wl::{BuildError, WlObjects};
use super::gtk_host::{self, HostEvents, HostLink, MainBound, Toplevel};
use super::presenter::{Target, WgpuPresenter};
use super::{lock, KeptHandles};

/// Environment switch for the decision-#3 separate window.
const WINDOW_ENV: &str = "SHARECTRL_VIDEO_WINDOW";
/// Key under which the sink window is stored on its GTK toplevel.
const SINK_KEY: &str = "sharectrl-video-sink";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    InWindow,
    Separate,
}

/// What the surface lock protects.
struct Locked {
    geo: Geometry,
    reveal: Reveal,
    /// `area`'s role object under GTK's current toplevel surface; `None` while
    /// the toplevel is unmapped (GDK destroyed its surface).
    area_sub: Option<WlSubsurface>,
    /// The toplevel surface `area_sub` belongs to.
    parent: Option<ObjectId>,
}

/// State shared by the GTK, input and render threads.
pub(crate) struct Shared {
    pub(crate) wl: WlObjects,
    pub(crate) state: SurfaceState,
    mode: Mode,
    /// The surface lock (see the module doc) and the layout it protects.
    locked: Mutex<Locked>,
    shown: AtomicBool,
    /// A `wp_fractional_scale_v1.preferred_scale` arrived; GDK's integer
    /// scale no longer applies.
    fractional_known: AtomicBool,
    pub(crate) stop: AtomicBool,
    qh: QueueHandle<Dispatcher>,
}

impl Shared {
    fn flush(&self) {
        if let Err(e) = self.wl.conn.flush() {
            tracing::warn!("video surface: Wayland flush failed: {e}");
        }
    }

    /// Publish the size and scale every other thread reads.
    fn publish(&self, geo: &Geometry) {
        let (w, h) = geo.physical();
        self.state.set_scale(geo.scale());
        self.state.set_size(w, h);
    }

    /// Queue the video's viewport for `layout` (applied by the next video
    /// commit, which may be Mesa's). Caller holds the surface lock.
    fn send_viewport(&self, layout: &VideoLayout) {
        let Some(vp) = &self.wl.video_viewport else {
            return;
        };
        match layout.source {
            Some((x, y, w, h)) => vp.set_source(x, y, w, h),
            // All −1 = unset: the whole buffer.
            None => vp.set_source(-1.0, -1.0, -1.0, -1.0),
        }
        vp.set_destination(layout.destination.0, layout.destination.1);
    }

    /// Apply the video layout NOW, atomically: the viewport (video state) and
    /// the video's position (area state) become visible in one area commit.
    /// Caller holds the surface lock.
    ///
    /// set_sync makes the video's commit cache instead of apply; the area
    /// commit then applies the area's pending position together with the
    /// video's cached viewport; set_desync returns the video to free-running
    /// presents. Without this the crop and the move could reach the screen in
    /// different frames and the picture would visibly jump by the bar height.
    fn commit_layout(&self, l: &Locked) {
        let layout = l.geo.video_layout();
        self.wl.video_sub.set_sync();
        self.send_viewport(&layout);
        self.wl.video.commit();
        self.wl.video_sub.set_position(0, layout.offset_y);
        self.wl.area.commit();
        self.wl.video_sub.set_desync();
        self.flush();
    }

    /// Map the area (and so the video under it, which maps with its parent).
    /// Caller holds the surface lock. The area is ours and desync: immediate.
    fn map_area(&self) {
        self.wl.area.attach(Some(&self.wl.transparent), 0, 0);
        self.wl.area.damage_buffer(0, 0, 1, 1);
        self.wl.area.commit();
    }

    /// Hang `area` under `toplevel`'s current `wl_surface` (GTK thread). A
    /// no-op when already there.
    fn attach_parent(&self, toplevel: &gtk::Window) {
        let Some(ptr) = toplevel.window().and_then(|w| wl_surface_of(&w)) else {
            return;
        };
        // SAFETY: GDK's live toplevel surface on this display (just read
        // from the realized, mapped GdkWindow).
        let parent = match unsafe { wl::foreign_surface(&self.wl.conn, ptr) } {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("video surface: cannot attach to the window: {e}");
                return;
            }
        };
        let mut l = lock(&self.locked);
        if l.parent.as_ref() == Some(&parent.id()) {
            return;
        }
        if let Some(old) = l.area_sub.take() {
            old.destroy();
        }
        let sub = self
            .wl
            .subcompositor
            .get_subsurface(&self.wl.area, &parent, &self.qh, ());
        // Desync: area's own commits (show/hide, the atomic layout dance) must
        // take effect without waiting for GTK to commit the toplevel.
        sub.set_desync();
        sub.set_position(l.geo.origin.0, l.geo.origin.1);
        // A new sub-surface is stacked top-most among its siblings
        // (wl_subsurface.place_above doc), i.e. above everything GTK draws.
        // place_above(parent) would instead put it just above the parent but
        // BELOW any other subsurface of the toplevel, so it is not called.
        l.area_sub = Some(sub);
        l.parent = Some(parent.id());
        if self.shown.load(Ordering::SeqCst) {
            self.map_area();
        }
        drop(l);
        self.flush();
        // The new sub-surface and its position are parent state ("becomes
        // visible on the next time the state of the parent surface is
        // applied"): make GTK commit even if nothing else needs repainting.
        toplevel.queue_draw();
        tracing::info!("video surface: attached to the window surface");
    }

    /// The toplevel was unmapped: GDK destroyed its surface, which left our
    /// role object inert. Destroy it; the next map attaches afresh.
    fn detach_parent(&self) {
        let mut l = lock(&self.locked);
        if let Some(sub) = l.area_sub.take() {
            sub.destroy();
        }
        l.parent = None;
        drop(l);
        self.flush();
    }

    /// GTK relayout or scale change (GTK thread). Returns whether the area
    /// moved inside the toplevel — that is GTK's state, so the caller must
    /// make GTK commit (queue a draw).
    fn update_allocation(&self, origin: (i32, i32), logical: (u32, u32), gdk_scale: i32) -> bool {
        let mut l = lock(&self.locked);
        let mut next = l.geo;
        next.origin = origin;
        next.logical = (logical.0.max(1), logical.1.max(1));
        if !self.fractional_known.load(Ordering::SeqCst) {
            next.scale120 = gdk_scale.max(1).unsigned_abs() * SCALE_DENOMINATOR;
        }
        if next == l.geo {
            return false;
        }
        let moved = next.origin != l.geo.origin;
        if moved {
            if let Some(sub) = &l.area_sub {
                sub.set_position(origin.0, origin.1);
            }
        }
        l.geo = next;
        self.publish(&next);
        tracing::info!(
            "video surface: layout origin={},{} logical={}x{} scale={:.3}",
            origin.0,
            origin.1,
            next.logical.0,
            next.logical.1,
            next.scale()
        );
        // The destination follows the new size right away, so the last frame
        // is stretched over the new area until the presenter catches up (the
        // Windows behaviour), instead of leaving web content uncovered.
        self.commit_layout(&l);
        moved
    }

    fn set_fractional_scale(&self, scale120: u32) {
        self.fractional_known.store(true, Ordering::SeqCst);
        let mut l = lock(&self.locked);
        if l.geo.scale120 != scale120 {
            tracing::info!(
                "video surface: fractional scale {:.3}",
                f64::from(scale120) / f64::from(SCALE_DENOMINATOR)
            );
            // Only the buffer size changes; the presenter picks it up on the
            // next frame and the logical layout is unaffected.
            l.geo.scale120 = scale120;
            let geo = l.geo;
            self.publish(&geo);
        }
    }

    /// A position on the video (surface-local logical px) in physical px of
    /// the **full** content rect. `feed_reveal`: also run the hover reveal on
    /// it (pointer motion), applying any change atomically.
    pub(crate) fn to_full_px(&self, x: f64, y: f64, feed_reveal: bool) -> (f64, f64) {
        let mut l = lock(&self.locked);
        let scale = l.geo.scale();
        // Surface-local y is relative to the (possibly cropped) video; the
        // full content rect starts `bar` px higher.
        let y_full = y + f64::from(l.geo.bar_logical());
        if feed_reveal
            && self.mode == Mode::InWindow
            && l.reveal.on_pointer(y_full * scale, scale).is_some()
        {
            l.geo.revealed = l.reveal.is_revealed();
            self.commit_layout(&l);
        }
        (x * scale, y_full * scale)
    }

    /// Pointer motion on the video (input thread).
    fn pointer_moved(&self, x: f64, y: f64) {
        let (px, py) = self.to_full_px(x, y, true);
        let (nx, ny) = self.state.normalize(px, py);
        self.state.input().moved(nx, ny);
        // Task D10: predict the cursor sprite at the real mouse. The OS cursor
        // is not hidden on Wayland (no portable GDK hook), so the sprite simply
        // coincides with it — the fallback the card allows — and still tracks
        // the pointer instantly on a high-RTT link.
        self.state.set_local_pointer(nx, ny);
        self.state.set_pointer_inside(true);
    }

    /// The pointer left the video (task D10): stop predicting the sprite so it
    /// falls back to the host's out-of-band position.
    fn pointer_left(&self) {
        self.state.set_pointer_inside(false);
    }

    /// Pointer button on the video at its last position (input thread).
    fn pointer_button(&self, button: u8, down: bool, x: f64, y: f64) {
        let (px, py) = self.to_full_px(x, y, false);
        let (nx, ny) = self.state.normalize(px, py);
        self.state.input().button(button, down, nx, ny);
    }

    fn reset_reveal(&self, l: &mut Locked) {
        l.reveal.reset();
        l.geo.revealed = false;
    }
}

impl HostEvents for Shared {
    fn state(&self) -> &SurfaceState {
        &self.state
    }

    fn wants_keys(&self) -> bool {
        self.shown.load(Ordering::SeqCst) && self.state.input().has_sink()
    }

    fn relayout(&self, toplevel: &gtk::Window, origin: (i32, i32), size: (u32, u32), scale: i32) {
        if self.update_allocation(origin, size, scale) {
            // The area's position is GTK's (double-buffered) state; make GTK
            // commit so it applies even if nothing else needs repainting.
            toplevel.queue_draw();
        }
    }

    fn mapped(&self, toplevel: &gtk::Window) {
        self.attach_parent(toplevel);
    }

    fn unmapped(&self) {
        self.detach_parent();
    }
}

impl Target for Shared {
    fn handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        (
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(self.wl.display)),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(self.wl.video_ptr)),
        )
    }

    fn state(&self) -> &SurfaceState {
        &self.state
    }

    fn is_shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }

    /// The surface lock covers the Wayland requests: Mesa's attach + damage +
    /// commit (inside `present`) must not interleave with a reveal or relayout
    /// commit, and the viewport must describe the committed buffer.
    fn commit(&self, size: (u32, u32), present: &mut dyn FnMut()) -> bool {
        let mut l = lock(&self.locked);
        // Hidden: never present. The unmapped surface gets no frame
        // callbacks, so a FIFO swapchain would block in its next acquire
        // forever; `show` re-maps the last committed buffer.
        if !self.shown.load(Ordering::SeqCst) {
            return false;
        }
        l.geo.attached = Some(size);
        if l.geo.viewport {
            self.send_viewport(&l.geo.video_layout());
        } else {
            self.wl
                .video
                .set_buffer_scale(buffer_scale_for(size, l.geo.int_scale()));
        }
        present();
        true
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The presenter's swapchain (which draws into `video`) is gone by now:
        // it holds an `Arc` to this. Children before parents, role objects
        // before their surfaces (wl_surface `defunct_role_object`).
        let l = lock(&self.locked);
        if let Some(sub) = &l.area_sub {
            sub.destroy();
        }
        drop(l);
        if let Some(f) = &self.wl.fractional {
            f.destroy();
        }
        if let Some(vp) = &self.wl.video_viewport {
            vp.destroy();
        }
        self.wl.video_sub.destroy();
        self.wl.video.destroy();
        if let Some(vp) = &self.wl.area_viewport {
            vp.destroy();
        }
        self.wl.area.destroy();
        self.wl.transparent.destroy();
        self.flush();
    }
}

/// The Wayland video surface.
pub(crate) struct WaylandSurface {
    shared: Arc<Shared>,
    input: Option<JoinHandle<()>>,
    host: Option<HostLink>,
    /// Routes drags on the sink window to the drop sink while it lives
    /// (removed on the main thread when the surface drops).
    _drops: MainBound<gdk_drop::RouteToken>,
    /// Handles for window-level hooks (in-window layout only).
    handles: Option<KeptHandles>,
}

fn wl_surface_of(window: &gdk::Window) -> Option<NonNull<c_void>> {
    let ptr: *mut gdk::ffi::GdkWindow = window.to_glib_none().0;
    // SAFETY: a live GdkWindow of the Wayland backend (`create` checked the
    // display); returns its wl_surface or NULL while it has none (unmapped).
    let surface = unsafe { gdk_wayland_sys::gdk_wayland_window_get_wl_surface(ptr.cast()) };
    NonNull::new(surface.cast::<c_void>())
}

/// The never-shown, widget-less GdkWindow whose pointer becomes the video
/// surface's `wl_surface` user data (why: `wl::create_surface_with_user_data`).
///
/// It is stored on the toplevel, so it lives exactly as long as the toplevel
/// object, which outlives every surface it parents.
fn create_sink(toplevel: &gtk::Window) -> Result<(gdk::Window, NonNull<c_void>), SurfaceError> {
    let attrs = gdk::WindowAttr {
        window_type: gdk::WindowType::Toplevel,
        wclass: gdk::WindowWindowClass::InputOnly,
        width: 1,
        height: 1,
        // No input events selected: pointer/key events aimed at it are
        // filtered by GDK. Drag-and-drop events are not masked; `gdk_drop`
        // takes those.
        event_mask: gdk::EventMask::empty(),
        ..gdk::WindowAttr::default()
    };
    let sink = gdk::Window::new(None, &attrs);
    let raw: *mut gdk::ffi::GdkWindow = sink.to_glib_none().0;
    let ptr = NonNull::new(raw)
        .ok_or_else(|| SurfaceError::Backend("gdk_window_new returned NULL".into()))?;
    // SAFETY: the value type matches every later access (none — it is only
    // kept alive); glib drops it when the toplevel is finalized. The object,
    // and so `ptr`, stays valid until then.
    unsafe { toplevel.set_data(SINK_KEY, sink.clone()) };
    Ok((sink, ptr.cast::<c_void>()))
}

/// The decision-#3 window: undecorated, transient for the app window, black
/// until the video covers it. Realized, not shown (`show` shows it).
fn separate_window(app: &Toplevel) -> Toplevel {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("ShareCtrlScreen — remote screen");
    window.set_decorated(false);
    window.set_transient_for(Some(&app.window));
    window.set_default_size(
        app.content.allocated_width().max(640),
        app.content.allocated_height().max(360),
    );
    let content = gtk::DrawingArea::new();
    content.connect_draw(|_, cr| {
        cr.set_source_rgb(0.0, 0.0, 0.0);
        if let Err(e) = cr.paint() {
            tracing::debug!("video window: background paint failed: {e}");
        }
        glib::Propagation::Stop
    });
    window.add(&content);
    content.show();
    window.realize();
    Toplevel {
        window,
        content: content.upcast(),
    }
}

/// `Platform::create_surface` for Wayland. Call on the GTK thread with the
/// app window's handles, after the window is shown.
pub(crate) fn create(
    parent: NonNull<c_void>,
    display: RawDisplayHandle,
) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
    let RawDisplayHandle::Wayland(display_handle) = display else {
        return Err(SurfaceError::UnsupportedHandle);
    };
    let gdk_display = gdk::Display::default()
        .ok_or_else(|| SurfaceError::Backend("GTK has no display".into()))?;
    if !gdk_display.backend().is_wayland() {
        return Err(SurfaceError::Backend(
            "the app window is a Wayland window but GTK is not running on Wayland".into(),
        ));
    }
    let app = gtk_host::find_toplevel(|w| wl_surface_of(w) == Some(parent))?;
    let forced = std::env::var(WINDOW_ENV).is_ok_and(|v| v == "separate");
    let wl_display = display_handle.display;
    let in_window = if forced {
        tracing::info!("video surface: {WINDOW_ENV}=separate, using the separate window");
        None
    } else {
        let (sink, sink_ptr) = create_sink(&app.window)?;
        // SAFETY: GDK's live connection; the sink is owned by the app
        // toplevel, which outlives the surfaces it parents.
        match unsafe { wl::create(wl_display, sink_ptr, true) } {
            Ok(created) => Some((created, sink)),
            Err(BuildError::NoViewporter) => {
                tracing::warn!(
                    "video surface: the compositor has no wp_viewporter; showing the remote \
                     screen in a separate window"
                );
                None
            }
            Err(BuildError::Failed(e)) => return Err(e),
        }
    };
    let (mode, top, created, sink) = match in_window {
        Some((created, sink)) => (Mode::InWindow, app, created, sink),
        None => {
            let top = separate_window(&app);
            let (sink, sink_ptr) = create_sink(&top.window)?;
            // SAFETY: as above; this sink is owned by the separate window,
            // which outlives the surfaces it parents (it is destroyed with the
            // host link, after the tree is detached by its unmap).
            let created = unsafe { wl::create(wl_display, sink_ptr, false) }?;
            (Mode::Separate, top, created, sink)
        }
    };
    let viewport = created.objects.video_viewport.is_some();
    let shared = Arc::new(Shared {
        wl: created.objects,
        state: SurfaceState::new(),
        mode,
        locked: Mutex::new(Locked {
            geo: Geometry::new(viewport),
            reveal: Reveal::default(),
            area_sub: None,
            parent: None,
        }),
        shown: AtomicBool::new(false),
        fractional_known: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        qh: created.queue.handle(),
    });
    let input = input::spawn(shared.clone(), created.queue, &created.seat)
        .map_err(|e| SurfaceError::Backend(format!("cannot start the video input thread: {e}")))?;
    let drops = MainBound::new(gdk_drop::route(&sink, Arc::downgrade(&shared)));
    let host = gtk_host::connect(&top, shared.clone(), mode == Mode::Separate);
    if mode == Mode::InWindow {
        // Already mapped: no `map` signal will come for this mapping.
        shared.attach_parent(&top.window);
    }
    let handles = (mode == Mode::InWindow).then(|| {
        KeptHandles(SurfaceHandles {
            video: RawWindowHandle::Wayland(WaylandWindowHandle::new(shared.wl.video_ptr)),
            parent: RawWindowHandle::Wayland(WaylandWindowHandle::new(parent)),
            display,
        })
    });
    let (w, h) = shared.state.size();
    tracing::info!("video surface: Wayland subsurface created ({mode:?}, {w}x{h} px)");
    Ok(Arc::new(WaylandSurface {
        shared,
        input: Some(input),
        host: Some(host),
        _drops: drops,
        handles,
    }))
}

impl VideoSurface for WaylandSurface {
    fn show(&self) {
        let s = &self.shared;
        if s.mode == Mode::Separate {
            if let Some(host) = &self.host {
                // Maps the window; its `map` handler attaches the tree.
                host.set_owned_visible(true);
            }
        }
        let mut l = lock(&s.locked);
        s.reset_reveal(&mut l);
        s.commit_layout(&l);
        s.shown.store(true, Ordering::SeqCst);
        s.map_area();
        s.flush();
        let (w, h) = l.geo.physical();
        tracing::info!(
            "video surface: show {w}x{h} (logical {}x{}, scale {:.3})",
            l.geo.logical.0,
            l.geo.logical.1,
            l.geo.scale()
        );
    }

    fn hide(&self) {
        let s = &self.shared;
        {
            let mut l = lock(&s.locked);
            s.shown.store(false, Ordering::SeqCst);
            // A NULL buffer on the area unmaps the whole tree at once — the
            // compositor applies it before GTK's next frame (the modal) arrives.
            s.wl.area.attach(None, 0, 0);
            s.wl.area.commit();
            s.reset_reveal(&mut l);
            s.commit_layout(&l);
        }
        if s.mode == Mode::Separate {
            if let Some(host) = &self.host {
                host.set_owned_visible(false);
            }
        }
        s.state.clear_video_size();
        // Keys stop being intercepted now; their releases would go to WebKit.
        s.state.input().release_all();
    }

    /// Nothing to poll: the layout follows GTK's `size-allocate` on the UI
    /// thread (where the allocation actually changes), and the presenter
    /// resizes its swapchain from the same geometry on the next frame.
    fn fit(&self) {}

    fn client_size(&self) -> (u32, u32) {
        self.shared.state.size()
    }

    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        self.shared.state.input().set_sink(sink);
    }

    /// Drops on the video arrive through GDK's own data device on the sink
    /// window, taken there by `gdk_drop` before GTK would drop them.
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        self.shared.state.set_drop_sink(sink);
    }

    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Ok(Box::new(WgpuPresenter::new(self.shared.clone())?))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    /// `None` for the separate window: its toplevel surface is re-created on
    /// every show, so no handle to it stays valid for the surface's lifetime.
    fn native_handles(&self) -> Option<SurfaceHandles> {
        self.handles.map(|h| h.0)
    }

    fn local_pointer(&self) -> Option<(f64, f64)> {
        self.shared.state.local_pointer()
    }

    // `set_cursor_control` stays the default no-op: there is no portable GDK /
    // wl_pointer way to hide the OS cursor here, so the sprite is drawn at the
    // local position and simply coincides with it (the card's fallback).
}

impl Drop for WaylandSurface {
    fn drop(&mut self) {
        // Release anything still held so the host is not left with a stuck key.
        self.shared.state.input().set_sink(None);
        self.host.take();
        let s = &self.shared;
        s.stop.store(true, Ordering::SeqCst);
        // Wake the input thread: the sync's `done` lands on our queue, and the
        // stop flag is checked after every dispatch.
        s.wl.conn.display().sync(&s.qh, ());
        s.flush();
        if let Some(t) = self.input.take() {
            let _ = t.join();
        }
    }
}
