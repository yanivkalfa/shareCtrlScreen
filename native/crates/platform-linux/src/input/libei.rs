//! Wayland GNOME / KDE backend: the RemoteDesktop portal's libei connection.
//!
//! The portal session ([`crate::portal_shared`], shared with screen capture)
//! is started on the portal runtime, then `ConnectToEIS` hands back a socket
//! to the compositor's EIS server, and this module speaks the EI protocol on
//! it as a *sender* client (with the pure-Rust `reis`).
//!
//! The injector is created on the transport thread and must never block it,
//! but the portal may be waiting for the user to answer a dialog, and EI needs
//! a reader for its pings and device announcements. So [`Libei`] is only a
//! channel: a task on the portal runtime waits for the session and connects,
//! then a dedicated client thread owns the EI connection and applies commands
//! while devices are resumed. Commands that
//! arrive before that (or while the compositor has paused the devices) are
//! dropped: a stale pointer move is worthless, and the front end's key/button
//! tracking only sends releases for presses, which the task in turn only
//! forwards for presses it really sent.
//!
//! EI semantics this relies on (protocol.xml):
//! * the compositor may split capabilities across devices (mutter: a pointer,
//!   an absolute pointer, a keyboard), so each event goes to the first resumed
//!   device with the capability;
//! * `start_emulating` after every `resumed`, with a sequence number that
//!   grows by at least one each time;
//! * one `frame` after every event, because one `motion_absolute` per frame is
//!   the most a client may send, and grouping unrelated events is not wanted;
//! * absolute positions are logical pixels inside one of the device's
//!   regions, else the event is silently discarded ([`mapping::region_position`]);
//! * `scroll_discrete` takes 1/120 detents, positive = down / right.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

use platform_api::Rect;
use protocol::Button;
use reis::ei::{self, handshake::ContextType};
use reis::event::{Connection, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::{EiHandshaker, HandshakeResp};
use reis::PendingRequestResult;
use rustix::time::{clock_gettime, ClockId};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::mapping::{self, Point, WheelSteps};
use super::{Backend, DEVICE_NAME};
use crate::portal_shared::{self, SharedSession};

/// One validated event for the connection task.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Pointer(Point),
    Button {
        code: u32,
        down: bool,
    },
    /// Protocol (Windows) wheel units: positive = up / right.
    Wheel {
        v120_x: i32,
        v120_y: i32,
    },
    Key {
        code: u32,
        down: bool,
    },
}

pub(super) struct Libei {
    tx: UnboundedSender<Command>,
}

impl Libei {
    /// Start the background connection; returns at once.
    pub(super) fn start(state_dir: &Path) -> Result<Self, String> {
        let rt = crate::permissions::runtime()
            .ok_or("RemoteDesktop portal: the portal runtime could not start (see app.log)")?;
        let session = portal_shared::acquire(state_dir);
        let (tx, rx) = unbounded_channel();
        rt.spawn(run_portal(session, rx));
        Ok(Self { tx })
    }

    /// A backend on an already-connected EIS socket (tests use an in-process
    /// EIS server; there is no portal).
    #[cfg(test)]
    pub(super) fn on_socket(
        socket: UnixStream,
    ) -> (Self, std::thread::JoinHandle<Result<(), String>>) {
        let (tx, rx) = unbounded_channel();
        let thread = spawn_client(socket, rx, ()).expect("the libei thread starts");
        (Self { tx }, thread)
    }

    fn send(&self, command: Command) {
        // Fails only once the task has ended, which it logged; input after
        // that has nowhere to go.
        let _ = self.tx.send(command);
    }
}

impl Backend for Libei {
    fn pointer(&mut self, at: Point) {
        self.send(Command::Pointer(at));
    }

    fn button(&mut self, button: Button, down: bool) {
        self.send(Command::Button {
            code: u32::from(mapping::evdev_button(button)),
            down,
        });
    }

    fn wheel(&mut self, steps: WheelSteps) {
        self.send(Command::Wheel {
            v120_x: steps.v120_x,
            v120_y: steps.v120_y,
        });
    }

    fn key(&mut self, evdev: u16, down: bool) {
        self.send(Command::Key {
            code: u32::from(evdev),
            down,
        });
    }

    fn flush(&mut self) {
        // The task flushes the socket after every command it applies.
    }
}

/// Wait for the portal session and connect to its EIS server (on the portal
/// runtime, where ashpd's D-Bus connection lives), then hand the socket to the
/// EI client thread. `session` goes along so the portal session stays open
/// for as long as the client runs.
async fn run_portal(session: Arc<SharedSession>, mut rx: UnboundedReceiver<Command>) {
    let connect = async {
        let granted = session.granted().await.map_err(|e| e.message.clone())?;
        granted.connect_to_eis().await
    };
    let fd = tokio::select! {
        fd = connect => fd,
        // The injector went away (session over) before the desktop answered.
        // Returning drops our handle; with no other holder the shared session
        // abandons its dialog (see `portal_shared`).
        () = drain(&mut rx) => return,
    };
    let started = fd.and_then(|fd| {
        spawn_client(UnixStream::from(fd), rx, session)
            .map_err(|e| ei_error("starting the libei thread", e))
    });
    if let Err(e) = started {
        tracing::warn!("remote control (libei): {e}; remote input stops");
    }
}

/// Run the EI client on its own thread. reis's event converter is not `Send`
/// (it stores callbacks as plain boxed closures), so it cannot live in a task
/// of the multi-threaded portal runtime; a current-thread runtime on a
/// dedicated thread gives it the async socket it needs. `keep_alive` is
/// dropped when the client ends.
fn spawn_client<K: Send + 'static>(
    socket: UnixStream,
    rx: UnboundedReceiver<Command>,
    keep_alive: K,
) -> io::Result<std::thread::JoinHandle<Result<(), String>>> {
    std::thread::Builder::new()
        .name("sharectrl-libei".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .map_err(|e| ei_error("starting the libei runtime", e))
                .and_then(|rt| rt.block_on(run_ei(socket, rx)));
            if let Err(e) = &result {
                tracing::warn!("remote control (libei): {e}; remote input stops");
            }
            drop(keep_alive);
            result
        })
}

/// Discard commands until the sender is dropped.
async fn drain(rx: &mut UnboundedReceiver<Command>) {
    while rx.recv().await.is_some() {}
}

fn ei_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what} failed: {e}")
}

/// The EI client proper, on a connected socket. Ends `Ok` when the injector
/// is dropped, `Err` when the compositor disconnects or breaks the protocol.
async fn run_ei(socket: UnixStream, mut rx: UnboundedReceiver<Command>) -> Result<(), String> {
    let context = ei::Context::new(socket).map_err(|e| ei_error("setting up the EI socket", e))?;
    // SAFETY: the clone shares the context's socket, which stays open while
    // any clone exists; the AsyncFd owns one, so its fd stays valid and the
    // same for the AsyncFd's whole life.
    let fd = unsafe { AsyncFd::register_with_interest(context.clone(), Interest::READABLE) }
        .map_err(|e| ei_error("registering the EI socket", e))?;

    let mut handshaker = EiHandshaker::new(DEVICE_NAME, ContextType::Sender);
    let resp: HandshakeResp = 'handshake: loop {
        tokio::select! {
            read = read_socket(&fd) => read?,
            command = rx.recv() => match command {
                Some(_) => continue,
                None => return Ok(()),
            },
        }
        while let Some(event) = context.pending_event() {
            if let Some(resp) = handshaker
                .handle_event(parsed(event)?)
                .map_err(|e| ei_error("the EI handshake", e))?
            {
                break 'handshake resp;
            }
        }
    };
    let mut converter = EiEventConverter::new(&context, resp);
    let mut state = EiState::default();

    loop {
        // Events may already be buffered (sent right after the handshake), so
        // drain before waiting.
        while let Some(event) = context.pending_event() {
            converter
                .handle_event(parsed(event)?)
                .map_err(|e| ei_error("reading EI events", e))?;
        }
        while let Some(event) = converter.next_event() {
            state.on_event(event)?;
        }
        flush(converter.connection())?;
        tokio::select! {
            read = read_socket(&fd) => read?,
            command = rx.recv() => match command {
                Some(command) => state.apply(converter.connection(), &command),
                None => {
                    state.shutdown(converter.connection());
                    return flush(converter.connection());
                }
            },
        }
    }
}

fn parsed(event: PendingRequestResult<ei::Event>) -> Result<ei::Event, String> {
    match event {
        PendingRequestResult::Request(event) => Ok(event),
        PendingRequestResult::ParseError(e) => Err(ei_error("parsing an EI message", e)),
        PendingRequestResult::InvalidObject(id) => Err(format!(
            "the compositor sent an event for unknown EI object {id}"
        )),
    }
}

/// Wait until the socket is readable and pull everything into reis's buffer.
async fn read_socket(fd: &AsyncFd<ei::Context>) -> Result<(), String> {
    let mut guard = fd
        .readable()
        .await
        .map_err(|e| ei_error("waiting on the EI socket", e))?;
    match guard.get_inner().read() {
        Ok(_) => {
            // `read` drains the socket to WouldBlock, so readiness is spent.
            guard.clear_ready();
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            guard.clear_ready();
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Err("the desktop closed the remote-control connection".into())
        }
        Err(e) => Err(ei_error("reading the EI socket", e)),
    }
}

fn flush(connection: &Connection) -> Result<(), String> {
    connection
        .flush()
        .map_err(|e| ei_error("writing to the EI socket", e))
}

/// CLOCK_MONOTONIC in microseconds, the clock EI frame timestamps use.
fn now_micros() -> u64 {
    let t = clock_gettime(ClockId::Monotonic);
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(t.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000).saturating_add(nanos / 1_000)
}

struct EiDevice {
    device: reis::event::Device,
    /// Between `resumed` (+ our `start_emulating`) and `paused`.
    emulating: bool,
}

#[derive(Default)]
struct EiState {
    devices: Vec<EiDevice>,
    /// `start_emulating` sequence; must grow on every call.
    sequence: u32,
    /// What we actually pressed, by evdev code, so only real presses are
    /// released (EI calls a release without a press a client bug).
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
}

impl EiState {
    fn index_of(&self, device: &reis::event::Device) -> Option<usize> {
        self.devices.iter().position(|d| d.device == *device)
    }

    fn on_event(&mut self, event: EiEvent) -> Result<(), String> {
        match event {
            EiEvent::SeatAdded(e) => e.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            ),
            EiEvent::DeviceAdded(e) => self.devices.push(EiDevice {
                device: e.device,
                emulating: false,
            }),
            EiEvent::DeviceResumed(e) => {
                if let Some(i) = self.index_of(&e.device) {
                    self.sequence = self.sequence.wrapping_add(1);
                    self.devices[i]
                        .device
                        .device()
                        .start_emulating(e.serial, self.sequence);
                    self.devices[i].emulating = true;
                }
            }
            EiEvent::DevicePaused(e) => {
                if let Some(i) = self.index_of(&e.device) {
                    self.devices[i].emulating = false;
                    // A paused device is reset to neutral by EIS ("all keys
                    // logically up"), so nothing is held through it anymore.
                    self.forget_held(i);
                }
            }
            EiEvent::DeviceRemoved(e) => {
                if let Some(i) = self.index_of(&e.device) {
                    self.forget_held(i);
                    self.devices.remove(i);
                }
            }
            EiEvent::Disconnected(e) => {
                return Err(format!(
                    "the desktop ended remote control ({:?}{})",
                    e.reason,
                    e.explanation.map(|x| format!(": {x}")).unwrap_or_default()
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn forget_held(&mut self, i: usize) {
        let device = &self.devices[i].device;
        if device.has_capability(DeviceCapability::Keyboard) {
            self.keys.clear();
        }
        if device.has_capability(DeviceCapability::Button) {
            self.buttons.clear();
        }
    }

    /// The first resumed device with `capability`. A clone (a reference-counted
    /// handle) so the held-key sets can be updated while it is in use.
    fn emulating(&self, capability: DeviceCapability) -> Option<reis::event::Device> {
        self.devices
            .iter()
            .find(|d| d.emulating && d.device.has_capability(capability))
            .map(|d| d.device.clone())
    }

    fn apply(&mut self, connection: &Connection, command: &Command) {
        let serial = connection.serial();
        match *command {
            Command::Pointer(at) => {
                let Some(device) = self.emulating(DeviceCapability::PointerAbsolute) else {
                    return;
                };
                let regions: Vec<Rect> = device.regions().iter().filter_map(region_rect).collect();
                let (Some(pointer), Some((x, y))) = (
                    device.interface::<ei::PointerAbsolute>(),
                    mapping::region_position(&regions, at),
                ) else {
                    return;
                };
                pointer.motion_absolute(x, y);
                device.device().frame(serial, now_micros());
            }
            Command::Button { code, down } => {
                let Some(device) = self.emulating(DeviceCapability::Button) else {
                    return;
                };
                let Some(button) = device.interface::<ei::Button>() else {
                    return;
                };
                let state = if down {
                    if !self.buttons.insert(code) {
                        return;
                    }
                    ei::button::ButtonState::Press
                } else {
                    if !self.buttons.remove(&code) {
                        return;
                    }
                    ei::button::ButtonState::Released
                };
                button.button(code, state);
                device.device().frame(serial, now_micros());
            }
            Command::Wheel { v120_x, v120_y } => {
                let Some(device) = self.emulating(DeviceCapability::Scroll) else {
                    return;
                };
                let Some(scroll) = device.interface::<ei::Scroll>() else {
                    return;
                };
                // EI's y grows downward; the protocol's wheel grows upward.
                scroll.scroll_discrete(v120_x, v120_y.saturating_neg());
                device.device().frame(serial, now_micros());
            }
            Command::Key { code, down } => {
                let Some(device) = self.emulating(DeviceCapability::Keyboard) else {
                    return;
                };
                let Some(keyboard) = device.interface::<ei::Keyboard>() else {
                    return;
                };
                let state = if down {
                    if !self.keys.insert(code) {
                        return;
                    }
                    ei::keyboard::KeyState::Press
                } else {
                    if !self.keys.remove(&code) {
                        return;
                    }
                    ei::keyboard::KeyState::Released
                };
                keyboard.key(code, state);
                device.device().frame(serial, now_micros());
            }
        }
    }

    /// The injector is gone: release what is still held (the front end's
    /// `release_all` normally did already) and stop emulating, as EI asks of
    /// a client before it leaves.
    fn shutdown(&mut self, connection: &Connection) {
        // Copies: `apply` removes each code from the set as it releases it.
        for code in self.keys.clone() {
            self.apply(connection, &Command::Key { code, down: false });
        }
        for code in self.buttons.clone() {
            self.apply(connection, &Command::Button { code, down: false });
        }
        let serial = connection.serial();
        for d in self.devices.iter_mut().filter(|d| d.emulating) {
            d.device.device().stop_emulating(serial);
            d.emulating = false;
        }
    }
}

/// An EI region as a [`Rect`]; `None` for one too large for `i32` coordinates.
fn region_rect(r: &reis::event::Region) -> Option<Rect> {
    Some(Rect {
        x: i32::try_from(r.x).ok()?,
        y: i32::try_from(r.y).ok()?,
        w: r.width,
        h: r.height,
    })
}
