//! `VideoSurface` on X11: a child window of the GTK toplevel, above WebKitGTK,
//! on our own x11rb connection — the X11 counterpart of the Windows child
//! HWND (`render/src/window.rs`).
//!
//! * **Placement**: the child covers the toplevel's content widget (below
//!   the header bar), following GTK's `size-allocate`; GTK logical px times
//!   GDK's window scale are X pixels.
//! * **Pointer** events are selected on the child, so the X server delivers
//!   them to our connection and dispatches them on our own thread; the
//!   implicit grab of a press keeps motion and the release coming to us even
//!   when a drag leaves the window. **Keys are not selected**: they propagate
//!   to the GTK toplevel, whose handler (`gtk_host`) forwards them, so there is
//!   one key path for X11 and Wayland and WebKit never sees them.
//! * **Hover-bar reveal**: an XShape bounding + input region that leaves out
//!   the top strip, so the web toolbar underneath shows and takes clicks. The
//!   window keeps its size and position, so the picture does not move and
//!   `client_size` does not change (the `SetWindowRgn` behaviour).
//! * **File drops**: XDND targets only top-level windows, so while the video
//!   is shown with a drop sink the app window's `XdndProxy` points at the
//!   child and the drag protocol runs on our connection (`xdnd`).
//! * **Contents**: X windows lose their pixels when unmapped or (without a
//!   compositor) covered; `Expose` asks the presenter to draw the last frame
//!   again, since a static remote screen sends no new one.
//! * **Scale**: GDK's integer window scale (`GDK_SCALE`,
//!   `Gdk/WindowScalingFactor`). WHY not `Xft.dpi`: on X11 it scales only
//!   text, while CSS px — and so the 40 px web toolbar — follow GDK's window
//!   scale; reading the DPI would misplace the reveal strip.
//!
//! ## References
//! * X11 protocol (CreateWindow, event propagation, `do-not-propagate`,
//!   implicit pointer grabs, Expose, UnmapWindow):
//!   <https://www.x.org/releases/current/doc/xproto/x11protocol.html>
//! * X Nonrectangular Window Shape Extension 1.1 (bounding and input kinds):
//!   <https://www.x.org/releases/current/doc/xextproto/shape.html>
//! * Core wheel buttons 4–7 (one notch per press, as the X server emulates
//!   them for smooth-scrolling devices): XInput 2.1 spec, "Smooth scrolling",
//!   <https://www.x.org/releases/current/doc/inputproto/XI2proto.txt>
//! * X cursor font glyphs (`XC_left_ptr` = 68): `X11/cursorfont.h`.
//! * Transparent cursor to hide the pointer over the video while controlling
//!   (task D10): an empty (all-zero mask) 1x1 pixmap passed to `CreateCursor`,
//!   swapped onto the window's `cursor` attribute — a per-window attribute, so
//!   it only hides the pointer over our window and needs no enter/leave
//!   balancing: <https://www.x.org/releases/current/doc/xproto/x11protocol.html>
//!   (CreateCursor, ChangeWindowAttributes). The cross-platform analogue is
//!   winit/tao `set_cursor_visible`:
//!   <https://docs.rs/winit/latest/winit/window/struct.Window.html#method.set_cursor_visible>.
//!   Drawing the sprite at the local pointer is client-side cursor prediction
//!   (AnyDesk / Microsoft RDP), chosen in the engine's `media/viewer.rs`.
//! * x11rb 0.14: <https://docs.rs/x11rb/0.14.0/x11rb/>
//! * `gdk_x11_window_get_xid`:
//!   <https://docs.gtk.org/gdk3-x11/>

use std::any::Any;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

use gtk::prelude::*;
use platform_api::{
    FileDrop, Presenter, RawDisplayHandle, RawWindowHandle, SurfaceError, SurfaceHandles,
    VideoInput, VideoSurface,
};
use raw_window_handle::{XlibDisplayHandle, XlibWindowHandle};
use surface_wgpu::reveal::{bar_px, Reveal};
use surface_wgpu::SurfaceState;
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, ChangeWindowAttributesAux, ClientMessageEvent, ClipOrdering, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor, EventMask, NotifyMode, Rectangle,
    StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

use super::gtk_host::{self, HostEvents, HostLink};
use super::presenter::{Repaint, Target, WgpuPresenter};
use super::xdnd::Xdnd;
use super::{lock, KeptHandles};

/// Windows wheel units per core wheel button press (one notch).
const UNITS_PER_NOTCH: i32 = 120;
/// `XC_left_ptr` in the X cursor font (`X11/cursorfont.h`); its mask is the
/// next glyph.
const XC_LEFT_PTR: u16 = 68;

/// What a core pointer button means for the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreButton {
    /// DOM `MouseEvent.button`.
    Press(u8),
    /// Wheel notch in Windows units (`dx`, `dy`); positive `dy` = away from
    /// the user.
    Wheel(i32, i32),
}

/// Core button number → meaning. 1/2/3 are left/middle/right; 4–7 are the
/// wheel (up, down, left, right); 8/9 (back/forward) are not in the protocol
/// and are dropped, as on Windows.
fn core_button(detail: u8) -> Option<CoreButton> {
    match detail {
        1 => Some(CoreButton::Press(0)),
        2 => Some(CoreButton::Press(1)),
        3 => Some(CoreButton::Press(2)),
        4 => Some(CoreButton::Wheel(0, UNITS_PER_NOTCH)),
        5 => Some(CoreButton::Wheel(0, -UNITS_PER_NOTCH)),
        6 => Some(CoreButton::Wheel(-UNITS_PER_NOTCH, 0)),
        7 => Some(CoreButton::Wheel(UNITS_PER_NOTCH, 0)),
        _ => None,
    }
}

/// The child window's geometry in X pixels for a content widget at `origin`
/// with `size` (GTK logical px) under GDK scale `scale`. X geometry is 16-bit,
/// so everything is clamped (an X window can be at most 32767 px wide).
fn child_geometry(origin: (i32, i32), size: (u32, u32), scale: i32) -> (i16, i16, u16, u16) {
    let scale = scale.max(1);
    let pos = |v: i32| {
        v.saturating_mul(scale)
            .clamp(i16::MIN.into(), i16::MAX.into()) as i16
    };
    let len = |v: u32| {
        u64::from(v)
            .saturating_mul(scale.unsigned_abs().into())
            .clamp(1, i16::MAX as u64) as u16
    };
    (pos(origin.0), pos(origin.1), len(size.0), len(size.1))
}

/// The visible (and hit-testable) part of a `size` window while the bar is
/// revealed: everything below the bar. Never empty — an empty bounding shape
/// would hide the window, not reveal the bar.
fn revealed_rect(size: (u16, u16), scale: f64) -> Rectangle {
    let (w, h) = size;
    let bar = (bar_px(scale).round() as u32).min(u32::from(h.saturating_sub(1))) as u16;
    Rectangle {
        x: 0,
        y: bar as i16,
        width: w,
        height: h - bar,
    }
}

/// The opaque black pixel for a window of `depth`: alpha set on a 32-bit
/// (ARGB) visual, the screen's black pixel otherwise.
fn opaque_black(depth: u8, screen_black: u32) -> u32 {
    if depth == 32 {
        0xff00_0000
    } else {
        screen_black
    }
}

/// Visibility, serialized against presents: once `hide` returns no frame
/// reaches the window (the UI paints a modal right after).
struct Gate {
    shown: bool,
}

/// State shared by the GTK thread (layout, keys), the event thread (pointer),
/// the render thread (presents) and every `VideoSurface` caller.
pub(crate) struct Shared {
    conn: RustConnection,
    window: Window,
    xdnd: Xdnd,
    /// ClientMessage type that wakes the event thread for shutdown.
    wake: Atom,
    /// Xlib display handed to wgpu (Vulkan's xlib surface, EGL's X11 platform).
    xdisplay: NonNull<c_void>,
    screen: i32,
    state: SurfaceState,
    gate: Mutex<Gate>,
    /// Lock-free copy of `gate.shown` for the key handler and the presenter's
    /// cheap check; the gate is authoritative.
    shown: AtomicBool,
    /// Current window size in X px (`0` until the first layout).
    geometry: Mutex<(u16, u16)>,
    reveal: Mutex<Reveal>,
    repaint: Mutex<Option<Weak<dyn Repaint>>>,
    stop: AtomicBool,
    /// Task D10: the arrow (view-only / host-initiated) and a transparent
    /// cursor (hidden while controlling so only the sprite shows). Both are
    /// kept alive for the window's lifetime so the attribute can swap between
    /// them; freed in `Drop`.
    cursor_arrow: Cursor,
    cursor_blank: Cursor,
}

// SAFETY: `xdisplay` is the Xlib `Display*` from the window handle — tao's
// own connection, opened for that handle and never closed, used by nobody but
// the presenter (see `xlib_display`; GDK's display is refused). It is never
// dereferenced here: only wgpu (Vulkan WSI / EGL) uses it, from the media
// thread and the X11 event thread's repaints, which the presenter serializes
// under its own mutex. Every other field is Send + Sync.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

fn x_err(what: &str) -> impl Fn(x11rb::errors::ConnectionError) -> SurfaceError + '_ {
    move |e| SurfaceError::Backend(format!("X11 {what} failed: {e}"))
}

/// A fully transparent cursor (task D10): a 1×1 depth-1 pixmap cleared to 0,
/// used as both source and mask, so the X server draws nothing while the
/// pointer is over our window. Swapped in for the arrow while this viewer
/// controls, so only the client-side sprite shows; swapped back on release.
/// An X11 window's cursor is a per-window attribute, so it can never leak to
/// another window and needs no enter/leave bookkeeping to stay balanced.
fn create_blank_cursor(conn: &RustConnection, drawable: Window) -> Result<Cursor, SurfaceError> {
    let id = |e: x11rb::errors::ReplyOrIdError| {
        SurfaceError::Backend(format!("X11 id allocation failed: {e}"))
    };
    let pixmap = conn.generate_id().map_err(id)?;
    conn.create_pixmap(1, pixmap, drawable, 1, 1)
        .map_err(x_err("CreatePixmap"))?;
    // CreatePixmap leaves the bits undefined; clear them so the mask is all 0.
    let gc = conn.generate_id().map_err(id)?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0))
        .map_err(x_err("CreateGC"))?;
    conn.poly_fill_rectangle(
        pixmap,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
    )
    .map_err(x_err("PolyFillRectangle"))?;
    conn.free_gc(gc).map_err(x_err("FreeGC"))?;
    let cursor = conn.generate_id().map_err(id)?;
    // Source and mask both the empty pixmap: mask 0 ⇒ nothing is drawn.
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)
        .map_err(x_err("CreateCursor"))?;
    conn.free_pixmap(pixmap).map_err(x_err("FreePixmap"))?;
    Ok(cursor)
}

impl Shared {
    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            tracing::warn!("video surface: X11 flush failed: {e}");
        }
    }

    /// Apply the bounding + input shape for the current reveal state.
    fn apply_shape(&self, revealed: bool) {
        let size = *lock(&self.geometry);
        let result = if revealed && size.1 > 1 {
            let rect = [revealed_rect(size, self.state.scale())];
            self.conn
                .shape_rectangles(
                    shape::SO::SET,
                    shape::SK::BOUNDING,
                    ClipOrdering::YX_BANDED,
                    self.window,
                    0,
                    0,
                    &rect,
                )
                .and_then(|_| {
                    self.conn.shape_rectangles(
                        shape::SO::SET,
                        shape::SK::INPUT,
                        ClipOrdering::YX_BANDED,
                        self.window,
                        0,
                        0,
                        &rect,
                    )
                })
                .map(|_| ())
        } else {
            // No mask = the default shape: the whole window, at any size.
            self.conn
                .shape_mask(shape::SO::SET, shape::SK::BOUNDING, self.window, 0, 0, NONE)
                .and_then(|_| {
                    self.conn
                        .shape_mask(shape::SO::SET, shape::SK::INPUT, self.window, 0, 0, NONE)
                })
                .map(|_| ())
        };
        if let Err(e) = result {
            tracing::warn!("video surface: X11 shape failed: {e}");
        }
        self.flush();
    }

    fn reset_reveal(&self) {
        // Task M15: the top-edge reveal is retired on Linux, so this now just
        // asserts the window's full (default) bounding + input region on every
        // show/hide. `apply_shape(false)` sets the NULL mask, which is the full
        // window and idempotent — nothing ever cuts the top strip any more.
        lock(&self.reveal).reset();
        self.apply_shape(false);
    }

    fn pointer(&self, x: i16, y: i16) {
        let (px, py) = (f64::from(x), f64::from(y));
        // Task M15: the top-edge hover reveal is retired on Linux (the native
        // menu replaces it), so pointer motion no longer drives it.
        let (nx, ny) = self.state.normalize(px, py);
        self.state.input().moved(nx, ny);
        // Task D10: the pointer is over the video — predict the sprite here so
        // it tracks the real mouse instead of the host's delayed echo.
        self.state.set_local_pointer(nx, ny);
        self.state.set_pointer_inside(true);
    }

    /// Task D10: swap the window's cursor attribute. A per-window attribute, so
    /// the transparent cursor only hides the pointer while it is over our video
    /// and can never leak to another window — no enter/leave balancing needed.
    fn set_os_cursor_hidden(&self, hidden: bool) {
        let cursor = if hidden {
            self.cursor_blank
        } else {
            self.cursor_arrow
        };
        if let Err(e) = self.conn.change_window_attributes(
            self.window,
            &ChangeWindowAttributesAux::new().cursor(cursor),
        ) {
            tracing::warn!("video surface: X11 cursor swap failed: {e}");
            return;
        }
        self.flush();
    }

    fn button(&self, detail: u8, down: bool, x: i16, y: i16) {
        match core_button(detail) {
            Some(CoreButton::Press(button)) => {
                let (nx, ny) = self.state.normalize(f64::from(x), f64::from(y));
                self.state.input().button(button, down, nx, ny);
            }
            // Each notch is one press (+ release, ignored).
            Some(CoreButton::Wheel(dx, dy)) if down => self.state.input().wheel(dx, dy),
            _ => {}
        }
    }

    /// Drops go to the video exactly while it is shown with a drop sink.
    fn update_drops(&self) {
        let on = self.shown.load(Ordering::SeqCst) && self.state.accepts_drops();
        self.xdnd.set_active(&self.conn, on);
    }

    fn repaint(&self) {
        let hook = lock(&self.repaint).as_ref().and_then(Weak::upgrade);
        if let Some(hook) = hook {
            hook.repaint();
        }
    }

    /// Dispatch our connection's events until told to stop.
    fn run_events(&self) {
        loop {
            let event = match self.conn.wait_for_event() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("video surface: X11 connection lost: {e}");
                    return;
                }
            };
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let inset = lock(&self.reveal).inset_px(self.state.scale());
            if self.xdnd.handle(&self.conn, &event, &self.state, inset) {
                continue;
            }
            match event {
                Event::MotionNotify(e) => self.pointer(e.event_x, e.event_y),
                Event::ButtonPress(e) => self.button(e.detail, true, e.event_x, e.event_y),
                Event::ButtonRelease(e) => self.button(e.detail, false, e.event_x, e.event_y),
                // Grab/ungrab crossings are not real pointer moves.
                Event::LeaveNotify(e) if e.mode == NotifyMode::NORMAL => {
                    // Task D10: the pointer left the video — stop predicting the
                    // sprite (it falls back to the host's out-of-band position).
                    // M15: no reveal to drive on a top-edge exit any more.
                    self.state.set_pointer_inside(false);
                }
                // The last of a series: draw once for the whole exposure.
                Event::Expose(e) if e.count == 0 => self.repaint(),
                Event::Error(e) => tracing::warn!("video surface: X11 error {e:?}"),
                _ => {}
            }
        }
    }
}

impl HostEvents for Shared {
    fn state(&self) -> &SurfaceState {
        &self.state
    }

    fn wants_keys(&self) -> bool {
        self.shown.load(Ordering::SeqCst) && self.state.input().has_sink()
    }

    fn relayout(&self, _toplevel: &gtk::Window, origin: (i32, i32), size: (u32, u32), scale: i32) {
        let (x, y, w, h) = child_geometry(origin, size, scale);
        let changed = {
            let mut geometry = lock(&self.geometry);
            let changed = *geometry != (w, h);
            *geometry = (w, h);
            changed
        };
        self.state.set_scale(f64::from(scale.max(1)));
        self.state.set_size(u32::from(w), u32::from(h));
        let aux = ConfigureWindowAux::new()
            .x(i32::from(x))
            .y(i32::from(y))
            .width(u32::from(w))
            .height(u32::from(h));
        if let Err(e) = self.conn.configure_window(self.window, &aux) {
            tracing::warn!("video surface: X11 configure failed: {e}");
        }
        // A rectangle shape does not grow with the window: redo it.
        if changed && lock(&self.reveal).is_revealed() {
            self.apply_shape(true);
        }
        self.flush();
        tracing::debug!("video surface: layout {x},{y} {w}x{h} (scale {scale})");
    }
}

impl Target for Shared {
    fn handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        (
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(self.xdisplay), self.screen)),
            RawWindowHandle::Xlib(XlibWindowHandle::new(self.window.into())),
        )
    }

    fn state(&self) -> &SurfaceState {
        &self.state
    }

    fn is_shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }

    fn commit(&self, _size: (u32, u32), present: &mut dyn FnMut()) -> bool {
        let gate = lock(&self.gate);
        if gate.shown {
            present();
        }
        gate.shown
    }

    fn set_repaint(&self, hook: Weak<dyn Repaint>) {
        *lock(&self.repaint) = Some(hook);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The presenter (which draws into the window) is gone by now: it holds
        // an `Arc` to this.
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

/// The X11 video surface.
pub(crate) struct X11Surface {
    shared: Arc<Shared>,
    events: Option<JoinHandle<()>>,
    host: Option<HostLink>,
    /// The child window, the app window handle `create_surface` received and
    /// the Xlib display wgpu also uses.
    handles: KeptHandles,
}

/// The Xlib display wgpu draws through: the one in the handle. tao opens a
/// fresh `Display` for every display handle it hands out and never closes
/// it, so nothing else in the process uses it.
///
/// WHY not fall back to GDK's own display: GDK's main thread uses it all the
/// time, while Vulkan WSI / EGL would use it from the media and X11 event
/// threads. libX11 before 1.8 (Ubuntu 22.04) is not thread-safe unless
/// `XInitThreads` ran first, and Mesa's xcb traffic on an Xlib-owned socket
/// would then corrupt GDK's connection.
fn xlib_display(display: RawDisplayHandle) -> Result<(NonNull<c_void>, i32), SurfaceError> {
    match display {
        RawDisplayHandle::Xlib(XlibDisplayHandle {
            display: Some(d),
            screen,
            ..
        }) => Ok((d, screen)),
        _ => Err(SurfaceError::Backend(
            "the X11 window handle carries no Xlib display of its own; create the video \
             surface from the app window's display handle (Tauri/tao provide one)"
                .into(),
        )),
    }
}

impl X11Surface {
    /// Create the hidden child window under the GTK toplevel `parent` (the
    /// XID of `parent_handle`). Call on the GTK main thread.
    pub(crate) fn create(
        parent: Window,
        parent_handle: RawWindowHandle,
        display: RawDisplayHandle,
    ) -> Result<Arc<Self>, SurfaceError> {
        let top = gtk_host::find_toplevel(|w| {
            w.downcast_ref::<gdkx11::X11Window>()
                .is_some_and(|x| x.xid() == u64::from(parent))
        })?;
        let (xdisplay, screen_num) = xlib_display(display)?;
        let name = gtk::gdk::Display::default()
            .map(|d| d.name().to_string())
            .ok_or_else(|| SurfaceError::Backend("GTK has no display".into()))?;
        // Our own connection to GTK's X server: events for our window arrive
        // here, never in GDK's queue, and are read on our own thread.
        let (conn, screen) = crate::x11_connect(&name).map_err(|e| {
            SurfaceError::Backend(format!("cannot connect to the X server {name}: {e}"))
        })?;
        let version = conn
            .shape_query_version()
            .map_err(x_err("SHAPE query"))?
            .reply()
            .map_err(|e| {
                SurfaceError::Backend(format!(
                    "the X server has no SHAPE extension, which the session toolbar needs: {e}"
                ))
            })?;
        // Input shapes (SK::INPUT) are SHAPE 1.1.
        if (version.major_version, version.minor_version) < (1, 1) {
            return Err(SurfaceError::Backend(format!(
                "the X server's SHAPE extension is {}.{}; the session toolbar needs 1.1",
                version.major_version, version.minor_version
            )));
        }
        // The child copies the toplevel's depth and visual. GTK gives CSD
        // toplevels a 32-bit ARGB visual under a compositor, where pixel 0
        // (the root's "black") is fully transparent: the desktop would show
        // through before the first frame and after every Expose.
        let parent_depth = conn
            .get_geometry(parent)
            .map_err(x_err("GetGeometry"))?
            .reply()
            .map_err(|e| {
                SurfaceError::Backend(format!("the app window {parent:#x} is not usable: {e}"))
            })?
            .depth;
        let black = opaque_black(parent_depth, conn.setup().roots[screen].black_pixel);
        let id = |e: x11rb::errors::ReplyOrIdError| {
            SurfaceError::Backend(format!("X11 id allocation failed: {e}"))
        };
        let window = conn.generate_id().map_err(id)?;
        let font = conn.generate_id().map_err(id)?;
        let cursor = conn.generate_id().map_err(id)?;
        // The arrow, not whatever text/link cursor GTK last showed for the
        // page under us (a window without a cursor inherits its parent's).
        conn.open_font(font, b"cursor").map_err(x_err("OpenFont"))?;
        conn.create_glyph_cursor(
            cursor,
            font,
            font,
            XC_LEFT_PTR,
            XC_LEFT_PTR + 1,
            0,
            0,
            0,
            0xffff,
            0xffff,
            0xffff,
        )
        .map_err(x_err("CreateGlyphCursor"))?;
        conn.close_font(font).map_err(x_err("CloseFont"))?;
        let aux = CreateWindowAux::new()
            // Black until the first frame (the letterbox colour), instead of a
            // frozen copy of the page underneath.
            .background_pixel(black)
            .cursor(cursor)
            // No key events: they must propagate to the GTK toplevel (see the
            // module doc).
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::POINTER_MOTION
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::LEAVE_WINDOW,
            );
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            parent,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &aux,
        )
        .map_err(x_err("CreateWindow"))?
        .check()
        .map_err(|e| {
            SurfaceError::Backend(format!(
                "cannot create the video window in the app window: {e}"
            ))
        })?;
        // Task D10: keep the arrow cursor alive (not freed) so the window's
        // cursor attribute can swap back to it after hiding it; make the
        // transparent companion now. Both are freed in `Drop`.
        let cursor_blank = create_blank_cursor(&conn, window)?;
        let wake = conn
            .intern_atom(false, b"_SHARECTRL_VIDEO_WAKE")
            .map_err(x_err("InternAtom"))?
            .reply()
            .map_err(|e| SurfaceError::Backend(format!("X11 InternAtom failed: {e}")))?
            .atom;
        let root = conn.setup().roots[screen].root;
        let xdnd = Xdnd::new(&conn, window, parent, root)?;
        conn.flush().map_err(x_err("flush"))?;

        let shared = Arc::new(Shared {
            conn,
            window,
            xdnd,
            wake,
            xdisplay,
            screen: screen_num,
            state: SurfaceState::new(),
            gate: Mutex::new(Gate { shown: false }),
            shown: AtomicBool::new(false),
            geometry: Mutex::new((0, 0)),
            reveal: Mutex::new(Reveal::default()),
            repaint: Mutex::new(None),
            stop: AtomicBool::new(false),
            cursor_arrow: cursor,
            cursor_blank,
        });
        let events = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("x11-video-input".into())
                .spawn(move || shared.run_events())
                .map_err(|e| {
                    SurfaceError::Backend(format!("cannot start the video input thread: {e}"))
                })?
        };
        let host = gtk_host::connect(&top, shared.clone(), false);
        let (w, h) = shared.state.size();
        tracing::info!("video surface: X11 child window {window:#x} created ({w}x{h} px)");
        let (display, video) = shared.handles();
        Ok(Arc::new(Self {
            shared,
            events: Some(events),
            host: Some(host),
            handles: KeptHandles(SurfaceHandles {
                video,
                parent: parent_handle,
                display,
            }),
        }))
    }
}

impl VideoSurface for X11Surface {
    fn show(&self) {
        let s = &self.shared;
        s.reset_reveal();
        let mut gate = lock(&s.gate);
        // On top of any sibling created since (a re-created web view), or we
        // would present, perfectly and invisibly, under the page.
        let raised = s
            .conn
            .configure_window(
                s.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )
            .and_then(|_| s.conn.map_window(s.window));
        if let Err(e) = raised {
            tracing::warn!("video surface: X11 map failed: {e}");
        }
        gate.shown = true;
        s.shown.store(true, Ordering::SeqCst);
        drop(gate);
        s.update_drops();
        // The map's Expose draws the last frame again (the presenter keeps it).
        s.flush();
    }

    /// Unmaps the window (never just shapes or moves it away): the shortcut
    /// hook's keyboard grab on it ends only when the server sees it
    /// unviewable, which keeps a modal's typing out of the remote session.
    fn hide(&self) {
        let s = &self.shared;
        {
            let mut gate = lock(&s.gate);
            gate.shown = false;
            s.shown.store(false, Ordering::SeqCst);
            if let Err(e) = s.conn.unmap_window(s.window) {
                tracing::warn!("video surface: X11 unmap failed: {e}");
            }
            // A round trip: when it returns, the server has processed the
            // unmap, so the modal GTK paints next is never drawn under us.
            if let Err(e) = s.conn.sync() {
                tracing::warn!("video surface: X11 sync failed: {e}");
            }
        }
        // Drops on the app window are GTK's again (the Settings dialog, the
        // page's own drop zones).
        s.update_drops();
        s.reset_reveal();
        s.state.clear_video_size();
        // Keys stop being intercepted now; their releases would go to WebKit.
        s.state.input().release_all();
    }

    /// Nothing to poll: the layout follows GTK's `size-allocate` on the UI
    /// thread (where the allocation actually changes), and the presenter
    /// resizes its swapchain from the same size on the next frame.
    fn fit(&self) {}

    fn client_size(&self) -> (u32, u32) {
        self.shared.state.size()
    }

    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        self.shared.state.input().set_sink(sink);
    }

    /// Drops reach the video through `XdndProxy` on the app window while it
    /// is shown with a sink (see `xdnd`); `None` gives them back to GTK.
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        self.shared.state.set_drop_sink(sink);
        self.shared.update_drops();
    }

    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Ok(Box::new(WgpuPresenter::new(self.shared.clone())?))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    /// The child window, the app window and the Xlib display wgpu also uses;
    /// all live as long as this surface.
    fn native_handles(&self) -> Option<SurfaceHandles> {
        Some(self.handles.0)
    }

    fn local_pointer(&self) -> Option<(f64, f64)> {
        self.shared.state.local_pointer()
    }

    fn set_cursor_control(&self, active: bool) {
        // X11's cursor is a window attribute shown only over our video, so hide
        // it whenever controlling; the sprite is drawn at the local pointer so
        // there is no second cursor. Restored to the arrow when control ends.
        self.shared.set_os_cursor_hidden(active);
    }
}

impl Drop for X11Surface {
    fn drop(&mut self) {
        // Release anything still held so the host is not left with a stuck key.
        self.shared.state.input().set_sink(None);
        self.shared.state.set_drop_sink(None);
        self.shared.xdnd.set_active(&self.shared.conn, false);
        // Task D10: release the two cursor resources (the window keeps showing
        // whichever is set until it is destroyed with its parent).
        let _ = self.shared.conn.free_cursor(self.shared.cursor_arrow);
        let _ = self.shared.conn.free_cursor(self.shared.cursor_blank);
        self.host.take();
        let s = &self.shared;
        s.stop.store(true, Ordering::SeqCst);
        // Wake the event thread out of its blocking wait with a message to our
        // own window; the stop flag is checked on every event.
        let wake = ClientMessageEvent::new(32, s.window, s.wake, [0u32; 5]);
        let _ = s
            .conn
            .send_event(false, s.window, EventMask::NO_EVENT, wake);
        s.flush();
        if let Some(t) = self.events.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_buttons_map_to_dom_buttons_and_wheel_notches() {
        assert_eq!(core_button(1), Some(CoreButton::Press(0)));
        assert_eq!(core_button(2), Some(CoreButton::Press(1)));
        assert_eq!(core_button(3), Some(CoreButton::Press(2)));
        // Wheel up = away from the user = positive dy, like WM_MOUSEWHEEL.
        assert_eq!(core_button(4), Some(CoreButton::Wheel(0, 120)));
        assert_eq!(core_button(5), Some(CoreButton::Wheel(0, -120)));
        assert_eq!(core_button(6), Some(CoreButton::Wheel(-120, 0)));
        assert_eq!(core_button(7), Some(CoreButton::Wheel(120, 0)));
        assert_eq!(core_button(8), None);
        assert_eq!(core_button(9), None);
        assert_eq!(core_button(0), None);
    }

    #[test]
    fn child_geometry_scales_and_clamps_to_x_limits() {
        assert_eq!(child_geometry((0, 46), (1280, 674), 1), (0, 46, 1280, 674));
        assert_eq!(
            child_geometry((12, 58), (1280, 674), 2),
            (24, 116, 2560, 1348)
        );
        // A zero scale is treated as 1; sizes are never 0 (BadValue).
        assert_eq!(child_geometry((5, 5), (0, 0), 0), (5, 5, 1, 1));
        assert_eq!(
            child_geometry((40_000, -40_000), (40_000, 10), 1),
            (i16::MAX, i16::MIN, i16::MAX as u16, 10)
        );
    }

    #[test]
    fn black_is_opaque_on_argb_visuals() {
        assert_eq!(opaque_black(32, 0), 0xff00_0000);
        assert_eq!(opaque_black(24, 0), 0);
    }

    #[test]
    fn revealed_shape_keeps_everything_below_the_bar() {
        let r = revealed_rect((1280, 720), 1.0);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 40, 1280, 680));
        assert_eq!(revealed_rect((2560, 1440), 2.0).y, 80);
        // Never the whole height: an empty shape would hide the window.
        let tiny = revealed_rect((100, 30), 1.0);
        assert_eq!((tiny.y, tiny.height), (29, 1));
    }
}
