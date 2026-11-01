//! Wayland wlroots backend: `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`.
//!
//! Sway, Hyprland, river, labwc and other wlroots compositors let any client
//! create a virtual pointer and keyboard on the seat, with no dialog. Their
//! events enter the seat like a second mouse and keyboard.
//!
//! Pointer coordinates: `motion_absolute(x, y, x_extent, y_extent)` positions
//! the pointer over the whole output layout (the protocol's "global compositor
//! space"); the layout is the bounding box of the outputs' xdg-output logical
//! rectangles, and the target rectangle is read in the same logical space
//! (what the ScreenCast portal reports as a stream's `position` and `size`).
//!
//! Keyboard: the protocol needs a keymap before the first key. We hand over
//! the seat's own keymap when it has a keyboard, so injected physical keys
//! produce whatever the host's layout says (the protocol's layout-independent
//! promise), else [`xkb::DEFAULT_KEYMAP`]. Modifiers are tracked here because
//! wlroots does not derive them from a virtual keyboard's keys (see [`xkb`]).

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::Instant;

use platform_api::Rect;
use protocol::Button;
use rustix::fs::{memfd_create, MemfdFlags};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_keyboard::{self, KeymapFormat, WlKeyboard};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::mapping::{self, Point, WheelSteps};
use super::xkb::{self, Modifiers};
use super::Backend;

const POINTER_MANAGER: &str = "zwlr_virtual_pointer_manager_v1";
const KEYBOARD_MANAGER: &str = "zwp_virtual_keyboard_manager_v1";

/// Without a known layout, the extent is arbitrary; this one keeps 1/65536
/// precision, finer than any monitor.
const UNKNOWN_EXTENT: u32 = 0xffff;

/// libinput's scroll distance for one wheel detent, which clients that ignore
/// `axis_discrete` use instead.
const DETENT_DISTANCE: f64 = 15.0;

/// `wl_keyboard.key_state` (the virtual keyboard reuses its values).
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// `wl_keyboard.keymap_format.xkb_v1`.
const XKB_V1: u32 = 1;

/// Whether the session's compositor offers both virtual-input protocols.
pub(super) fn available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<State>(&conn) else {
        return false;
    };
    let list = globals.contents().clone_list();
    let has = |name: &str| list.iter().any(|g| g.interface == name);
    has(POINTER_MANAGER) && has(KEYBOARD_MANAGER)
}

/// Every output's logical rectangle (xdg-output) on the session's compositor,
/// in the compositor's order. Empty when there is no Wayland connection or no
/// xdg-output support. GNOME and KDE implement xdg-output too, so the uinput
/// backend uses this to learn the layout on any Wayland desktop.
pub(super) fn output_rects() -> Vec<Rect> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Vec::new();
    };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else {
        return Vec::new();
    };
    let mut state = State::default();
    bind_outputs(&globals, &queue.handle(), &mut state);
    match queue.roundtrip(&mut state) {
        Ok(_) => state.output_rects(),
        Err(e) => {
            tracing::debug!("wayland: reading the output layout failed: {e}");
            Vec::new()
        }
    }
}

/// Bind every `wl_output` and, where xdg-output exists, its logical geometry.
/// Outputs are numbered in registry order; that index is each one's user data.
fn bind_outputs(globals: &GlobalList, qh: &QueueHandle<State>, state: &mut State) {
    let Ok(manager) = globals.bind::<ZxdgOutputManagerV1, _, _>(qh, 1..=3, ()) else {
        return;
    };
    for global in globals.contents().clone_list() {
        if global.interface != WlOutput::interface().name {
            continue;
        }
        let index = state.outputs.len();
        let output: WlOutput =
            globals
                .registry()
                .bind(global.name, global.version.min(4), qh, index);
        manager.get_xdg_output(&output, qh, index);
        state.outputs.push(OutputGeometry::default());
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct OutputGeometry {
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

#[derive(Default)]
struct State {
    seat_has_keyboard: bool,
    /// The seat keyboard's keymap: fd and size.
    keymap: Option<(OwnedFd, u32)>,
    outputs: Vec<OutputGeometry>,
}

impl State {
    fn output_rects(&self) -> Vec<Rect> {
        self.outputs
            .iter()
            .filter_map(|o| {
                let (x, y) = o.position?;
                let (w, h) = o.size?;
                Some(Rect {
                    x,
                    y,
                    w: u32::try_from(w).ok().filter(|w| *w > 0)?,
                    h: u32::try_from(h).ok().filter(|h| *h > 0)?,
                })
            })
            .collect()
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Outputs plugged in later are not followed: the layout is read once,
        // like the X11 backend's primary monitor.
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            state.seat_has_keyboard = caps.contains(Capability::Keyboard);
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(KeymapFormat::XkbV1),
            fd,
            size,
        } = event
        {
            state.keymap = Some((fd, size));
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as Proxy>::Event,
        _: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Geometry comes from xdg-output, which reports logical coordinates;
        // wl_output's own are physical and pre-transform.
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => output.position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.size = Some((width, height));
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ZxdgOutputManagerV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

fn wl_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("wlroots virtual input: {what} failed: {e}")
}

pub(super) struct Wlr {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    modifiers: Modifiers,
    /// Bounding box of every output (logical), which `motion_absolute` spans.
    layout: Option<Rect>,
    /// What `None` targets map onto: wlroots has no primary output, so the
    /// first one the compositor lists.
    primary: Option<Rect>,
    /// The protocol's timestamps have "an undefined base" but one clock per
    /// object; milliseconds since construction is that clock.
    epoch: Instant,
    dead: bool,
}

impl Wlr {
    pub(super) fn connect_to_env() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| {
            format!("wlroots virtual input: cannot connect to the Wayland compositor ({e})")
        })?;
        Self::with_connection(conn)
    }

    /// Set up both devices on `conn`'s compositor.
    pub(super) fn with_connection(conn: Connection) -> Result<Self, String> {
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| wl_error("reading the registry", e))?;
        let qh = queue.handle();
        let mut state = State::default();
        let missing = |name: &str| {
            format!(
                "wlroots virtual input: the compositor does not offer {name} (only wlroots-based \
                 compositors such as Sway and Hyprland do)"
            )
        };
        // Version 3 for `wl_keyboard.release`; nothing newer is needed.
        let seat: WlSeat = globals
            .bind(&qh, 1..=3, ())
            .map_err(|_| missing("a wl_seat"))?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| missing(POINTER_MANAGER))?;
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| missing(KEYBOARD_MANAGER))?;
        bind_outputs(&globals, &qh, &mut state);
        queue
            .roundtrip(&mut state)
            .map_err(|e| wl_error("reading the seat and outputs", e))?;

        if state.seat_has_keyboard {
            // `get_keyboard` on a seat that never had a keyboard is a protocol
            // error, hence the capability check. The keymap arrives at once.
            let seat_keyboard = seat.get_keyboard(&qh, ());
            queue
                .roundtrip(&mut state)
                .map_err(|e| wl_error("reading the seat keymap", e))?;
            if seat_keyboard.version() >= 3 {
                seat_keyboard.release();
            }
        }
        let (keymap_fd, keymap_size, keymap_text) = match state.keymap.take() {
            Some((fd, size)) => {
                let text = read_keymap(&fd, size)?;
                (fd, size, text)
            }
            None => {
                let (fd, size) = default_keymap_fd()?;
                (fd, size, xkb::DEFAULT_KEYMAP.to_owned())
            }
        };

        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        keyboard.keymap(XKB_V1, keymap_fd.as_fd(), keymap_size);
        // The keymap fd is ours to close once sent; the compositor got a copy.
        drop(keymap_fd);
        queue.roundtrip(&mut state).map_err(|e| {
            wl_error(
                "creating the virtual devices (is the client allowed to use them?)",
                e,
            )
        })?;

        let outputs = state.output_rects();
        Ok(Self {
            conn,
            queue,
            state,
            pointer,
            keyboard,
            modifiers: Modifiers::from_keymap(&keymap_text),
            layout: mapping::bounding_box(&outputs),
            primary: outputs.first().copied(),
            epoch: Instant::now(),
            dead: false,
        })
    }

    /// A round trip: everything sent so far has been processed, and any
    /// protocol error it caused is reported.
    #[cfg(test)]
    pub(super) fn sync(&mut self) -> Result<(), String> {
        if self.dead {
            return Err("the connection already failed".into());
        }
        self.queue
            .roundtrip(&mut self.state)
            .map(drop)
            .map_err(|e| wl_error("a round trip", e))
    }

    fn now(&self) -> u32 {
        // Wrapping after 49 days is what a u32 millisecond clock does; the
        // truncation is the intent.
        self.epoch.elapsed().as_millis() as u32
    }

    fn lost(&mut self, what: &str, e: impl std::fmt::Display) {
        if !self.dead {
            let detail = self
                .conn
                .protocol_error()
                .map(|p| format!(" (protocol error: {p})"))
                .unwrap_or_default();
            tracing::warn!("wlroots virtual input: {what} failed: {e}{detail}; remote input stops");
            self.dead = true;
        }
    }
}

/// The seat keymap's text, for its modifier map.
fn read_keymap(fd: &OwnedFd, size: u32) -> Result<String, String> {
    let file = File::from(
        fd.try_clone()
            .map_err(|e| wl_error("duplicating the keymap fd", e))?,
    );
    let mut text = vec![0; usize::try_from(size).unwrap_or(0)];
    // Positioned read: the fd's offset may be shared with the compositor.
    file.read_exact_at(&mut text, 0)
        .map_err(|e| wl_error("reading the seat keymap", e))?;
    // The text is NUL-terminated (the size counts the terminator).
    if let Some(end) = text.iter().position(|b| *b == 0) {
        text.truncate(end);
    }
    Ok(String::from_utf8_lossy(&text).into_owned())
}

/// [`xkb::DEFAULT_KEYMAP`] in an anonymous memory file, as the protocol wants
/// (an fd the compositor can map), with the NUL terminator xkbcommon expects.
fn default_keymap_fd() -> Result<(OwnedFd, u32), String> {
    let fd = memfd_create("sharectrl-keymap", MemfdFlags::CLOEXEC)
        .map_err(|e| wl_error("creating the keymap file", e))?;
    let mut file = File::from(fd);
    file.write_all(xkb::DEFAULT_KEYMAP.as_bytes())
        .and_then(|()| file.write_all(&[0]))
        .map_err(|e| wl_error("writing the keymap file", e))?;
    let size = u32::try_from(xkb::DEFAULT_KEYMAP.len() + 1)
        .map_err(|_| wl_error("sizing the keymap", "too large"))?;
    Ok((OwnedFd::from(file), size))
}

impl Backend for Wlr {
    fn pointer(&mut self, at: Point) {
        // On Wayland the shared monitor is the one picked in the portal dialog
        // (published by the capture); without one, our own primary stand-in.
        let shared = crate::portal_monitor::current().and_then(|m| m.rect);
        let at = mapping::on_primary(at, shared.or(self.primary));
        let (fx, fy) = mapping::layout_fraction(at, self.layout);
        let (ext_x, ext_y) = self
            .layout
            .map_or((UNKNOWN_EXTENT, UNKNOWN_EXTENT), |l| (l.w, l.h));
        // `f` is in [0,1], so the float-to-int cast cannot overflow.
        let along =
            |f: f64, ext: u32| ((f * f64::from(ext)).floor() as u32).min(ext.saturating_sub(1));
        let time = self.now();
        self.pointer
            .motion_absolute(time, along(fx, ext_x), along(fy, ext_y), ext_x, ext_y);
        self.pointer.frame();
    }

    fn button(&mut self, button: Button, down: bool) {
        let state = if down {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        let time = self.now();
        self.pointer
            .button(time, u32::from(mapping::evdev_button(button)), state);
        self.pointer.frame();
    }

    fn wheel(&mut self, steps: WheelSteps) {
        if steps.notches_x == 0 && steps.notches_y == 0 {
            return;
        }
        let time = self.now();
        self.pointer.axis_source(AxisSource::Wheel);
        // wl_pointer axes grow down / right; the protocol's wheel grows up / right.
        for (axis, discrete) in [
            (Axis::VerticalScroll, -steps.notches_y),
            (Axis::HorizontalScroll, steps.notches_x),
        ] {
            if discrete != 0 {
                self.pointer.axis_discrete(
                    time,
                    axis,
                    DETENT_DISTANCE * f64::from(discrete),
                    discrete,
                );
            }
        }
        self.pointer.frame();
    }

    fn key(&mut self, evdev: u16, down: bool) {
        let state = if down { KEY_PRESSED } else { KEY_RELEASED };
        let time = self.now();
        self.keyboard.key(time, u32::from(evdev), state);
        // After the key, as a hardware keyboard's driver reports it.
        if let Some(mods) = self.modifiers.key(evdev, down) {
            self.keyboard.modifiers(mods.depressed, 0, mods.locked, 0);
        }
    }

    fn flush(&mut self) {
        if self.dead {
            return;
        }
        if let Err(e) = self.conn.flush() {
            self.lost("sending events", e);
            return;
        }
        // Read whatever the compositor sent (nothing, normally) without
        // blocking, so a protocol error is noticed and its socket buffer never
        // fills up behind an unread queue.
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    self.lost("reading from the compositor", e);
                    return;
                }
            }
        }
        if let Err(e) = self.queue.dispatch_pending(&mut self.state) {
            self.lost("handling compositor events", e);
        }
    }
}

impl Drop for Wlr {
    fn drop(&mut self) {
        self.pointer.destroy();
        self.keyboard.destroy();
        // Best effort: the compositor removes our devices when the
        // connection closes anyway.
        if let Err(e) = self.conn.flush() {
            tracing::debug!("wlroots virtual input: final flush failed: {e}");
        }
    }
}
