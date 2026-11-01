//! The L7 acceptance test: on a private Xvfb, while the hook holds its grab,
//! Super and Alt+Tab reach the hook (and so the remote) and **not** a window
//! manager's passive grabs for them; focus-out, unmap and a competing grab
//! behave as the trait requires. Skipped cleanly when `Xvfb` is not installed.
//!
//! Each test starts its own Xvfb: keyboard grabs are server-wide, so tests
//! sharing one server would steal each other's keys.
//!
//! Keys are typed with XTEST, the only input Xvfb has. Production ignores XTEST
//! keys (they may be our own injector's), so the delivery tests turn that off
//! and [`virtual_keyboard_input_is_not_forwarded`] checks the production path.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use platform_api::ShortcutHook;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, GrabMode, GrabStatus, InputFocus, ModMask, Window,
    WindowClass, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

use super::tests::{xcb, TestSurface};
use super::LinuxShortcutHook;

const SUPER_L: u8 = 133;
const ALT_L: u8 = 64;
const TAB: u8 = 23;
const WAIT: Duration = Duration::from_secs(5);

/// A private X server for one test.
struct Xvfb {
    child: Child,
    display: String,
}

impl Xvfb {
    /// `None` when Xvfb is not installed (the test is then skipped).
    fn start() -> Option<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = 300 + std::process::id() % 400;
        for _ in 0..32 {
            let n = base + NEXT.fetch_add(1, Ordering::SeqCst);
            if Path::new(&format!("/tmp/.X{n}-lock")).exists() {
                continue;
            }
            let (display, listen) = if unix_sockets_work() {
                (format!(":{n}"), ["-nolisten", "tcp"])
            } else {
                (format!("127.0.0.1:{n}"), ["-listen", "tcp"])
            };
            let server = format!(":{n}");
            let spawned = Command::new("Xvfb")
                .args([server.as_str(), listen[0], listen[1]])
                // -noreset: an X server resets when its last client leaves,
                // and drops whoever connects mid-reset; the readiness probe
                // below is such a last client.
                .args(["-noreset", "-screen", "0", "640x480x24"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match spawned {
                Ok(child) => child,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("skipping: Xvfb is not installed (apt-get install xvfb)");
                    return None;
                }
                Err(e) => panic!("cannot start Xvfb: {e}"),
            };
            let deadline = Instant::now() + WAIT;
            while Instant::now() < deadline {
                if RustConnection::connect(Some(&display)).is_ok() {
                    return Some(Self { child, display });
                }
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break; // display number taken after all; try the next
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("could not start Xvfb on any display number");
    }
}

/// Whether a server can listen in `/tmp/.X11-unix`. Under WSLg that directory
/// is a read-only mount holding WSLg's own socket, so Xvfb only gets its
/// Linux abstract socket, which x11rb does not dial; the tests then use TCP on
/// loopback instead (X access control still admits only local clients).
fn unix_sockets_work() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WORKS.get_or_init(|| {
        let probe = format!("/tmp/.X11-unix/sct-probe-{}", std::process::id());
        let ok = std::os::unix::net::UnixListener::bind(&probe).is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    })
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The app's windows as the test client created them: a toplevel with the
/// video window inside, plus another toplevel to move the focus to.
struct Desk {
    conn: RustConnection,
    root: Window,
    parent: Window,
    video: Window,
    elsewhere: Window,
}

impl Desk {
    fn new(display: &str) -> Self {
        let (conn, screen) = match RustConnection::connect(Some(display)) {
            Ok(c) => c,
            Err(e) => panic!("connect: {e}"),
        };
        let root = conn.setup().roots[screen].root;
        let window = |parent, w, h| {
            let id = conn.generate_id().unwrap_or_else(|e| panic!("id: {e}"));
            conn.create_window(
                COPY_DEPTH_FROM_PARENT,
                id,
                parent,
                0,
                0,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap_or_else(|e| panic!("create window: {e}"));
            id
        };
        let parent = window(root, 320, 240);
        let video = window(parent, 300, 200);
        let elsewhere = window(root, 100, 100);
        for w in [parent, video, elsewhere] {
            conn.map_window(w).unwrap_or_else(|e| panic!("map: {e}"));
        }
        let desk = Self {
            conn,
            root,
            parent,
            video,
            elsewhere,
        };
        desk.focus(parent);
        desk
    }

    fn sync(&self) {
        self.conn
            .get_input_focus()
            .unwrap_or_else(|e| panic!("sync: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("sync: {e}"));
    }

    fn focus(&self, window: Window) {
        self.conn
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)
            .unwrap_or_else(|e| panic!("focus: {e}"));
        self.sync();
    }

    fn set_video_mapped(&self, mapped: bool) {
        let r = if mapped {
            self.conn.map_window(self.video)
        } else {
            self.conn.unmap_window(self.video)
        };
        r.unwrap_or_else(|e| panic!("map/unmap: {e}"));
        self.sync();
    }

    /// Type through XTEST, as a person on this keyboard would.
    fn key(&self, keycode: u8, down: bool) {
        let kind = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.conn
            .xtest_fake_input(kind, keycode, CURRENT_TIME, self.root, 0, 0, 0)
            .unwrap_or_else(|e| panic!("xtest: {e}"));
        self.sync();
    }

    fn tap(&self, keycode: u8) {
        self.key(keycode, true);
        self.key(keycode, false);
    }

    /// Whether any client holds an active keyboard grab: try one ourselves.
    fn keyboard_grabbed(&self) -> bool {
        let status = self
            .conn
            .grab_keyboard(
                false,
                self.root,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap_or_else(|e| panic!("grab: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("grab: {e}"))
            .status;
        if status == GrabStatus::SUCCESS {
            self.conn
                .ungrab_keyboard(CURRENT_TIME)
                .unwrap_or_else(|e| panic!("ungrab: {e}"));
            self.sync();
            return false;
        }
        true
    }
}

/// A window manager stand-in: passive grabs for Super and Alt+Tab on the root,
/// as GNOME/KDE/Openbox set them.
struct WindowManager {
    conn: RustConnection,
}

impl WindowManager {
    fn new(display: &str, root: Window) -> Self {
        let (conn, _) = match RustConnection::connect(Some(display)) {
            Ok(c) => c,
            Err(e) => panic!("connect: {e}"),
        };
        for (key, mods) in [(SUPER_L, ModMask::ANY), (TAB, ModMask::M1)] {
            conn.grab_key(false, root, mods, key, GrabMode::ASYNC, GrabMode::ASYNC)
                .unwrap_or_else(|e| panic!("grab_key: {e}"));
        }
        let wm = Self { conn };
        wm.pressed();
        wm
    }

    /// Keycodes pressed into our grabs since the last call.
    fn pressed(&self) -> Vec<u8> {
        // A round trip guarantees everything the server sent before it is read.
        self.conn
            .get_input_focus()
            .unwrap_or_else(|e| panic!("sync: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("sync: {e}"));
        let mut keys = Vec::new();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if let Event::KeyPress(e) = event {
                keys.push(e.detail);
            }
        }
        keys
    }
}

/// The hook on its own thread, the way the engine runs it.
struct Running {
    installed: bool,
    keys: Receiver<(String, bool)>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    fn start(display: &str, video: Window, parent: Window, accept_virtual: bool) -> Self {
        let (key_tx, keys) = mpsc::channel();
        let (installed_tx, installed_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (display, stop) = (display.to_string(), stop.clone());
            std::thread::spawn(move || {
                let mut hook = LinuxShortcutHook {
                    active: None,
                    x11_display: Some(display),
                    accept_virtual_keyboards: accept_virtual,
                };
                let surface = TestSurface(Some(xcb(video, parent)));
                let forward = Box::new(move |code: &str, down: bool| {
                    let _ = key_tx.send((code.to_string(), down));
                });
                let installed = hook.install(&surface, forward);
                let _ = installed_tx.send(installed);
                if installed {
                    hook.run(&stop);
                }
                hook.uninstall();
            })
        };
        let installed = installed_rx.recv_timeout(WAIT).unwrap_or(false);
        Self {
            installed,
            keys,
            stop,
            thread: Some(thread),
        }
    }

    /// Exactly `n` forwarded keys (waiting up to [`WAIT`]), then nothing more
    /// for a moment.
    fn take(&self, n: usize) -> Vec<(String, bool)> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while got.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.keys.recv_timeout(left) {
                Ok(k) => got.push(k),
                Err(_) => break,
            }
        }
        while let Ok(k) = self.keys.recv_timeout(Duration::from_millis(150)) {
            got.push(k);
        }
        got
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn k(code: &str, down: bool) -> (String, bool) {
    (code.to_string(), down)
}

#[test]
fn grabbed_shortcuts_reach_the_hook_not_the_window_manager() {
    let Some(x) = Xvfb::start() else { return };
    let desk = Desk::new(&x.display);
    let wm = WindowManager::new(&x.display, desk.root);

    // Control: without the hook the window manager gets Super and Alt+Tab.
    desk.tap(SUPER_L);
    desk.key(ALT_L, true);
    desk.tap(TAB);
    desk.key(ALT_L, false);
    assert_eq!(
        wm.pressed(),
        vec![SUPER_L, TAB],
        "test setup: WM grabs work"
    );

    let hook = Running::start(&x.display, desk.video, desk.parent, true);
    assert!(hook.installed, "install on a focused, mapped window");
    assert!(
        desk.keyboard_grabbed(),
        "focused at install: grabbed at once"
    );

    desk.tap(SUPER_L);
    desk.key(ALT_L, true);
    desk.tap(TAB);
    desk.key(ALT_L, false);
    assert_eq!(
        hook.take(6),
        vec![
            k("MetaLeft", true),
            k("MetaLeft", false),
            k("AltLeft", true),
            k("Tab", true),
            k("Tab", false),
            k("AltLeft", false),
        ]
    );
    assert!(wm.pressed().is_empty(), "the WM saw none of them");

    // Focus moves away mid-combo: the held key is released on the remote and
    // the keyboard goes back to the desktop.
    desk.key(ALT_L, true);
    assert_eq!(hook.take(1), vec![k("AltLeft", true)]);
    desk.focus(desk.elsewhere);
    assert_eq!(hook.take(1), vec![k("AltLeft", false)], "stuck-key guard");
    wait_until("grab released on focus-out", || !desk.keyboard_grabbed());
    desk.key(ALT_L, false);
    desk.tap(SUPER_L);
    assert_eq!(wm.pressed(), vec![SUPER_L]);
    assert!(hook.take(0).is_empty(), "nothing forwarded while unfocused");

    // Focus back: grabbed again. Video hidden (unmapped): released, so a modal
    // can be typed into. Shown again: grabbed.
    desk.focus(desk.parent);
    wait_until("grab on focus-in", || desk.keyboard_grabbed());
    desk.set_video_mapped(false);
    wait_until("grab released on unmap", || !desk.keyboard_grabbed());
    desk.tap(SUPER_L);
    assert_eq!(wm.pressed(), vec![SUPER_L]);
    desk.set_video_mapped(true);
    wait_until("grab on map", || desk.keyboard_grabbed());
    desk.tap(SUPER_L);
    assert_eq!(
        hook.take(2),
        vec![k("MetaLeft", true), k("MetaLeft", false)]
    );
    assert!(wm.pressed().is_empty());

    drop(hook);
    assert!(!desk.keyboard_grabbed(), "uninstall releases the grab");
}

#[test]
fn install_reports_false_while_another_client_holds_the_keyboard() {
    let Some(x) = Xvfb::start() else { return };
    let desk = Desk::new(&x.display);
    let (other, _) = match RustConnection::connect(Some(&x.display)) {
        Ok(c) => c,
        Err(e) => panic!("connect: {e}"),
    };
    let status = other
        .grab_keyboard(
            false,
            desk.root,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .unwrap_or_else(|e| panic!("grab: {e}"))
        .reply()
        .unwrap_or_else(|e| panic!("grab: {e}"))
        .status;
    assert_eq!(status, GrabStatus::SUCCESS);

    let hook = Running::start(&x.display, desk.video, desk.parent, true);
    assert!(
        !hook.installed,
        "AlreadyGrabbed must report install == false"
    );
}

#[test]
fn install_refuses_a_video_window_outside_the_app_window() {
    let Some(x) = Xvfb::start() else { return };
    let desk = Desk::new(&x.display);
    let hook = Running::start(&x.display, desk.elsewhere, desk.parent, true);
    assert!(!hook.installed);
}

#[test]
fn virtual_keyboard_input_is_not_forwarded() {
    let Some(x) = Xvfb::start() else { return };
    let desk = Desk::new(&x.display);
    let wm = WindowManager::new(&x.display, desk.root);
    let hook = Running::start(&x.display, desk.video, desk.parent, false);
    assert!(hook.installed);
    assert!(desk.keyboard_grabbed());

    // Production path: an XTEST key (possibly our own injector's) is never sent
    // to the remote, and the grab steps aside so the next ones reach the
    // desktop normally.
    desk.tap(SUPER_L);
    assert!(hook.take(0).is_empty(), "XTEST keys are not forwarded");
    wait_until("grab steps aside", || !desk.keyboard_grabbed());
    desk.tap(SUPER_L);
    assert_eq!(wm.pressed(), vec![SUPER_L]);

    // A new focus period grabs again.
    desk.focus(desk.elsewhere);
    desk.focus(desk.parent);
    wait_until("grab on focus-in", || desk.keyboard_grabbed());
}
