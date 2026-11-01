//! `VideoSurface` on Wayland: a `wl_subsurface` tree above WebKitGTK inside the
//! GTK3 toplevel — the Wayland counterpart of the Windows `ChildWindowSurface`
//! (`native/crates/platform-windows/src/adapters.rs`, `render/src/window.rs`).
//!
//! Threads that touch the surfaces, and how they are kept apart:
//! * GTK thread — creation, relayout (`size-allocate`), keys.
//! * input thread — pointer events, hover reveal.
//! * render (viewer media) thread — wgpu present, `fit`.
//! * any thread — `show` / `hide` (the UI command thread).
//!
//! Every request that changes the video surface's state, and every commit of
//! it, happens with the **surface lock** (`Shared::geo`) held. Mesa/wgpu sends
//! attach + damage + commit inside one `present`, under that same lock, so our
//! own commits (reveal, relayout) can never land between Mesa's attach and its
//! commit and publish half a frame.
//!
//! ## References
//! * `VideoSurface` / `Presenter` contract: `native/crates/platform-api/src/surface.rs`
//! * wl_subsurface sync/desync and cached state:
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * GstWlWindow resize: set_sync → commit child → commit parent → set_desync
//!   (`gst_wl_window_update_borders` / `gst_wl_window_resize_video_surface`):
//!   <https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwlwindow.c>

use std::any::Any;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use platform_api::{
    FileDrop, Presenter, RawDisplayHandle, RawWindowHandle, SurfaceError, VideoInput, VideoSurface,
};

use crate::error::SpikeError;
use crate::geometry::{normalize, Geometry, VideoLayout, SCALE_DENOMINATOR};
use crate::wl::WlObjects;

/// Lock a mutex whose data stays consistent across a panic. Every guarded value
/// here is plain data written in single statements, so a poisoned lock still
/// holds a valid value — and refusing input or present after an unrelated
/// panic would only add a frozen video to the failure.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Input held down on the video, so focus loss / hide / sink removal can
/// release it (the host must never keep a stuck key or button).
#[derive(Default)]
struct Held {
    /// evdev key codes.
    keys: Vec<u16>,
    buttons: Vec<u8>,
    /// Last normalized pointer position, for synthesized button releases.
    last_pos: (f64, f64),
}

/// State shared by the GTK, input and render threads.
pub struct Shared {
    pub(crate) wl: WlObjects,
    /// The surface lock (see the module doc) and the layout it protects.
    geo: Mutex<Geometry>,
    shown: AtomicBool,
    /// A `wp_fractional_scale_v1.preferred_scale` arrived; GDK's integer scale
    /// no longer applies.
    fractional_known: AtomicBool,
    input_sink: Mutex<Option<Sender<VideoInput>>>,
    held: Mutex<Held>,
    /// Last presented frame size (`w << 32 | h`), for letterbox normalization.
    video_dims: AtomicU64,
}

impl Shared {
    pub(crate) fn new(wl: WlObjects, geometry: Geometry) -> Self {
        Self {
            wl,
            geo: Mutex::new(geometry),
            shown: AtomicBool::new(false),
            fractional_known: AtomicBool::new(false),
            input_sink: Mutex::new(None),
            held: Mutex::new(Held {
                last_pos: (0.5, 0.5),
                ..Held::default()
            }),
            video_dims: AtomicU64::new(0),
        }
    }

    /// Take the surface lock.
    pub(crate) fn lock_geo(&self) -> MutexGuard<'_, Geometry> {
        lock(&self.geo)
    }

    pub(crate) fn is_shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }

    pub(crate) fn raw_display(&self) -> NonNull<c_void> {
        self.wl.raw.display
    }

    pub(crate) fn raw_video_surface(&self) -> NonNull<c_void> {
        self.wl.raw.video_surface
    }

    /// Queue the video's viewport for `layout` (applied by the next video commit,
    /// which may be Mesa's). Caller holds the surface lock.
    pub(crate) fn send_viewport(&self, layout: &VideoLayout) {
        let vp = &self.wl.video_viewport;
        match layout.source {
            Some((x, y, w, h)) => vp.set_source(x, y, w, h),
            // All −1 = unset: the whole buffer.
            None => vp.set_source(-1.0, -1.0, -1.0, -1.0),
        }
        vp.set_destination(layout.destination.0, layout.destination.1);
    }

    /// Apply `geo`'s video layout NOW, atomically: the viewport (video state)
    /// and the video's position (area state) become visible in one area commit.
    /// Caller holds the surface lock.
    ///
    /// set_sync makes the video's commit cache instead of apply; the area commit
    /// then applies the area's pending position together with the video's cached
    /// viewport; set_desync returns the video to free-running presents. Without
    /// this the crop and the move could reach the screen in different frames and
    /// the picture would visibly jump by the bar height.
    fn commit_layout(&self, geo: &Geometry) {
        let layout = geo.video_layout();
        self.wl.video_sub.set_sync();
        self.send_viewport(&layout);
        self.wl.video.commit();
        self.wl.video_sub.set_position(0, layout.offset_y);
        self.wl.area.commit();
        self.wl.video_sub.set_desync();
        self.flush();
    }

    fn flush(&self) {
        if let Err(e) = self.wl.conn.flush() {
            tracing::warn!("video surface: Wayland flush failed: {e}");
        }
    }

    /// GTK relayout or scale change (GTK thread). Returns whether the area moved
    /// inside the toplevel — that is GTK's state, so the caller must make GTK
    /// commit (queue a draw).
    pub(crate) fn update_allocation(
        &self,
        origin: (i32, i32),
        logical: (u32, u32),
        gdk_scale: i32,
    ) -> bool {
        let mut geo = self.lock_geo();
        let mut next = *geo;
        next.origin = origin;
        next.logical = (logical.0.max(1), logical.1.max(1));
        if !self.fractional_known.load(Ordering::SeqCst) {
            next.scale120 = gdk_scale.max(1) as u32 * SCALE_DENOMINATOR;
        }
        if next == *geo {
            return false;
        }
        let moved = next.origin != geo.origin;
        if moved {
            self.wl.area_sub.set_position(origin.0, origin.1);
        }
        *geo = next;
        tracing::info!(
            "video surface: layout origin={},{} logical={}x{} scale={:.3}",
            origin.0,
            origin.1,
            next.logical.0,
            next.logical.1,
            next.scale()
        );
        // The destination follows the new size right away, so the last frame is
        // stretched over the new area until the presenter catches up (the Windows
        // behaviour), instead of leaving web content uncovered.
        self.commit_layout(&geo);
        moved
    }

    pub(crate) fn set_fractional_scale(&self, scale120: u32) {
        self.fractional_known.store(true, Ordering::SeqCst);
        let mut geo = self.lock_geo();
        if geo.scale120 != scale120 {
            tracing::info!(
                "video surface: fractional scale {:.3}",
                scale120 as f64 / 120.0
            );
            // Only the buffer size changes; the presenter picks it up on the next
            // frame and the logical layout is unaffected.
            geo.scale120 = scale120;
        }
    }

    pub(crate) fn set_video_dims(&self, w: u32, h: u32) {
        self.video_dims
            .store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    }

    fn video_dims(&self) -> (u32, u32) {
        let v = self.video_dims.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    pub(crate) fn emit(&self, ev: VideoInput) {
        if let Some(tx) = lock(&self.input_sink).as_ref() {
            // A closed receiver means the session ended; dropping is correct.
            let _ = tx.send(ev);
        }
    }

    /// Pointer motion on the video, surface-local logical px (input thread).
    pub(crate) fn pointer_moved(&self, x: f64, y: f64) {
        let (rect, y_full) = {
            let mut geo = self.lock_geo();
            // Surface-local y is relative to the (possibly cropped) video; the
            // full content rect starts `bar` px higher.
            let y_full = y + geo.bar_logical() as f64;
            if let Some(revealed) = geo.reveal_transition(y_full) {
                geo.revealed = revealed;
                self.commit_layout(&geo);
                tracing::debug!(
                    "video surface: bar {}",
                    if revealed { "revealed" } else { "hidden" }
                );
            }
            (geo.logical, y_full)
        };
        let (nx, ny) = normalize(rect, self.video_dims(), x, y_full);
        lock(&self.held).last_pos = (nx, ny);
        self.emit(VideoInput::Move { nx, ny });
    }

    pub(crate) fn pointer_button(&self, button: u8, down: bool) {
        let (nx, ny) = {
            let mut held = lock(&self.held);
            if down {
                if !held.buttons.contains(&button) {
                    held.buttons.push(button);
                }
            } else {
                held.buttons.retain(|&b| b != button);
            }
            held.last_pos
        };
        self.emit(VideoInput::Button {
            button,
            down,
            nx,
            ny,
        });
    }

    /// Whether the GTK key handlers should take keys away from WebKit.
    pub(crate) fn intercepts_keys(&self) -> bool {
        self.is_shown()
    }

    /// A key on the GTK toplevel while the video is shown (GTK thread).
    pub(crate) fn key(&self, evdev: u16, down: bool) {
        {
            let mut held = lock(&self.held);
            if down {
                if !held.keys.contains(&evdev) {
                    held.keys.push(evdev);
                }
            } else {
                held.keys.retain(|&k| k != evdev);
            }
        }
        if let Some(code) = crate::keys::dom_code(evdev) {
            self.emit(VideoInput::Key { code, down });
        }
    }

    /// Synthesize releases for everything held (focus loss, hide, sink removal).
    pub(crate) fn release_all_held(&self) {
        let (keys, buttons, (nx, ny)) = {
            let mut held = lock(&self.held);
            (
                std::mem::take(&mut held.keys),
                std::mem::take(&mut held.buttons),
                held.last_pos,
            )
        };
        if keys.is_empty() && buttons.is_empty() {
            return;
        }
        tracing::debug!(
            "video surface: releasing {} key(s), {} button(s)",
            keys.len(),
            buttons.len()
        );
        for code in keys.into_iter().filter_map(crate::keys::dom_code) {
            self.emit(VideoInput::Key { code, down: false });
        }
        for button in buttons {
            self.emit(VideoInput::Button {
                button,
                down: false,
                nx,
                ny,
            });
        }
    }
}

/// The Wayland video surface.
pub struct WaylandSubsurface {
    shared: Arc<Shared>,
}

impl WaylandSubsurface {
    /// `Platform::create_surface` for Wayland. Call on the GTK thread with the
    /// app window's handles, after the window is shown (realized).
    pub fn create(
        window: RawWindowHandle,
        display: RawDisplayHandle,
    ) -> Result<Arc<Self>, SurfaceError> {
        let (parent, display) = match (window, display) {
            (RawWindowHandle::Wayland(w), RawDisplayHandle::Wayland(d)) => (w.surface, d.display),
            (RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_), _) => {
                return Err(SpikeError::NotWayland("X11").into())
            }
            _ => return Err(SpikeError::NotWayland("non-Wayland").into()),
        };
        let parts = crate::gtk_side::find(parent)?;
        let sink = crate::gtk_side::create_sink(&parts.toplevel)?;
        // SAFETY: `display` is GDK's live connection and `parent` the realized
        // toplevel's surface (found through GDK just above); the sink is owned by
        // the toplevel, whose surface is destroyed (unrealized) before the
        // toplevel object — and the sink with it — is finalized.
        let created = unsafe { crate::wl::create(display, parent, sink) }?;
        let shared = Arc::new(Shared::new(created.objects, Geometry::default()));
        crate::input::spawn(shared.clone(), created.queue, &created.seat)
            .map_err(|e| SurfaceError::Backend(format!("video input thread: {e}")))?;
        crate::gtk_side::connect(&parts, shared.clone());
        tracing::info!("native video subsurface created");
        Ok(Arc::new(Self { shared }))
    }
}

impl VideoSurface for WaylandSubsurface {
    fn show(&self) {
        let s = &self.shared;
        let mut geo = s.lock_geo();
        geo.revealed = false;
        s.commit_layout(&geo);
        s.shown.store(true, Ordering::SeqCst);
        // Mapping the area maps the video under it (subsurfaces map with their
        // parent); the area is ours and desync, so this is immediate.
        s.wl.area.attach(Some(&s.wl.transparent), 0, 0);
        s.wl.area.damage_buffer(0, 0, 1, 1);
        s.wl.area.commit();
        s.flush();
        tracing::info!(
            "video surface: show {}x{} (logical {}x{}, scale {:.3})",
            geo.physical().0,
            geo.physical().1,
            geo.logical.0,
            geo.logical.1,
            geo.scale()
        );
    }

    fn hide(&self) {
        let s = &self.shared;
        {
            let mut geo = s.lock_geo();
            s.shown.store(false, Ordering::SeqCst);
            // A NULL buffer on the area unmaps the whole tree at once — the
            // compositor applies it before GTK's next frame (the modal) arrives.
            s.wl.area.attach(None, 0, 0);
            s.wl.area.commit();
            geo.revealed = false;
            s.commit_layout(&geo);
        }
        s.video_dims.store(0, Ordering::Relaxed);
        // Keys stop being intercepted now; their releases would go to WebKit.
        s.release_all_held();
    }

    /// Nothing to poll: the layout follows GTK's `size-allocate` synchronously on
    /// the UI thread (where the allocation actually changes), and the presenter
    /// resizes its swapchain from the same geometry on the next frame.
    fn fit(&self) {}

    fn client_size(&self) -> (u32, u32) {
        self.shared.lock_geo().physical()
    }

    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        if sink.is_none() {
            self.shared.release_all_held();
        }
        *lock(&self.shared.input_sink) = sink;
    }

    /// The spike does not accept drops (out of its scope; L5 adds a
    /// `wl_data_device` on our seat): GDK refuses drags over the sink window, so
    /// every drop is refused — the contract's `None` behaviour either way.
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        if sink.is_some() {
            tracing::info!("video surface: file drops are not supported by the spike surface");
        }
    }

    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Ok(Box::new(crate::presenter::WgpuPresenter::new(
            self.shared.clone(),
        )?))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
