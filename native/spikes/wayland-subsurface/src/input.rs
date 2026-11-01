//! Pointer input on the video subsurface, dispatched on a dedicated thread from
//! our own event queue on GDK's connection.
//!
//! We bind our own `wl_seat` and `wl_pointer`. Compositors send seat events to
//! every `wl_pointer` a client created for that seat, so GDK's pointer and ours
//! both see them; ours keeps only events whose focus is the video surface, and
//! GDK routes those to the widget-less sink window, which GTK drops (see
//! `wl::create_surface_with_user_data`). The keyboard never focuses a
//! subsurface ("A sub-surface never has the keyboard focus of any seat"), so
//! keys are taken from the GTK toplevel instead (`gtk_side`).
//!
//! ## References
//! * wl_seat / wl_pointer (enter/leave/motion/button/axis/frame, value120 v8):
//!   <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * wayland-client `Dispatch`, `EventQueue::blocking_dispatch` (prepare_read →
//!   poll → read → dispatch): <https://docs.rs/wayland-client/0.31/wayland_client/struct.EventQueue.html>
//! * wp_fractional_scale_v1.preferred_scale:
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/fractional-scale/fractional-scale-v1.xml>

use std::sync::Arc;

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use platform_api::VideoInput;

use crate::geometry::{dom_button, WheelAccumulator};
use crate::surface::Shared;

/// State owned by the input thread.
pub struct InputState {
    shared: Option<Arc<Shared>>,
    pointer: Option<WlPointer>,
    /// The pointer's focus is our video surface.
    inside: bool,
    wheel: WheelAccumulator,
    /// The pointer sends `frame` events (wl_seat v5+); otherwise every axis
    /// event is a frame of its own.
    framed: bool,
}

impl InputState {
    /// The state starts detached: globals are bound before the shared surface
    /// exists, and nothing is dispatched until [`spawn`] attaches it.
    pub fn detached() -> Self {
        Self {
            shared: None,
            pointer: None,
            inside: false,
            wheel: WheelAccumulator::default(),
            framed: false,
        }
    }

    fn is_video(&self, surface: &WlSurface) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|s| s.wl.video.id() == surface.id())
    }
}

/// Start the input thread. It runs for the process lifetime: the surface is
/// never destroyed, and the loop ends only if GDK's connection dies (at which
/// point GDK itself exits the app).
pub fn spawn(
    shared: Arc<Shared>,
    mut queue: EventQueue<InputState>,
    seat: &WlSeat,
) -> Result<(), std::io::Error> {
    let framed = seat.version() >= 5;
    std::thread::Builder::new()
        .name("wl-video-input".into())
        .spawn(move || {
            let mut state = InputState {
                shared: Some(shared),
                framed,
                ..InputState::detached()
            };
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    tracing::error!("video input: Wayland dispatch stopped: {e}");
                    return;
                }
            }
        })
        .map(|_| ())
}

impl Dispatch<WlRegistry, GlobalListContents> for InputState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals come and go (outputs, hot-plugged seats); the objects the video
        // needs were bound at creation and a removed seat simply stops sending.
    }
}

impl Dispatch<WlSeat, ()> for InputState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let has_pointer =
            matches!(capabilities, WEnum::Value(c) if c.contains(wl_seat::Capability::Pointer));
        match (has_pointer, state.pointer.take()) {
            (true, None) => state.pointer = Some(seat.get_pointer(qh, ())),
            (true, Some(p)) => state.pointer = Some(p),
            (false, Some(p)) => {
                if p.version() >= 3 {
                    p.release();
                }
                state.inside = false;
            }
            (false, None) => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for InputState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.inside = state.is_video(&surface);
                if state.inside {
                    if let Some(s) = &state.shared {
                        s.pointer_moved(surface_x, surface_y);
                    }
                }
            }
            // Leaving for the revealed bar or another window. No button can be
            // held across a leave (the implicit grab keeps the focus), so there
            // is nothing to release here.
            wl_pointer::Event::Leave { surface, .. } => {
                if state.is_video(&surface) {
                    state.inside = false;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let (true, Some(s)) = (state.inside, &state.shared) {
                    s.pointer_moved(surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(b),
                ..
            } => {
                // An implicit grab keeps the focus on the surface the press
                // happened on, so a drag that leaves the video still delivers
                // its release here.
                if let (true, Some(s), Some(button)) =
                    (state.inside, &state.shared, dom_button(button))
                {
                    s.pointer_button(button, b == wl_pointer::ButtonState::Pressed);
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                state
                    .wheel
                    .continuous(axis == wl_pointer::Axis::HorizontalScroll, value);
                if !state.framed {
                    state.flush_wheel();
                }
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => state
                .wheel
                .detents(axis == wl_pointer::Axis::HorizontalScroll, discrete * 120),
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => state
                .wheel
                .detents(axis == wl_pointer::Axis::HorizontalScroll, value120),
            wl_pointer::Event::Frame => state.flush_wheel(),
            _ => {}
        }
    }
}

impl InputState {
    fn flush_wheel(&mut self) {
        let wheel = self.wheel.flush();
        if let (true, Some(s), Some((dx, dy))) = (self.inside, &self.shared, wheel) {
            s.emit(VideoInput::Wheel { dx, dy });
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for InputState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (wp_fractional_scale_v1::Event::PreferredScale { scale }, Some(s)) =
            (event, &state.shared)
        {
            s.set_fractional_scale(scale);
        }
    }
}

// Objects whose events we do not need. wl_surface (area): enter/leave output
// and preferred_buffer_scale — the scale comes from fractional-scale or GDK.
// wl_buffer: release — the transparent pixel is attached again as is, which
// wl_surface.attach allows for shm buffers. wl_shm: format list — ARGB8888 is
// mandatory for every compositor.
delegate_noop!(InputState: ignore WlSurface);
delegate_noop!(InputState: ignore WlBuffer);
delegate_noop!(InputState: ignore WlShm);
delegate_noop!(InputState: WlCompositor);
delegate_noop!(InputState: WlSubcompositor);
delegate_noop!(InputState: WlSubsurface);
delegate_noop!(InputState: WlRegion);
delegate_noop!(InputState: WlShmPool);
delegate_noop!(InputState: WpViewporter);
delegate_noop!(InputState: WpViewport);
delegate_noop!(InputState: WpFractionalScaleManagerV1);
