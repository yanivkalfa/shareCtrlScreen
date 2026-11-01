//! X11 shortcut capture: an XInput2 keyboard grab on the video window, held
//! exactly while the session window has the input focus and the video is shown.
//!
//! ## Why `XIGrabDevice` and not the core `XGrabKeyboard`
//!
//! Both grabs behave the same (async mode, the same `AlreadyGrabbed` / `Frozen`
//! / `NotViewable` refusals, ended by the server when the grab window becomes
//! unviewable). But a core grab delivers core `KeyPress` events, which cannot
//! tell a real keyboard from XTEST, and the trait requires the hook to ignore
//! events produced by our own injector, which (host side, task L6) types
//! through XTEST. XI2 device events carry the `sourceid` of the slave device
//! that produced them, so input from an XTEST device can be recognised.
//!
//! Recognised XTEST input is not forwarded, and the grab steps aside until the
//! session window loses focus or the video is hidden: a grab cannot pass a key
//! through, so dropping it is the only way to let XTEST-driven typing (our
//! injector, or a user driving this machine through Synergy / VNC) reach the
//! app. A press the grab took is replayed through XTEST after the ungrab,
//! following the key's state at that moment: the focused window gets the press
//! and, unless the key is still held, a release, so the key ends up in the
//! state its sender left it in. The guarantee is "no stuck key and no lost
//! press"; a tap whose release raced the stand-down can arrive twice, and a
//! still-held key gets an extra release before its press. From then on XTEST
//! keys reach GTK like any key.
//!
//! ## Not under XWayland
//!
//! An X client's grab under XWayland binds only other X clients: the Wayland
//! compositor keeps its shortcuts (mutter honours XWayland grabs only for
//! allow-listed apps). `install` therefore refuses when the server is
//! XWayland, with the remedy (start without `GDK_BACKEND=x11`, so the Wayland
//! inhibitor is used).
//!
//! ## Design
//!
//! * **Our own connection.** Grabs and event selections belong to the client
//!   that made them; ours never disturb GTK's. The server is `$DISPLAY` (the one
//!   GTK uses); `install` proves it is the right one by finding the video window
//!   under the app window there.
//! * **State is read back from the server, never inferred from event details.**
//!   Any focus or structure event re-queries `GetInputFocus` and the video's map
//!   state. Activating a grab itself generates focus events (mode `Grab` /
//!   `Ungrab`), and decoding the focus-detail matrix by hand is how stuck grabs
//!   happen.
//! * **"Focused"** = the input focus is the app window or one of its
//!   descendants. A focus of `PointerRoot` or `None` is never ours.
//! * **The video is a direct child of the app window** (the L5 contract), so
//!   structure events on those two windows are all it takes to see the video
//!   become viewable or not; `install` refuses any other layout.
//! * **Keys already down when the grab starts** went to GTK (and so to the
//!   remote through the surface); their releases land in our grab. The grab
//!   reads the key state when it starts and forwards those releases too (a
//!   stray key-up is harmless; a missing one is a stuck key).
//! * **Async grab mode.** The server never freezes the keyboard waiting for us,
//!   so a slow hook thread cannot stall input (the trait's "never block the OS
//!   input path").
//! * **Click-away is the escape.** Only the keyboard is grabbed; a click on
//!   another window still reaches the window manager, which moves the focus, and
//!   the focus-out ends our grab and releases every held key on the remote.
//! * **Refused grabs are retried briefly.** Window managers hold their own grab
//!   for a moment while switching windows (Alt+Tab into our window lands focus
//!   before the WM lets go), so `AlreadyGrabbed` / `Frozen` right after a focus
//!   change is normal. `install` waits [`INSTALL_PATIENCE`] and reports `false`
//!   if still refused; later focus-ins wait [`RUN_PATIENCE`] and log.
//! * **Two threads.** A reader thread blocks in `wait_for_event` and hands
//!   events over a channel; `run` owns all state, so the grab decision and the
//!   forwarding happen in one place. `shutdown` wakes the reader with a
//!   `ClientMessage` to our own unmapped window (as the clipboard owner does).
//!
//! ## References
//!
//! * X Window System Protocol: `GrabKeyboard` (grab-window viewability, status
//!   values, async mode), `GetInputFocus` / `SetInputFocus`, `FocusIn` /
//!   `FocusOut` (modes `Grab`/`Ungrab`/`WhileGrabbed`), `ChangeWindowAttributes`
//!   (per-client event masks):
//!   <https://www.x.org/releases/X11R7.7/doc/xproto/x11protocol.html>
//! * `XGrabKeyboard(3)`: <https://www.x.org/releases/current/doc/man/man3/XGrabKeyboard.3.xhtml>
//! * XI2 protocol (`XIQueryVersion` must come first, `XIGrabDevice`,
//!   `XIGetClientPointer`, `XIQueryDevice`, `XIGetProperty`, `DeviceEvent`
//!   `sourceid` and the `KeyRepeat` flag, `HierarchyEvent`):
//!   <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/specs/XI2proto.txt>
//! * The X server marks its XTEST slave devices with the boolean device property
//!   `XTEST Device` (`XI_PROP_XTEST_DEVICE`, `include/xserver-properties.h`,
//!   set in `Xi/xtest.c`): <https://gitlab.freedesktop.org/xorg/xserver>
//! * XTEST extension (`FakeInput`):
//!   <https://www.x.org/releases/X11R7.7/doc/xextproto/xtest.html>
//! * `QueryKeymap` (32-byte bit vector, keycode `8N + M` = bit `M` of byte
//!   `N`): X protocol, as above.
//! * Xwayland 23.1 and newer announce themselves with the `XWAYLAND` extension
//!   (`hw/xwayland/` in <https://gitlab.freedesktop.org/xorg/xserver>); older
//!   ones (Ubuntu 22.04 ships 22.1) are recognised by the session's
//!   `WAYLAND_DISPLAY`.
//! * x11rb `xinput` / `xtest` bindings: <https://docs.rs/x11rb/0.14/x11rb/protocol/index.html>

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xinput::{
    self, ConnectionExt as _, DeviceType, GrabOwner, XIEventMask, XIGetPropertyItems,
};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    EventMask, GrabMode, GrabStatus, InputFocus, MapState, NotifyDetail, NotifyMode, Window,
    WindowClass, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use platform_api::KeyForwarder;

use super::{dom_code, HeldKeys};

/// How long `install` keeps retrying a grab another client holds.
const INSTALL_PATIENCE: Duration = Duration::from_millis(300);
/// How long a later focus-in keeps retrying (WM switch animations can hold
/// their grab for a few hundred milliseconds).
const RUN_PATIENCE: Duration = Duration::from_secs(1);
/// Spacing of grab retries.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);
/// How often `run` looks at the stop flag when nothing happens.
const STOP_POLL: Duration = Duration::from_millis(100);
/// Ancestry walks stop here; real window trees are a handful of levels deep.
const MAX_TREE_DEPTH: usize = 64;
/// The XI2 version we speak. 2.0 has everything used here; asking for 2.2
/// gets the `KeyRepeat` flag on servers that have it.
const XI_VERSION: (u16, u16) = (2, 2);

/// Knobs that only tests change.
#[derive(Debug, Default)]
pub(super) struct Options {
    /// X server name; `None` = `$DISPLAY`.
    pub display: Option<String>,
    /// Forward keys from XTEST devices instead of stepping aside.
    pub accept_virtual_keyboards: bool,
}

/// Why the grab is not held although focus and visibility would allow it.
/// Cleared when either of those goes away, so the next focus-in starts fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandDown {
    /// Keys arrived from an XTEST device (see the module docs).
    VirtualKeyboard,
    /// Another client kept the keyboard grabbed past our patience.
    Refused,
}

/// A grab attempt the server turned down, classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrabResult {
    Grabbed,
    /// The video window is not viewable (raced with an unmap); the next
    /// `MapNotify` brings us back.
    NotViewable,
    /// Someone else holds the keyboard, or it is frozen by a sync grab; worth
    /// retrying.
    Busy(&'static str),
}

impl GrabResult {
    fn of(status: GrabStatus) -> Self {
        match status {
            GrabStatus::SUCCESS => GrabResult::Grabbed,
            GrabStatus::NOT_VIEWABLE => GrabResult::NotViewable,
            GrabStatus::ALREADY_GRABBED => GrabResult::Busy(
                "another program is holding the keyboard (AlreadyGrabbed); close its menu or \
                 dialog and focus the session window again",
            ),
            GrabStatus::FROZEN => GrabResult::Busy(
                "the keyboard is frozen by another program's grab (Frozen); close its menu or \
                 dialog and focus the session window again",
            ),
            // InvalidTime cannot happen with CurrentTime; any other value is a
            // server bug. Both are worth one more try.
            _ => GrabResult::Busy("the X server refused the keyboard grab"),
        }
    }
}

/// The installed X11 capture.
pub(super) struct X11Grab {
    conn: Arc<RustConnection>,
    video: Window,
    parent: Window,
    root: Window,
    /// The master keyboard paired with our client pointer (the one GTK types
    /// with). Every grabbed key event names it as `deviceid`.
    master: u16,
    /// XTEST slave keyboards (see the module docs).
    xtest: Vec<u16>,
    accept_virtual_keyboards: bool,
    forward: KeyForwarder,
    held: HeldKeys,
    /// Keycodes that were already down when the grab started (see the module
    /// docs); their releases are forwarded, their repeats ignored.
    down_at_grab: Vec<u32>,
    grabbed: bool,
    stand_down: Option<StandDown>,
    /// Retrying a refused grab until this instant.
    retry_until: Option<Instant>,
    /// The server's reason for the last refusal that exhausted the retries.
    refusal: &'static str,
    /// Retry budget for the next refusal: [`INSTALL_PATIENCE`] during
    /// `install`, [`RUN_PATIENCE`] afterwards.
    patience: Duration,
    events: Receiver<Result<Event, String>>,
    reader: Option<JoinHandle<()>>,
    wake: Window,
    shutdown: Arc<AtomicBool>,
}

fn x11_err(what: &'static str) -> impl Fn(ReplyError) -> String {
    move |e| format!("X11 {what} failed: {e}")
}

impl X11Grab {
    /// Connect, check XInput2, find the windows and devices, start listening,
    /// and take the grab now when the session window is already focused.
    pub(super) fn install(
        options: &Options,
        video: Window,
        parent: Window,
        forward: KeyForwarder,
    ) -> Result<Self, String> {
        let display = match &options.display {
            Some(d) => d.clone(),
            None => std::env::var("DISPLAY").map_err(|_| {
                "DISPLAY is not set, so there is no X server to grab on".to_string()
            })?,
        };
        // The crate's connect: also reaches a local server that only has an
        // abstract socket (WSLg, where /tmp/.X11-unix is read-only).
        let (conn, screen) = crate::x11_connect(&display)
            .map_err(|e| format!("cannot connect to the X server at {display}: {e}"))?;
        let conn = Arc::new(conn);
        let root = conn
            .setup()
            .roots
            .get(screen)
            .map(|s| s.root)
            .ok_or("the X server reported no screen for this display")?;

        let xi = conn
            .xinput_xi_query_version(XI_VERSION.0, XI_VERSION.1)
            .map_err(|e| format!("this X server has no XInput extension: {e}"))?
            .reply()
            .map_err(|e| format!("this X server has no XInput extension: {e}"))?;
        if xi.major_version < 2 {
            return Err(format!(
                "this X server only has XInput {}.{}; shortcut capture needs XInput 2",
                xi.major_version, xi.minor_version
            ));
        }

        if is_xwayland(&conn, options.display.is_none())? {
            return Err(
                "the app runs under XWayland, where the desktop keeps its shortcuts; start it \
                 without GDK_BACKEND=x11 to capture them"
                    .into(),
            );
        }
        if parent_of(&conn, video)? != Some(parent) {
            return Err(
                "the video window is not a child of the app window on this X server \
                 (is DISPLAY the display the app runs on?)"
                    .into(),
            );
        }
        let master = master_keyboard(&conn)?;
        let xtest = xtest_keyboards(&conn)?;

        // Per-client event masks: GTK's own selections on these windows are
        // untouched. Focus changes are reported on the app window for focus
        // moving into or out of it or any descendant; structure events tell us
        // when the video (or the whole window) is mapped, unmapped or gone.
        let watch = |window, mask| {
            conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(mask),
            )
            .map_err(|e| format!("X11 event selection failed: {e}"))
        };
        watch(
            parent,
            EventMask::FOCUS_CHANGE | EventMask::STRUCTURE_NOTIFY,
        )?;
        watch(video, EventMask::STRUCTURE_NOTIFY)?;
        // Device hot-plug can add or renumber XTEST devices and masters.
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: u16::from(xinput::Device::ALL),
                mask: vec![XIEventMask::HIERARCHY],
            }],
        )
        .map_err(|e| format!("X11 event selection failed: {e}"))?;

        // An unmapped 1x1 InputOnly window: the target of the wake-up message
        // that ends the reader's blocking wait.
        let wake = conn
            .generate_id()
            .map_err(|e| format!("X11 request failed: {e}"))?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            wake,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(|e| format!("X11 request failed: {e}"))?;
        conn.flush().map_err(|e| format!("X11 flush failed: {e}"))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();
        let reader = {
            let (conn, shutdown) = (conn.clone(), shutdown.clone());
            std::thread::Builder::new()
                .name("shortcuts-x11".into())
                .spawn(move || pump(&conn, &tx, &shutdown))
                .map_err(|e| format!("cannot start the shortcut thread: {e}"))?
        };

        let mut grab = Self {
            conn,
            video,
            parent,
            root,
            master,
            xtest,
            accept_virtual_keyboards: options.accept_virtual_keyboards,
            forward,
            held: HeldKeys::default(),
            down_at_grab: Vec::new(),
            grabbed: false,
            stand_down: None,
            retry_until: None,
            refusal: "",
            patience: INSTALL_PATIENCE,
            events,
            reader: Some(reader),
            wake,
            shutdown,
        };
        // From here on `grab` owns the reader; returning an error drops it,
        // which stops the thread (Drop calls `shutdown`).
        grab.refresh()?;
        while grab.retry_until.is_some() {
            std::thread::sleep(RETRY_INTERVAL);
            grab.refresh()?;
        }
        if grab.stand_down == Some(StandDown::Refused) {
            // The caller reports `install == false` with this reason.
            return Err(grab.refusal.to_string());
        }
        grab.patience = RUN_PATIENCE;
        Ok(grab)
    }

    /// Process events until `stop` is set or the connection dies.
    pub(super) fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            let timeout = if self.retry_until.is_some() {
                RETRY_INTERVAL
            } else {
                STOP_POLL
            };
            let step = match self.events.recv_timeout(timeout) {
                Ok(Ok(event)) => self.on_event(event),
                Ok(Err(e)) => Err(e),
                Err(RecvTimeoutError::Timeout) if self.retry_until.is_some() => self.refresh(),
                Err(RecvTimeoutError::Timeout) => Ok(()),
                Err(RecvTimeoutError::Disconnected) => {
                    Err("the X11 event thread stopped".to_string())
                }
            };
            if let Err(e) = step {
                tracing::warn!("shortcut capture stopped: {e}");
                self.release();
                return;
            }
        }
    }

    /// Release the grab (and every held key on the remote), stop the reader.
    /// Idempotent.
    pub(super) fn shutdown(&mut self) {
        self.release();
        let Some(reader) = self.reader.take() else {
            return;
        };
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the reader out of its blocking wait; it checks the flag after
        // every event.
        let atom = u32::from(AtomEnum::NOTICE);
        let wake = ClientMessageEvent::new(32, self.wake, atom, [0u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.wake, EventMask::NO_EVENT, wake);
        let _ = self.conn.destroy_window(self.wake);
        let _ = self.conn.flush();
        let _ = reader.join();
    }

    fn on_event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::XinputKeyPress(e) => {
                let repeat = u32::from(e.flags) & u32::from(xinput::KeyEventFlags::KEY_REPEAT) != 0;
                self.on_key(e.deviceid, e.sourceid, e.detail, true, repeat);
                Ok(())
            }
            Event::XinputKeyRelease(e) => {
                self.on_key(e.deviceid, e.sourceid, e.detail, false, false);
                Ok(())
            }
            Event::XinputHierarchy(_) => {
                self.xtest = xtest_keyboards(&self.conn)?;
                let master = master_keyboard(&self.conn)?;
                if master != self.master {
                    // Our grab (if any) was on a device that is gone or no
                    // longer the one GTK types with.
                    self.release();
                    self.master = master;
                }
                self.refresh()
            }
            Event::DestroyNotify(e) if e.window == self.video || e.window == self.parent => {
                Err("the session window was closed".into())
            }
            Event::FocusOut(e) => {
                // A stand-down lasts until focus really leaves the window (not
                // a move to a child, not a grab starting or ending). Taken from
                // the event because a quick away-and-back is over before the
                // re-query below could see it.
                let left = e.detail != NotifyDetail::INFERIOR
                    && matches!(e.mode, NotifyMode::NORMAL | NotifyMode::WHILE_GRABBED);
                if left {
                    self.stand_down = None;
                }
                self.refresh()
            }
            Event::UnmapNotify(e) => {
                // Hidden video: the same fresh start on the next show.
                if e.window == self.video {
                    self.stand_down = None;
                }
                self.refresh()
            }
            Event::FocusIn(e) => {
                // The competing grab that made us give up has ended: the
                // server reports its end as a focus-in with mode Ungrab. A
                // virtual-keyboard stand-down is not lifted by that.
                if e.mode == NotifyMode::UNGRAB && self.stand_down == Some(StandDown::Refused) {
                    self.stand_down = None;
                }
                self.refresh()
            }
            Event::MapNotify(_) | Event::ReparentNotify(_) => self.refresh(),
            _ => Ok(()),
        }
    }

    fn on_key(&mut self, device: u16, source: u16, keycode: u32, down: bool, repeat: bool) {
        if device != self.master {
            return;
        }
        let virtual_keyboard = !self.accept_virtual_keyboards && self.xtest.contains(&source);
        if !self.grabbed {
            // We only receive key events through the grab, so this one was
            // delivered (and taken from everyone else) before our ungrab
            // landed. An XTEST key of the stand-down is replayed like the one
            // that triggered it; others are not ours to forward any more.
            if virtual_keyboard && self.stand_down == Some(StandDown::VirtualKeyboard) {
                self.replay(keycode, down, false);
            }
            return;
        }
        let pressed_before_grab = self.down_at_grab.contains(&keycode);
        if virtual_keyboard {
            // A virtual key held since before the grab reached the remote
            // through GTK; its release is ours now and can be replayed to no
            // one (the key is already up), so the trait's owed key-up is sent
            // here. A stray key-up is harmless and cannot loop.
            if pressed_before_grab && !down {
                if let Some(code) = dom_code(keycode) {
                    (self.forward)(code, false);
                }
            }
            tracing::info!(
                "keys from a virtual (XTEST) keyboard: shortcut capture paused until the \
                 session window is focused again"
            );
            self.stand_down = Some(StandDown::VirtualKeyboard);
            // The ungrab goes out inside the replay's server grab: see `replay`.
            self.replay(keycode, down, true);
            return;
        }
        if pressed_before_grab {
            if down && repeat {
                // Its press went to the remote through GTK; repeats did too.
                return;
            }
            self.down_at_grab.retain(|&k| k != keycode);
        }
        let Some(code) = dom_code(keycode) else {
            return;
        };
        if down {
            // A repeat of a key whose press we never saw is not a new press:
            // the remote would get a key-down with no history.
            if repeat && !self.held.contains(code) {
                return;
            }
            self.held.press(code);
            (self.forward)(code, true);
        } else if self.held.release(code) || pressed_before_grab {
            (self.forward)(code, false);
        }
    }

    /// Give a key event the grab took back to the desktop through XTEST, after
    /// the ungrab, so the focused window ends up with a press/release pair and
    /// the key's state is what the sender left it at.
    ///
    /// WHY it reads the key state first: XTEST's `FakeInput` acts on the XTEST
    /// device's key state, which the taken event already changed. A press
    /// replayed onto a key still down is dropped (modifiers) or turned into an
    /// auto-repeat; a release onto a key already up is dropped. So the replay
    /// follows the server's state right after the ungrab ([`replay_plan`]).
    ///
    /// WHY under a server grab: between reading the state and faking the
    /// events, another XTEST client (the sender itself) could change it; a
    /// release landing in that gap would leave our replayed press stuck down.
    /// `GrabServer` holds every other client's requests for the few requests
    /// of the replay; the pending ungrab is sent inside the same grab.
    ///
    /// `ungrab`: drop our keyboard grab first, inside the same server grab
    /// (the stand-down itself); `false` for events taken before an ungrab that
    /// already happened.
    fn replay(&mut self, keycode: u32, down: bool, ungrab: bool) {
        if !down {
            // A taken release is never replayed: by the time we see it the key
            // is up (a release onto it would be dropped) or pressed anew (a
            // release would cut that press). When its press was taken too, the
            // press's replay already included a release.
            if ungrab {
                self.release();
            }
            return;
        }
        let _ = self.conn.grab_server();
        if ungrab {
            self.drop_grab();
        }
        let outcome = self.replay_press(keycode);
        // Whatever the replay did: the server must never stay grabbed. (A send
        // that failed means the connection is gone, and the server drops a
        // dead client's grab itself.)
        let _ = self.conn.ungrab_server();
        let _ = self.conn.flush();
        if let Err(e) = outcome {
            tracing::debug!("shortcut capture: could not replay a virtual key: {e}");
        }
    }

    /// The press half of [`replay`](Self::replay), run inside the server grab.
    fn replay_press(&self, keycode: u32) -> Result<(), String> {
        let key = u8::try_from(keycode).map_err(|_| format!("keycode {keycode} out of range"))?;
        // Its round trip is ordered after the ungrab sent before it.
        let still_down = self
            .conn
            .query_keymap()
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply()
            .map_err(x11_err("key state query"))
            .map(|r| keycodes_down(&r.keys).contains(&keycode))?;
        for &kind in replay_plan(still_down) {
            // Device 0 = the core keyboard, like the sender's own events.
            self.conn
                .xtest_fake_input(kind, key, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(|e| format!("X11 request failed: {e}"))?;
        }
        Ok(())
    }

    /// Re-read focus and visibility from the server and take or drop the grab
    /// to match.
    fn refresh(&mut self) -> Result<(), String> {
        let eligible = self.session_focused()? && self.video_viewable()?;
        if !eligible {
            self.stand_down = None;
            self.retry_until = None;
            self.release();
            return Ok(());
        }
        if self.grabbed || self.stand_down.is_some() {
            return Ok(());
        }
        let status = self
            .conn
            .xinput_xi_grab_device(
                self.video,
                CURRENT_TIME,
                NONE,
                self.master,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                // Our client owns no windows the keys could go to; report
                // everything to the grab window.
                GrabOwner::NO_OWNER,
                &[u32::from(XIEventMask::KEY_PRESS | XIEventMask::KEY_RELEASE)],
            )
            .map_err(|e| format!("X11 grab request failed: {e}"))?
            .reply()
            .map_err(x11_err("keyboard grab"))?
            .status;
        match GrabResult::of(status) {
            GrabResult::Grabbed => {
                self.grabbed = true;
                self.retry_until = None;
                // Read after the grab is ours: a key that goes down later is
                // delivered to us, so nothing falls between the two.
                let keys = self
                    .conn
                    .query_keymap()
                    .map_err(|e| format!("X11 request failed: {e}"))?
                    .reply()
                    .map_err(x11_err("key state query"))?
                    .keys;
                self.down_at_grab = keycodes_down(&keys);
            }
            GrabResult::NotViewable => self.retry_until = None,
            GrabResult::Busy(why) => {
                let now = Instant::now();
                let until = *self.retry_until.get_or_insert(now + self.patience);
                if now >= until {
                    // During `install` the caller logs the reason itself.
                    if self.patience == RUN_PATIENCE {
                        tracing::warn!("shortcut capture cannot take the keyboard: {why}");
                    }
                    self.retry_until = None;
                    self.stand_down = Some(StandDown::Refused);
                    self.refusal = why;
                }
            }
        }
        Ok(())
    }

    /// Drop the grab and send the remote a key-up for everything still held:
    /// the real releases will land in whatever window has the focus now.
    fn release(&mut self) {
        self.drop_grab();
        let _ = self.conn.flush();
    }

    /// [`release`](Self::release) without the flush, for a caller that sends
    /// more requests in the same batch.
    fn drop_grab(&mut self) {
        if std::mem::take(&mut self.grabbed) {
            let _ = self.conn.xinput_xi_ungrab_device(CURRENT_TIME, self.master);
        }
        // Keys down since before the grab release into GTK again from here.
        self.down_at_grab.clear();
        for code in self.held.drain() {
            (self.forward)(code, false);
        }
    }

    fn session_focused(&self) -> Result<bool, String> {
        let focus = self
            .conn
            .get_input_focus()
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply()
            .map_err(x11_err("focus query"))?
            .focus;
        if focus == NONE || focus == u32::from(InputFocus::POINTER_ROOT) || focus == self.root {
            return Ok(false);
        }
        is_self_or_descendant(&self.conn, focus, self.parent)
    }

    fn video_viewable(&self) -> Result<bool, String> {
        let attrs = self
            .conn
            .get_window_attributes(self.video)
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply();
        match attrs {
            Ok(a) => Ok(a.map_state == MapState::VIEWABLE),
            Err(e) if is_bad_window(&e) => Err("the video window no longer exists".into()),
            Err(e) => Err(format!("X11 window query failed: {e}")),
        }
    }
}

impl Drop for X11Grab {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Hand every event to `run` until shut down or the connection fails.
fn pump(conn: &RustConnection, tx: &Sender<Result<Event, String>>, shutdown: &AtomicBool) {
    loop {
        let event = conn.wait_for_event();
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let failed = event.is_err();
        let sent = tx.send(event.map_err(|e| format!("X11 connection lost: {e}")));
        if failed || sent.is_err() {
            return;
        }
    }
}

fn is_bad_window(e: &ReplyError) -> bool {
    matches!(e, ReplyError::X11Error(x) if x.error_kind == ErrorKind::Window)
}

/// Whether this X server is XWayland (see the module docs). `session_server`
/// = we dialled the session's `$DISPLAY`, so the session's `WAYLAND_DISPLAY`
/// speaks for it (Xwayland older than 23.1 has no `XWAYLAND` extension).
fn is_xwayland(conn: &RustConnection, session_server: bool) -> Result<bool, String> {
    let announced = conn
        .query_extension(b"XWAYLAND")
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply()
        .map_err(x11_err("extension query"))?
        .present;
    Ok(announced || (session_server && std::env::var_os("WAYLAND_DISPLAY").is_some()))
}

/// The parent of `window`; `None` when it has none or no longer exists.
fn parent_of(conn: &RustConnection, window: Window) -> Result<Option<Window>, String> {
    let tree = conn
        .query_tree(window)
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply();
    match tree {
        Ok(t) if t.parent == NONE => Ok(None),
        Ok(t) => Ok(Some(t.parent)),
        Err(e) if is_bad_window(&e) => Ok(None),
        Err(e) => Err(format!("X11 window tree query failed: {e}")),
    }
}

/// The XTEST events that replay a taken key press, given whether the key is
/// still down right after the ungrab.
fn replay_plan(still_down: bool) -> &'static [u8] {
    if still_down {
        // Still held (Synergy, VNC, a relayed modifier): a fresh press onto a
        // held key would be dropped or become a repeat, so release it first
        // (a stray release is harmless) and press it again; its real release
        // then reaches the window ungrabbed.
        &[KEY_RELEASE_EVENT, KEY_PRESS_EVENT]
    } else {
        // Already up: its release was taken by the grab too (it arrives later
        // and is not replayed), or reached the window before the server grab
        // (then the window gets one extra tap, never a stuck key).
        &[KEY_PRESS_EVENT, KEY_RELEASE_EVENT]
    }
}

/// Keycodes set in a `QueryKeymap` bit vector: keycode `8 * byte + bit`.
fn keycodes_down(keys: &[u8; 32]) -> Vec<u32> {
    let mut down = Vec::new();
    for (byte, &bits) in (0u32..).zip(keys.iter()) {
        for bit in 0..8 {
            if bits & (1 << bit) != 0 {
                down.push(byte * 8 + bit);
            }
        }
    }
    down
}

/// Whether `window` is `ancestor` or lies inside it. A window that vanishes
/// mid-walk is not inside anything.
fn is_self_or_descendant(
    conn: &RustConnection,
    mut window: Window,
    ancestor: Window,
) -> Result<bool, String> {
    for _ in 0..MAX_TREE_DEPTH {
        if window == ancestor {
            return Ok(true);
        }
        let tree = conn
            .query_tree(window)
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply();
        match tree {
            Ok(t) if t.parent == NONE || t.parent == t.root => return Ok(t.parent == ancestor),
            Ok(t) => window = t.parent,
            Err(e) if is_bad_window(&e) => return Ok(false),
            Err(e) => return Err(format!("X11 window tree query failed: {e}")),
        }
    }
    Ok(false)
}

/// The master keyboard paired with this client's pointer: the keyboard the
/// user types into GTK with.
fn master_keyboard(conn: &RustConnection) -> Result<u16, String> {
    let pointer = conn
        .xinput_xi_get_client_pointer(NONE)
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply()
        .map_err(x11_err("client pointer query"))?
        .deviceid;
    let masters = conn
        .xinput_xi_query_device(u16::from(xinput::Device::ALL_MASTER))
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply()
        .map_err(x11_err("input device query"))?
        .infos;
    masters
        .iter()
        .find(|d| d.deviceid == pointer && d.type_ == DeviceType::MASTER_POINTER)
        .map(|d| d.attachment)
        .or_else(|| {
            masters
                .iter()
                .find(|d| d.type_ == DeviceType::MASTER_KEYBOARD)
                .map(|d| d.deviceid)
        })
        .ok_or_else(|| "the X server reports no keyboard".to_string())
}

/// Slave keyboards the server marks as XTEST devices.
fn xtest_keyboards(conn: &RustConnection) -> Result<Vec<u16>, String> {
    let atom = conn
        .intern_atom(true, b"XTEST Device")
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply()
        .map_err(x11_err("atom lookup"))?
        .atom;
    // only_if_exists: no such atom means the server never created an XTEST
    // device.
    if atom == NONE {
        return Ok(Vec::new());
    }
    let devices = conn
        .xinput_xi_query_device(u16::from(xinput::Device::ALL))
        .map_err(|e| format!("X11 request failed: {e}"))?
        .reply()
        .map_err(x11_err("input device query"))?
        .infos;
    let mut xtest = Vec::new();
    for device in devices.iter().filter(|d| {
        matches!(
            d.type_,
            DeviceType::SLAVE_KEYBOARD | DeviceType::FLOATING_SLAVE
        )
    }) {
        let prop = conn
            .xinput_xi_get_property(device.deviceid, false, atom, u32::from(AtomEnum::ANY), 0, 1)
            .map_err(|e| format!("X11 request failed: {e}"))?
            .reply();
        match prop {
            Ok(p) if property_is_true(&p.items) => xtest.push(device.deviceid),
            Ok(_) => {}
            // The device went away between the two requests.
            Err(ReplyError::X11Error(_)) => {}
            Err(e) => return Err(format!("X11 device property query failed: {e}")),
        }
    }
    Ok(xtest)
}

/// A boolean device property: present with a non-zero first item.
fn property_is_true(items: &XIGetPropertyItems) -> bool {
    match items {
        XIGetPropertyItems::Data8(v) => v.first().is_some_and(|&b| b != 0),
        XIGetPropertyItems::Data16(v) => v.first().is_some_and(|&b| b != 0),
        XIGetPropertyItems::Data32(v) => v.first().is_some_and(|&b| b != 0),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grab_status_classification() {
        assert_eq!(GrabResult::of(GrabStatus::SUCCESS), GrabResult::Grabbed);
        assert_eq!(
            GrabResult::of(GrabStatus::NOT_VIEWABLE),
            GrabResult::NotViewable
        );
        for busy in [
            GrabStatus::ALREADY_GRABBED,
            GrabStatus::FROZEN,
            GrabStatus::INVALID_TIME,
        ] {
            assert!(matches!(GrabResult::of(busy), GrabResult::Busy(_)));
        }
        assert!(matches!(
            GrabResult::of(GrabStatus::ALREADY_GRABBED),
            GrabResult::Busy(why) if why.contains("AlreadyGrabbed")
        ));
        assert!(matches!(
            GrabResult::of(GrabStatus::FROZEN),
            GrabResult::Busy(why) if why.contains("Frozen")
        ));
    }

    #[test]
    fn replay_leaves_the_key_as_the_sender_left_it() {
        // Held: release first (a press onto a held key would be dropped or
        // repeat), then press; the real release comes later.
        assert_eq!(replay_plan(true), &[KEY_RELEASE_EVENT, KEY_PRESS_EVENT]);
        // Up: a whole tap, ending up.
        assert_eq!(replay_plan(false), &[KEY_PRESS_EVENT, KEY_RELEASE_EVENT]);
    }

    #[test]
    fn keymap_bits_name_keycodes() {
        let mut keys = [0u8; 32];
        keys[8] = 0b0000_0001; // keycode 64, Alt_L
        keys[16] = 0b0010_0000; // keycode 133, Super_L
        keys[31] = 0b1000_0000; // keycode 255
        assert_eq!(keycodes_down(&keys), vec![64, 133, 255]);
        assert!(keycodes_down(&[0; 32]).is_empty());
    }

    #[test]
    fn boolean_device_property() {
        assert!(property_is_true(&XIGetPropertyItems::Data8(vec![1])));
        assert!(property_is_true(&XIGetPropertyItems::Data32(vec![1, 0])));
        assert!(!property_is_true(&XIGetPropertyItems::Data8(vec![0])));
        assert!(!property_is_true(&XIGetPropertyItems::Data16(vec![])));
    }
}
