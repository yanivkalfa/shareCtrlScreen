//! Injector tests: the shared front end and backend selection (pure), then each
//! backend against a real peer where one can run unattended:
//!
//! * XTest against a private `Xvfb` (never the developer's display);
//! * uinput against the kernel, read back through evdev (root or the `input`
//!   group; the device is grabbed so nothing reaches a running desktop);
//! * libei against an in-process EIS server built from `reis`'s server half;
//! * wlroots against a private headless `sway`.
//!
//! A test whose peer is missing prints a `SKIP:` / `skipping:` line naming
//! what is missing and returns; `SCT_X11_TESTS_REQUIRED=1` (Xvfb) and
//! `SCT_LINUX_INPUT_TESTS_REQUIRED=1` (uinput with evdev, sway) turn those
//! skips into failures, for the CI jobs that provide the peers (T3/T4). The
//! EIS test needs nothing and always runs; the portal path needs a real
//! GNOME/KDE session and is `#[ignore]`d.

use crate::test_xserver as xserver;

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use platform_api::{InputInjector, PermissionState, Rect};
use protocol::{Button, InputMsg};

use super::mapping::{Point, WheelSteps};
use super::uinput::UinputAccess;
use super::*;
use crate::x11_connect::x11_connect;
use crate::SessionType;
use xserver::XServer;

// ---- front end ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Pointer(Point),
    Button(Button, bool),
    Wheel(WheelSteps),
    Key(u16, bool),
    Focus(Point),
    Flush,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Call>>>);

impl Recorder {
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
    fn push(&self, c: Call) {
        self.0.lock().unwrap().push(c);
    }
}

impl Backend for Recorder {
    fn pointer(&mut self, at: Point) {
        self.push(Call::Pointer(at));
    }
    fn button(&mut self, button: Button, down: bool) {
        self.push(Call::Button(button, down));
    }
    fn wheel(&mut self, steps: WheelSteps) {
        self.push(Call::Wheel(steps));
    }
    fn key(&mut self, evdev: u16, down: bool) {
        self.push(Call::Key(evdev, down));
    }
    fn focus_window_at(&mut self, at: Point) -> bool {
        self.push(Call::Focus(at));
        true
    }
    fn flush(&mut self) {
        self.push(Call::Flush);
    }
}

fn recorded() -> (LinuxInjector, Recorder) {
    let rec = Recorder::default();
    (LinuxInjector::new(Box::new(rec.clone())), rec)
}

fn key(code: &str, down: bool) -> InputMsg {
    if down {
        InputMsg::KeyDown { code: code.into() }
    } else {
        InputMsg::KeyUp { code: code.into() }
    }
}

fn at(nx: f64, ny: f64, target: Option<Rect>) -> Point {
    Point { nx, ny, target }
}

const SCREEN: Rect = Rect {
    x: 0,
    y: 0,
    w: 1920,
    h: 1080,
};

#[test]
fn a_button_moves_first_even_when_already_held() {
    let (mut inj, rec) = recorded();
    inj.set_target_rect(Some(SCREEN));
    let down = InputMsg::ButtonDown {
        b: Button::Left,
        x: 0.25,
        y: 0.5,
    };
    inj.dispatch(&down);
    inj.dispatch(&down);
    let p = at(0.25, 0.5, Some(SCREEN));
    assert_eq!(
        rec.take(),
        vec![
            Call::Pointer(p),
            Call::Button(Button::Left, true),
            Call::Flush,
            Call::Pointer(p),
            Call::Flush,
        ]
    );
}

#[test]
fn key_repeat_is_one_press_and_releases_need_a_press() {
    let (mut inj, rec) = recorded();
    inj.dispatch(&key("KeyA", false));
    inj.dispatch(&key("KeyA", true));
    inj.dispatch(&key("KeyA", true));
    inj.dispatch(&key("KeyA", false));
    inj.dispatch(&key("Nonsense", true));
    assert_eq!(
        rec.take(),
        vec![
            Call::Flush,
            Call::Key(30, true),
            Call::Flush,
            Call::Flush,
            Call::Key(30, false),
            Call::Flush,
            Call::Flush,
        ]
    );
}

#[test]
fn release_all_releases_exactly_what_is_held() {
    let (mut inj, rec) = recorded();
    for msg in [
        key("ShiftLeft", true),
        key("KeyA", true),
        key("KeyB", true),
        key("KeyB", false),
        InputMsg::ButtonDown {
            b: Button::Right,
            x: 0.0,
            y: 0.0,
        },
    ] {
        inj.dispatch(&msg);
    }
    rec.take();
    inj.release_all();
    assert_eq!(
        rec.take(),
        vec![
            Call::Key(42, false),
            Call::Key(30, false),
            Call::Button(Button::Right, false),
            Call::Flush,
        ],
        "keys in fixed order, no pointer move before the button"
    );
    inj.release_all();
    assert_eq!(rec.take(), vec![Call::Flush], "nothing is held twice");
}

#[test]
fn wheel_accumulates_and_skips_empty_messages() {
    let (mut inj, rec) = recorded();
    inj.dispatch(&InputMsg::Wheel { dx: 0, dy: 0 });
    inj.dispatch(&InputMsg::Wheel { dx: 0, dy: 60 });
    inj.dispatch(&InputMsg::Wheel { dx: 0, dy: 60 });
    let calls = rec.take();
    assert_eq!(calls[0], Call::Flush, "an empty wheel is not forwarded");
    assert_eq!(
        calls[1],
        Call::Wheel(WheelSteps {
            v120_y: 60,
            ..WheelSteps::default()
        })
    );
    assert_eq!(
        calls[3],
        Call::Wheel(WheelSteps {
            v120_y: 60,
            notches_y: 1,
            ..WheelSteps::default()
        })
    );
}

#[test]
fn degenerate_target_rects_mean_primary() {
    let (mut inj, rec) = recorded();
    inj.set_target_rect(Some(Rect {
        x: 5,
        y: 5,
        w: 0,
        h: 10,
    }));
    inj.dispatch(&InputMsg::Move { x: 0.5, y: 0.5 });
    assert_eq!(rec.take()[0], Call::Pointer(at(0.5, 0.5, None)));
    assert!(inj.focus_window_at(0.1, 0.2));
    assert_eq!(rec.take(), vec![Call::Focus(at(0.1, 0.2, None))]);
}

// ---- selection ---------------------------------------------------------------

fn caps(session: SessionType) -> Capabilities {
    Capabilities {
        session,
        xtest: false,
        wlr_virtual_input: false,
        portal_remote_desktop: None,
        uinput: UinputAccess::Missing("module not loaded".into()),
    }
}

#[test]
fn x11_prefers_xtest_and_falls_back_to_uinput() {
    let mut c = caps(SessionType::X11);
    c.xtest = true;
    assert_eq!(select(&c), Ok(vec![BackendKind::XTest]));
    c.uinput = UinputAccess::Writable;
    assert_eq!(
        select(&c),
        Ok(vec![BackendKind::XTest, BackendKind::Uinput])
    );
    c.xtest = false;
    assert_eq!(select(&c), Ok(vec![BackendKind::Uinput]));
    // Wayland-only routes mean nothing in an X11 session.
    c.uinput = UinputAccess::Missing("x".into());
    c.wlr_virtual_input = true;
    c.portal_remote_desktop = Some(2);
    let refusal = select(&c).unwrap_err();
    assert!(refusal.message.contains("XTEST"), "{refusal:?}");
}

#[test]
fn wayland_order_is_wlroots_portal_uinput() {
    let mut c = caps(SessionType::Wayland);
    c.wlr_virtual_input = true;
    c.portal_remote_desktop = Some(2);
    c.uinput = UinputAccess::Writable;
    assert_eq!(
        select(&c),
        Ok(vec![
            BackendKind::Wlroots,
            BackendKind::Portal,
            BackendKind::Uinput
        ])
    );
    c.wlr_virtual_input = false;
    assert_eq!(
        select(&c),
        Ok(vec![BackendKind::Portal, BackendKind::Uinput])
    );
    c.xtest = true;
    assert_eq!(
        select(&c).map(|k| k[0]),
        Ok(BackendKind::Portal),
        "XWayland's XTest only reaches X clients"
    );
}

#[test]
fn a_version_1_portal_is_not_a_route() {
    let mut c = caps(SessionType::Wayland);
    c.portal_remote_desktop = Some(1);
    let refusal = select(&c).unwrap_err();
    assert!(refusal.message.contains("version 1"), "{refusal:?}");
    assert!(!refusal.permission, "nothing the user can grant fixes this");
    c.portal_remote_desktop = Some(3);
    assert_eq!(select(&c), Ok(vec![BackendKind::Portal]));
}

#[test]
fn refusals_name_the_uinput_fix_and_map_to_the_right_error() {
    let mut c = caps(SessionType::Wayland);
    c.uinput = UinputAccess::Denied("log out and back in".into());
    let refusal = select(&c).unwrap_err();
    assert!(refusal.permission);
    assert!(refusal.message.contains("log out and back in"));
    assert!(refusal.message.contains("no remote-control portal"));
    assert!(matches!(
        InputError::from(refusal),
        InputError::PermissionDenied(_)
    ));
    c.uinput = UinputAccess::Missing("modprobe".into());
    assert!(matches!(
        InputError::from(select(&c).unwrap_err()),
        InputError::Unavailable(_)
    ));
}

// ---- helpers for live peers --------------------------------------------------

/// A missing peer (uinput access, evdev, sway) is a skip on a developer box
/// and a failure where `SCT_LINUX_INPUT_TESTS_REQUIRED=1` says the run must
/// exercise it (the CI jobs of T3/T4). Missing Xvfb follows the X11 tests'
/// own switch, `SCT_X11_TESTS_REQUIRED=1` ([`xserver::skip`]).
fn skip_input(why: &str) {
    if std::env::var_os("SCT_LINUX_INPUT_TESTS_REQUIRED").is_some_and(|v| v == "1") {
        panic!("{why}, and SCT_LINUX_INPUT_TESTS_REQUIRED=1");
    }
    eprintln!("SKIP: {why}");
}

fn tool_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// A child process killed on drop.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn eventually(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

// ---- XTest on a private Xvfb ---------------------------------------------------

/// A private Xvfb on a display number nobody else holds (the shared harness
/// checks the socket file, the lock file and the abstract socket, so it never
/// shadows WSLg's `:0`), reached with `x11_connect` like production does.
struct Xvfb(XServer);

impl Xvfb {
    fn start() -> Option<Self> {
        if !xserver::installed("Xvfb") {
            return None;
        }
        XServer::start(
            "Xvfb",
            &["-screen", "0", "1280x720x24", "+extension", "XTEST"],
            None,
            false,
        )
        .map(|server| {
            // Shown with --nocapture: proves the tests never run on a desktop's `:0`.
            eprintln!("input tests: private Xvfb on {}", server.name);
            Self(server)
        })
    }

    fn connect(&self) -> (x11_client::RustConnection, usize) {
        x11_connect(&self.0.name).expect("the private Xvfb")
    }

    fn xtest(&self) -> xtest::XTest {
        xtest::XTest::connect(Some(&self.0.name)).expect("XTest on the private Xvfb")
    }
}

mod x11_client {
    pub use x11rb::connection::Connection;
    pub use x11rb::protocol::xproto::{
        AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask, InputFocus,
        PropMode, Window, WindowClass,
    };
    pub use x11rb::protocol::Event;
    pub use x11rb::rust_connection::RustConnection;
    pub use x11rb::wrapper::ConnectionExt as _;
    pub use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

    pub fn window(
        conn: &RustConnection,
        parent: Window,
        (x, y, w, h): (i16, i16, u16, u16),
        mask: EventMask,
    ) -> Window {
        let id = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            id,
            parent,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(mask),
        )
        .unwrap();
        conn.map_window(id).unwrap();
        id
    }

    pub fn sync(conn: &RustConnection) {
        conn.get_input_focus().unwrap().reply().unwrap();
    }
}

#[test]
fn xtest_events_reach_an_x_client() {
    use x11_client::*;
    let Some(x) = Xvfb::start() else { return };
    let (conn, screen) = x.connect();
    let root = conn.setup().roots[screen].root;
    let win = window(
        &conn,
        root,
        (100, 100, 200, 200),
        EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION,
    );
    conn.set_input_focus(InputFocus::PARENT, win, CURRENT_TIME)
        .unwrap();
    sync(&conn);

    let backend = x.xtest();
    let mut inj = LinuxInjector::new(Box::new(backend));
    inj.set_target_rect(Some(Rect {
        x: 100,
        y: 100,
        w: 200,
        h: 200,
    }));
    for msg in [
        InputMsg::Move { x: 0.5, y: 0.5 },
        InputMsg::ButtonDown {
            b: Button::Left,
            x: 0.5,
            y: 0.5,
        },
        InputMsg::ButtonUp {
            b: Button::Left,
            x: 0.5,
            y: 0.5,
        },
        InputMsg::Wheel { dx: 0, dy: 120 },
        key("KeyA", true),
        key("KeyA", true),
        key("KeyA", false),
        key("ShiftLeft", true),
        InputMsg::ButtonDown {
            b: Button::Right,
            x: 0.5,
            y: 0.5,
        },
    ] {
        inj.dispatch(&msg);
    }
    inj.release_all();

    // (kind, detail) of every key/button event, plus window-relative motion.
    let mut seen: Vec<(&str, u8)> = Vec::new();
    let mut moved_to_centre = false;
    eventually("the injected events", Duration::from_secs(5), || {
        while let Some(event) = conn.poll_for_event().unwrap() {
            match event {
                Event::MotionNotify(e) => {
                    moved_to_centre |= e.event == win && (e.event_x, e.event_y) == (100, 100);
                }
                Event::ButtonPress(e) => seen.push(("press", e.detail)),
                Event::ButtonRelease(e) => seen.push(("release", e.detail)),
                Event::KeyPress(e) => seen.push(("key", e.detail)),
                Event::KeyRelease(e) => seen.push(("keyup", e.detail)),
                _ => {}
            }
        }
        seen.len() >= 10
    });
    assert!(moved_to_centre, "the pointer reached the target's centre");
    assert_eq!(
        seen,
        vec![
            ("press", 1),
            ("release", 1),
            ("press", 4),
            ("release", 4),
            ("key", 38),
            ("keyup", 38),
            ("key", 50),
            ("press", 3),
            ("keyup", 50),
            ("release", 3),
        ],
        "KeyA is keycode 38 and pressed once despite the repeat; wheel up is button 4; \
         release_all lets go of Shift and the right button"
    );
}

#[test]
fn xtest_focus_without_a_window_manager_sets_the_focus() {
    use x11_client::*;
    let Some(x) = Xvfb::start() else { return };
    let (conn, screen) = x.connect();
    let root = conn.setup().roots[screen].root;
    let left = window(&conn, root, (0, 0, 100, 100), EventMask::NO_EVENT);
    let right = window(&conn, root, (600, 300, 100, 100), EventMask::NO_EVENT);
    conn.set_input_focus(InputFocus::PARENT, left, CURRENT_TIME)
        .unwrap();
    sync(&conn);

    let mut inj = LinuxInjector::new(Box::new(x.xtest()));
    assert!(inj.focus_window_at(650.0 / 1280.0, 350.0 / 720.0));
    assert_eq!(
        conn.get_input_focus().unwrap().reply().unwrap().focus,
        right
    );
    assert!(
        !inj.focus_window_at(1000.0 / 1280.0, 10.0 / 720.0),
        "no window there, only the root"
    );
}

#[test]
fn xtest_focus_asks_an_ewmh_window_manager_for_the_client_window() {
    use x11_client::*;
    let Some(x) = Xvfb::start() else { return };
    let (conn, screen) = x.connect();
    let root = conn.setup().roots[screen].root;
    // A reparenting WM's frame with the application window inside it.
    let frame = window(&conn, root, (200, 200, 300, 300), EventMask::NO_EVENT);
    let client = window(&conn, frame, (10, 30, 280, 260), EventMask::NO_EVENT);
    let intern = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
    let (active, supported, wm_state) = (
        intern(b"_NET_ACTIVE_WINDOW"),
        intern(b"_NET_SUPPORTED"),
        intern(b"WM_STATE"),
    );
    conn.change_property32(PropMode::REPLACE, client, wm_state, wm_state, &[1, 0])
        .unwrap();
    conn.change_property32(
        PropMode::REPLACE,
        root,
        supported,
        AtomEnum::ATOM,
        &[active],
    )
    .unwrap();
    // Become the window manager: client messages to the root come to us.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_REDIRECT),
    )
    .unwrap();
    sync(&conn);

    let mut inj = LinuxInjector::new(Box::new(x.xtest()));
    inj.set_target_rect(Some(Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 720,
    }));
    assert!(inj.focus_window_at(300.0 / 1280.0, 300.0 / 720.0));
    let mut request = None;
    eventually("_NET_ACTIVE_WINDOW", Duration::from_secs(5), || {
        while let Some(event) = conn.poll_for_event().unwrap() {
            if let Event::ClientMessage(m) = event {
                request = Some(m);
            }
        }
        request.is_some()
    });
    let m = request.unwrap();
    assert_eq!(m.type_, active);
    assert_eq!(m.window, client, "the WM_STATE window, not the frame");
    assert_eq!(
        m.data.as_data32()[0],
        2,
        "source: pager / direct user action"
    );
}

// ---- uinput, read back through evdev ----------------------------------------

#[test]
fn uinput_frames_reach_evdev() {
    use input_linux::sys::input_event;
    use input_linux::{EvdevHandle, EventKind, InputEvent};
    use std::os::unix::fs::OpenOptionsExt;

    let access = uinput::access();
    if access != UinputAccess::Writable {
        skip_input(&format!(
            "uinput read-back needs write access to /dev/uinput ({})",
            access.explain()
        ));
        return;
    }
    // Reading back needs the evdev handler (WSL2's kernel has uinput but no
    // evdev, so its devices get no /dev/input/event* node).
    let handlers = std::fs::read_to_string("/proc/bus/input/handlers").unwrap_or_default();
    if !handlers.contains("Name=evdev") {
        skip_input("uinput read-back: this kernel has no evdev handler (no /dev/input/event*)");
        return;
    }
    let backend = uinput::Uinput::with_geometry(None, None).expect("uinput devices");
    let (kbd_path, ptr_path) = backend.evdev_paths().expect("evdev nodes in sysfs");
    let open = |path: &Path| {
        let mut file = None;
        let what = format!(
            "{} to open (read access to /dev/input/event* needs root or the input group)",
            path.display()
        );
        eventually(&what, Duration::from_secs(5), || {
            file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .open(path)
                .ok();
            file.is_some()
        });
        let handle = EvdevHandle::new(file.unwrap());
        // Exclusive: the events must not reach a desktop running on this
        // machine (a developer's session would get a stray "a").
        handle.grab(true).expect("EVIOCGRAB");
        handle
    };
    let (kbd, ptr) = (open(&kbd_path), open(&ptr_path));
    let mut inj = LinuxInjector::new(Box::new(backend));
    inj.dispatch(&key("KeyA", true));
    inj.dispatch(&key("KeyA", false));
    inj.dispatch(&InputMsg::Wheel { dx: 0, dy: 120 });
    inj.dispatch(&InputMsg::Move { x: 0.5, y: 0.5 });

    let read_all = |dev: &EvdevHandle<std::fs::File>, want: usize| {
        let mut got: Vec<(EventKind, u16, i32)> = Vec::new();
        eventually("evdev events", Duration::from_secs(5), || {
            let mut buf: [input_event; 16] = [InputEvent::zeroed().into_raw(); 16];
            if let Ok(n) = dev.read(&mut buf) {
                for raw in &buf[..n] {
                    let e = InputEvent::with_raw(*raw).unwrap();
                    got.push((e.kind, e.code, e.value));
                }
            }
            got.len() >= want
        });
        got
    };
    let syn = (EventKind::Synchronize, 0, 0);
    assert_eq!(
        read_all(&kbd, 4),
        vec![(EventKind::Key, 30, 1), syn, (EventKind::Key, 30, 0), syn]
    );
    assert_eq!(
        read_all(&ptr, 6),
        vec![
            (EventKind::Relative, 8, 1),    // REL_WHEEL
            (EventKind::Relative, 11, 120), // REL_WHEEL_HI_RES
            syn,
            (EventKind::Absolute, 0, 32768), // ABS_X at half of 0..=65535
            (EventKind::Absolute, 1, 32768),
            syn,
        ]
    );
}

// ---- libei against an in-process EIS server -----------------------------------

/// A minimal EIS server, the compositor's side, on a blocking thread: one seat,
/// an absolute pointer (with one 1920x1080 region) and a keyboard as separate
/// devices (mutter's layout). Reports what the client sends, one line each.
fn eis_server(socket: UnixStream, report: mpsc::Sender<String>) {
    use reis::eis;
    use reis::handshake::EisHandshaker;
    use reis::request::{DeviceCapability, EisRequest, EisRequestConverter};

    let ctx = eis::Context::new(socket).unwrap();
    let read = |ctx: &eis::Context| loop {
        match ctx.read() {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return false,
        }
    };
    let mut handshaker = EisHandshaker::new(&ctx, 1);
    let resp = 'hs: loop {
        assert!(read(&ctx), "client left during the handshake");
        while let Some(r) = ctx.pending_request() {
            let reis::PendingRequestResult::Request(r) = r else {
                panic!("bad handshake request")
            };
            if let Some(resp) = handshaker.handle_request(r).unwrap() {
                break 'hs resp;
            }
        }
    };
    let mut conv = EisRequestConverter::new(&ctx, resp, 1);
    let _seat = conv.handle().add_seat(
        Some("seat0"),
        DeviceCapability::PointerAbsolute
            | DeviceCapability::Button
            | DeviceCapability::Scroll
            | DeviceCapability::Keyboard,
    );
    conv.handle().flush().unwrap();
    let mut devices = Vec::new();
    loop {
        if !read(&ctx) {
            return;
        }
        while let Some(r) = ctx.pending_request() {
            let reis::PendingRequestResult::Request(r) = r else {
                continue;
            };
            conv.handle_request(r).unwrap();
        }
        while let Some(request) = conv.next_request() {
            let line = match request {
                EisRequest::Bind(b) => {
                    let pointer = b.seat.add_device(
                        Some("pointer"),
                        eis::device::DeviceType::Virtual,
                        DeviceCapability::PointerAbsolute
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll,
                        |d| d.device().region(0, 0, 1920, 1080, 1.0),
                    );
                    let keyboard = b.seat.add_device(
                        Some("keyboard"),
                        eis::device::DeviceType::Virtual,
                        DeviceCapability::Keyboard.into(),
                        |_| {},
                    );
                    pointer.resumed();
                    keyboard.resumed();
                    devices.push(pointer);
                    devices.push(keyboard);
                    continue;
                }
                EisRequest::DeviceStartEmulating(_) => "start".to_owned(),
                EisRequest::DeviceStopEmulating(_) => "stop".to_owned(),
                EisRequest::PointerMotionAbsolute(m) => {
                    format!("abs {} {}", m.dx_absolute, m.dy_absolute)
                }
                EisRequest::Button(b) => format!("button {} {:?}", b.button, b.state),
                EisRequest::ScrollDiscrete(s) => {
                    format!("scroll {} {}", s.discrete_dx, s.discrete_dy)
                }
                EisRequest::KeyboardKey(k) => format!("key {} {:?}", k.key, k.state),
                EisRequest::Frame(_) => continue,
                EisRequest::Disconnect => return,
                other => format!("other {other:?}"),
            };
            if report.send(line).is_err() {
                return;
            }
        }
        conv.handle().flush().unwrap();
    }
}

#[test]
fn libei_sender_talks_to_an_eis_server() {
    let (client, server) = UnixStream::pair().unwrap();
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || eis_server(server, tx));
    let (backend, client_thread) = libei::Libei::on_socket(client);
    let mut inj = LinuxInjector::new(Box::new(backend));
    inj.set_target_rect(Some(SCREEN));

    let next = || rx.recv_timeout(Duration::from_secs(5)).expect("EIS report");
    assert_eq!((next(), next()), ("start".into(), "start".into()));
    for msg in [
        InputMsg::Move { x: 0.5, y: 0.25 },
        InputMsg::ButtonDown {
            b: Button::Left,
            x: 0.5,
            y: 0.25,
        },
        InputMsg::ButtonUp {
            b: Button::Left,
            x: 0.5,
            y: 0.25,
        },
        InputMsg::Wheel { dx: 0, dy: 120 },
        key("KeyA", true),
        key("KeyA", true),
        key("KeyA", false),
        key("ShiftLeft", true),
    ] {
        inj.dispatch(&msg);
    }
    let expected = [
        "abs 960 270",
        "abs 960 270",
        "button 272 Press",
        "abs 960 270",
        "button 272 Released",
        "scroll 0 -120",
        "key 30 Press",
        "key 30 Released",
        "key 42 Press",
    ];
    for want in expected {
        assert_eq!(next(), want);
    }
    // Dropping the injector ends the task, which lets go of Shift and stops
    // emulating before it leaves.
    drop(inj);
    assert_eq!(next(), "key 42 Released");
    assert_eq!((next(), next()), ("stop".into(), "stop".into()));
    assert_eq!(client_thread.join().expect("client thread joins"), Ok(()));
    server.join().unwrap();
}

// ---- wlroots against a private headless sway --------------------------------

struct Sway {
    _child: Reaped,
    dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Sway {
    fn start() -> Option<Self> {
        if !tool_available("sway") || !tool_available("swaymsg") {
            skip_input("the wlroots test needs sway and swaymsg installed");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        // No XWayland: a killed sway then leaves no X lock file or socket.
        std::fs::write(
            &config,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x720\n",
        )
        .unwrap();
        let child = Reaped(
            Command::new("sway")
                .arg("-c")
                .arg(&config)
                .env("XDG_RUNTIME_DIR", dir.path())
                .env("WLR_BACKENDS", "headless")
                .env("WLR_RENDERER", "pixman")
                .env("WLR_LIBINPUT_NO_DEVICES", "1")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DISPLAY")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("sway starts"),
        );
        let mut socket = None;
        eventually("sway's Wayland socket", Duration::from_secs(10), || {
            socket = std::fs::read_dir(dir.path()).ok().and_then(|entries| {
                entries.flatten().map(|e| e.path()).find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                })
            });
            socket.is_some() && sway_ipc(dir.path()).is_some()
        });
        Some(Self {
            _child: child,
            socket: socket.unwrap(),
            dir,
        })
    }

    fn msg(&self, args: &[&str]) -> String {
        let ipc = sway_ipc(self.dir.path()).expect("sway IPC socket");
        let out = Command::new("swaymsg")
            .env("SWAYSOCK", ipc)
            .args(args)
            .output()
            .expect("swaymsg runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn sway_ipc(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("sway-ipc."))
        })
}

#[test]
fn wlroots_virtual_devices_join_the_seat() {
    let Some(sway) = Sway::start() else { return };
    let conn = wayland_client::Connection::from_socket(UnixStream::connect(&sway.socket).unwrap())
        .unwrap();
    let mut backend = wlr::Wlr::with_connection(conn).expect("wlroots virtual input");
    // The headless seat has no keyboard, so this exercised the default keymap.
    backend.pointer(at(0.5, 0.5, None));
    backend.button(Button::Left, true);
    backend.button(Button::Left, false);
    backend.wheel(WheelSteps {
        v120_y: 120,
        notches_y: 1,
        ..WheelSteps::default()
    });
    backend.key(42, true);
    backend.key(30, true);
    backend.key(30, false);
    backend.key(42, false);
    backend.flush();
    backend.sync().expect("no protocol error from sway");

    let inputs = sway.msg(&["-t", "get_inputs", "--raw"]);
    assert!(inputs.contains("\"type\": \"pointer\""), "{inputs}");
    assert!(inputs.contains("\"type\": \"keyboard\""), "{inputs}");
    assert!(
        inputs.contains("English (US)"),
        "sway compiled our default keymap: {inputs}"
    );
}

// ---- the real portal ---------------------------------------------------------

/// Run inside a GNOME 46+ or KDE Plasma 6 session: shows the consent dialog
/// (answer it) and moves the pointer to the screen centre. See
/// plans/checklists/L6.md.
#[test]
#[ignore = "needs a GNOME 46+ / KDE Plasma 6 session with the RemoteDesktop portal"]
fn portal_libei_moves_the_real_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let report = crate::permissions::report(SessionType::Wayland, dir.path());
    assert_ne!(
        report.input_injection,
        PermissionState::Denied,
        "{report:?}"
    );
    let mut inj = create(SessionType::Wayland, dir.path()).expect("an injector");
    std::thread::sleep(Duration::from_secs(20));
    inj.dispatch(&InputMsg::Move { x: 0.5, y: 0.5 });
    std::thread::sleep(Duration::from_secs(1));
    inj.release_all();
}
