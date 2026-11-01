//! The viewer's video surface on Linux (task L5): a native surface inside the
//! app window, above WebKitGTK — the AnyDesk layout (decision #3).
//!
//! * X11 ([`x11`]): a child X window of the GTK toplevel, created on our own
//!   x11rb connection; the hover-bar clip is an XShape region.
//! * Wayland ([`wayland`]): a `wl_subsurface` tree on the GTK toplevel's
//!   `wl_surface` (the L5-spike design, `plans/spikes/L5-wayland-subsurface.md`),
//!   or — when the compositor lacks `wp_viewporter`, or the user asks for it —
//!   the decision-#3 fallback: the same tree in a separate undecorated window.
//! * Both draw with one wgpu presenter ([`presenter`]: Vulkan, GL fallback) over
//!   the renderer shared with macOS (`surface-wgpu`), and take keys from the
//!   GTK toplevel ([`gtk_host`]) so WebKit never sees them during a session.
//!
//! ## References
//! * Contract: `platform_api::{VideoSurface, Presenter}` (trait docs).
//! * Plan: `plans/04-TARGET-ARCHITECTURE.md` §4, `plans/02-LINUX-FINDINGS.md` §6,
//!   `plans/05-TASKS.md` (L5), `plans/spikes/L5-wayland-subsurface.md`.
//! * raw-window-handle 0.6 (what tao hands out: `XlibWindowHandle` = the GTK
//!   toplevel's XID, `WaylandWindowHandle` = its `wl_surface`):
//!   <https://docs.rs/raw-window-handle/0.6/raw_window_handle/>

mod dnd;
mod gtk_host;
mod keys;
mod presenter;
mod wayland;
mod x11;
mod xdnd;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use platform_api::{RawDisplayHandle, RawWindowHandle, SurfaceError, SurfaceHandles, VideoSurface};

/// `Platform::create_surface` for Linux. Call on the GTK main thread, after
/// the app window is shown (its native window must exist).
pub(crate) fn create(
    window: RawWindowHandle,
    display: RawDisplayHandle,
) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
    match window {
        RawWindowHandle::Xlib(w) => {
            let xid = u32::try_from(w.window).map_err(|_| {
                SurfaceError::Backend(format!("X11 window id {:#x} is out of range", w.window))
            })?;
            Ok(x11::X11Surface::create(xid, window, display)?)
        }
        RawWindowHandle::Xcb(w) => Ok(x11::X11Surface::create(w.window.get(), window, display)?),
        RawWindowHandle::Wayland(w) => wayland::create(w.surface, display),
        _ => Err(SurfaceError::UnsupportedHandle),
    }
}

/// Native handles a surface keeps for `VideoSurface::native_handles`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeptHandles(pub(crate) SurfaceHandles);

// SAFETY: the handles are only copied out to the caller, never dereferenced
// here. They name the app window, our own video window/surface and the
// display connection, all of which outlive the surface that keeps them
// (the same lifetime `SurfaceHandles` documents).
unsafe impl Send for KeptHandles {}
// SAFETY: as above; plain copies of pointers and ids.
unsafe impl Sync for KeptHandles {}

/// Lock a mutex whose data stays consistent across a panic. Every guarded
/// value here is plain data or protocol handles updated in single steps, and
/// refusing input or presents after an unrelated panic would only add a
/// frozen picture (or a stuck key on the host) to the failure.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
