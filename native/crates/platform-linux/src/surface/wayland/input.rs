//! Pointer input on the video subsurface, dispatched on a dedicated thread
//! from our own event queue on GDK's connection.
//!
//! We bind our own `wl_seat` and `wl_pointer`. Compositors send seat events to
//! every `wl_pointer` a client created for that seat, so GDK's pointer and
//! ours both see them; ours keeps only events whose focus is the video
//! surface, and GDK routes those to the widget-less sink window, which GTK
//! drops (see `wl::create_surface_with_user_data`). The keyboard never
//! focuses a subsurface ("A sub-surface never has the keyboard focus of any
//! seat"), so keys are taken from the GTK toplevel instead (`gtk_host`).
//!
//! ## References
//! * wl_seat / wl_pointer (enter/leave/motion/button/axis/frame, value120 v8),
//!   wl_display.sync: <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
//! * wayland-client `Dispatch`, `EventQueue::blocking_dispatch` (prepare_read →
//!   poll → read → dispatch): <https://docs.rs/wayland-client/0.31/wayland_client/struct.EventQueue.html>
//! * wp_fractional_scale_v1.preferred_scale:
//!   <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/fractional-scale/fractional-scale-v1.xml>

use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use platform_api::FileDrop;
use wayland_client::event_created_child;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::WlCallback;
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

use super::super::dnd::{parse_uri_list, URI_LIST};
use super::super::lock;
use super::geometry::{dom_button, AxisFrame};
use super::Shared;

/// MIME types a drag source offers, collected from `wl_data_offer.offer`
/// (sent before the `enter` that names the offer).
#[derive(Debug, Default)]
pub(crate) struct OfferTypes(Mutex<Vec<String>>);

/// The most a dropped file list may weigh: far beyond any real list, so a
/// hostile source cannot make us buffer an unbounded stream.
const MAX_LIST_BYTES: u64 = 4 << 20;

/// The drag currently over one of this client's surfaces.
struct Drag {
    offer: WlDataOffer,
    /// Last position, surface-local logical px.
    pos: (f64, f64),
    /// Over the video, offering a file list, with a drop sink installed.
    accepted: bool,
}

/// State owned by the input thread.
pub(crate) struct Dispatcher {
    shared: Option<Arc<Shared>>,
    pointer: Option<WlPointer>,
    drag: Option<Drag>,
    /// Our offer for a drop that landed on GTK's widgets.
    dropped_elsewhere: Option<WlDataOffer>,
    /// The pointer's focus is our video surface.
    inside: bool,
    /// Last pointer position on the video (surface-local logical px), for
    /// button events, which carry none.
    last: (f64, f64),
    wheel: AxisFrame,
    /// The pointer sends `frame` events (wl_seat v5+); otherwise every axis
    /// event is a frame of its own.
    framed: bool,
}

impl Dispatcher {
    fn is_video(&self, surface: &WlSurface) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|s| s.wl.video.id() == surface.id())
    }

    fn flush_wheel(&mut self) {
        let wheel = self.wheel.flush();
        if let (true, Some(s), Some((dx, dy))) = (self.inside, &self.shared, wheel) {
            s.state.input().wheel(dx, dy);
        }
    }
}

/// Start the input thread. It runs until the surface asks it to stop (a
/// `wl_display.sync` on this queue wakes it) or GDK's connection dies (at
/// which point GDK itself exits the app).
pub(crate) fn spawn(
    shared: Arc<Shared>,
    mut queue: EventQueue<Dispatcher>,
    seat: &WlSeat,
    data_device: Option<WlDataDevice>,
) -> std::io::Result<JoinHandle<()>> {
    let framed = seat.version() >= 5;
    std::thread::Builder::new()
        .name("wl-video-input".into())
        .spawn(move || {
            let stop = shared.clone();
            let mut state = Dispatcher {
                shared: Some(shared),
                pointer: None,
                drag: None,
                dropped_elsewhere: None,
                inside: false,
                last: (0.0, 0.0),
                wheel: AxisFrame::default(),
                framed,
            };
            while !stop.stop.load(Ordering::SeqCst) {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    tracing::error!("video input: Wayland dispatch stopped: {e}");
                    return;
                }
            }
            if let Some(p) = state.pointer.take() {
                if p.version() >= 3 {
                    p.release();
                }
            }
            if let Some(d) = data_device {
                if d.version() >= 2 {
                    d.release();
                }
            }
        })
}

impl Dispatch<WlRegistry, GlobalListContents> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals come and go (outputs, hot-plugged seats); the objects the
        // video needs were bound at creation and a removed seat simply stops
        // sending.
    }
}

impl Dispatch<WlSeat, ()> for Dispatcher {
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

impl Dispatch<WlPointer, ()> for Dispatcher {
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
                if let (true, Some(s)) = (state.inside, &state.shared) {
                    state.last = (surface_x, surface_y);
                    s.pointer_moved(surface_x, surface_y);
                }
            }
            // Leaving for the revealed bar or another window. No button can be
            // held across a leave (the implicit grab keeps the focus), so
            // there is nothing to release here.
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
                    state.last = (surface_x, surface_y);
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
                    let (x, y) = state.last;
                    s.pointer_button(button, b == wl_pointer::ButtonState::Pressed, x, y);
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
            } => state.wheel.detents(
                axis == wl_pointer::Axis::HorizontalScroll,
                discrete.saturating_mul(120),
            ),
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

impl Dispatcher {
    /// A drag entered `surface`. Only drags onto the video are touched: every
    /// data device of this client gets its own offer for the same source, and
    /// an `accept(NULL)` from us would override GTK's answer for its own
    /// widgets.
    fn drag_enter(
        &mut self,
        serial: u32,
        surface: &WlSurface,
        pos: (f64, f64),
        offer: WlDataOffer,
    ) {
        if let Some(old) = self.dropped_elsewhere.take() {
            old.destroy();
        }
        if let Some(old) = self.drag.take() {
            old.offer.destroy();
        }
        let accepts = self
            .shared
            .as_ref()
            .is_some_and(|s| s.state.accepts_drops());
        let offers_list = offer
            .data::<OfferTypes>()
            .is_some_and(|t| lock(&t.0).iter().any(|m| m == URI_LIST));
        let accepted = self.is_video(surface) && offers_list && accepts;
        if accepted {
            offer.accept(serial, Some(URI_LIST.to_owned()));
            if offer.version() >= 3 {
                // A v3 source cancels a drop whose action was never negotiated.
                offer.set_actions(DndAction::Copy, DndAction::Copy);
            }
        }
        // Not accepted: no request at all, not even accept(NULL) — over GTK's
        // widgets GTK answers through its own offer for the same source, and
        // over the video an unanswered offer is a refused drop.
        self.drag = Some(Drag {
            offer,
            pos,
            accepted,
        });
    }

    /// The user let go: if over the video, read the list off the input path
    /// (the source writes it at its own pace) and hand it to the drop sink.
    fn drag_drop(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let Some(s) = self.shared.clone().filter(|_| drag.accepted) else {
            // GTK may still be reading its own offer for this drop; release
            // ours only once the next drag starts.
            self.dropped_elsewhere = Some(drag.offer);
            return;
        };
        let (px, py) = s.to_full_px(drag.pos.0, drag.pos.1, false);
        let (nx, ny) = s.state.normalize(px, py);
        let (reader, writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("video surface: drop pipe failed: {e}");
                drag.offer.destroy();
                return;
            }
        };
        drag.offer.receive(URI_LIST.to_owned(), writer.as_fd());
        // Our copy of the write end must close, or the read never sees EOF.
        drop(writer);
        if let Err(e) = s.wl.conn.flush() {
            tracing::warn!("video surface: Wayland flush failed: {e}");
        }
        let spawned = std::thread::Builder::new()
            .name("wl-video-drop".into())
            .spawn(move || {
                let mut data = Vec::new();
                let read = reader.take(MAX_LIST_BYTES).read_to_end(&mut data);
                let paths = match read {
                    Ok(_) => parse_uri_list(&data),
                    Err(e) => {
                        tracing::warn!("video surface: reading the dropped list failed: {e}");
                        Vec::new()
                    }
                };
                let delivered =
                    !paths.is_empty() && s.state.deliver_drop(FileDrop { paths, nx, ny });
                if delivered && drag.offer.version() >= 3 {
                    drag.offer.finish();
                }
                drag.offer.destroy();
                if let Err(e) = s.wl.conn.flush() {
                    tracing::warn!("video surface: Wayland flush failed: {e}");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("video surface: cannot start the drop reader: {e}");
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Dispatcher {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id: Some(offer),
            } => state.drag_enter(serial, &surface, (x, y), offer),
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(drag) = &mut state.drag {
                    drag.pos = (x, y);
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(drag) = state.drag.take() {
                    drag.offer.destroy();
                }
            }
            wl_data_device::Event::Drop => state.drag_drop(),
            // Clipboard offers are GTK's business (and the clipboard
            // module's); ours are just released.
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(Dispatcher, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, OfferTypes::default()),
    ]);
}

impl Dispatch<WlDataOffer, OfferTypes> for Dispatcher {
    fn event(
        _: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        types: &OfferTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            lock(&types.0).push(mime_type);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for Dispatcher {
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

// Objects whose events we do not need. wl_callback: the stop wake-up only has
// to make `blocking_dispatch` return. wl_surface (area): enter/leave output
// and preferred_buffer_scale — the scale comes from fractional-scale or GDK.
// wl_buffer: release — the transparent pixel is attached again as is, which
// wl_surface.attach allows for shm buffers. wl_shm: format list — ARGB8888 is
// mandatory for every compositor.
delegate_noop!(Dispatcher: ignore WlCallback);
delegate_noop!(Dispatcher: ignore WlSurface);
delegate_noop!(Dispatcher: ignore WlBuffer);
delegate_noop!(Dispatcher: ignore WlShm);
delegate_noop!(Dispatcher: WlCompositor);
delegate_noop!(Dispatcher: WlSubcompositor);
delegate_noop!(Dispatcher: WlSubsurface);
delegate_noop!(Dispatcher: WlRegion);
delegate_noop!(Dispatcher: WlShmPool);
delegate_noop!(Dispatcher: WpViewporter);
delegate_noop!(Dispatcher: WpViewport);
delegate_noop!(Dispatcher: WpFractionalScaleManagerV1);
delegate_noop!(Dispatcher: WlDataDeviceManager);
