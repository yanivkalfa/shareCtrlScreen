//! L5-spike: a wgpu-rendered `wl_subsurface` above WebKitGTK inside a tao/wry
//! (Tauri 2) window, implementing the `platform-api` `VideoSurface` and
//! `Presenter` contracts unchanged. Result and measurements:
//! `plans/spikes/L5-wayland-subsurface.md`.
//!
//! ## References
//! * Plan: `plans/04-TARGET-ARCHITECTURE.md` §4, `plans/02-LINUX-FINDINGS.md` §6,
//!   `plans/05-TASKS.md` (L5-spike, L5).
//! * Prior art: GStreamer gtkwaylandsink / GstWlWindow —
//!   <https://gitlab.freedesktop.org/gstreamer/gstreamer/-/tree/main/subprojects/gst-plugins-bad/ext/gtk>
//! * Protocols: `wayland.xml` (wl_subsurface, wl_pointer), wp_viewporter,
//!   wp_fractional_scale_v1 — <https://wayland.app/protocols/>
//! * GTK3 Wayland backend: <https://gitlab.gnome.org/GNOME/gtk/-/tree/gtk-3-24/gdk/wayland>
//! * wgpu raw-window-handle surfaces: <https://docs.rs/wgpu/30/wgpu/enum.SurfaceTargetUnsafe.html>

#![cfg(target_os = "linux")]

pub mod error;
pub mod geometry;
mod gtk_side;
mod input;
pub mod keys;
mod presenter;
mod surface;
mod wl;

pub use error::SpikeError;
pub use surface::WaylandSubsurface;
