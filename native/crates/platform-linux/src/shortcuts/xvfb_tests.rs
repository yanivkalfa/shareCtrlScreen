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

use crate::test_xserver as xserver;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use platform_api::ShortcutHook;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, InputFocus, ModMask, Window, WindowClass, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

use super::tests::{xcb, TestSurface};
use super::LinuxShortcutHook;
use xserver::XServer;

const SUPER_L: u8 = 133;
const ALT_L: u8 = 64;
const TAB: u8 = 23;
const WAIT: Duration = Duration::from_secs(5);

/// A private Xvfb for one test, from the crate's shared test-server harness
/// (display numbers, readiness, the WSLg abstract-socket case). `None` = not
/// installed: the test is skipped.
///
/// `-noreset`: an X server resets when its last client leaves and refuses
/// whoever connects during the reset; tests here open and close clients
/// freely.
fn xvfb() -> Option<XServer> {
    if !xserver::installed("Xvfb") {
        return None;
    }
    XServer::start(
        "Xvfb",
        &["-noreset", "-screen", "0", "640x480x24"],
        None,
        false,
    )
}

/// A test client on `display`, through the same connect the hook uses.
fn connect(display: &str) -> (RustConnection, usize) {
    match crate::x11_connect(display) {
        Ok(c) => c,
        Err(e) => panic!("connect to {display}: {e}"),
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
        let (conn, screen) = connect(display);
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

    /// Keycodes the server has down right now.
    fn keys_down(&self) -> Vec<usize> {
        let keys = self
            .conn
            .query_keymap()
            .unwrap_or_else(|e| panic!("keymap: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("keymap: {e}"))
            .keys;
        (0..256)
            .filter(|&k| keys[k / 8] & (1 << (k % 8)) != 0)
            .collect()
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
        let (conn, _) = connect(display);
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

/// A client watching the key events the app window gets (GTK's view).
struct Observer {
    conn: RustConnection,
}

impl Observer {
    fn new(display: &str, window: Window) -> Self {
        let (conn, _) = connect(display);
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
        )
        .unwrap_or_else(|e| panic!("select: {e}"));
        conn.get_input_focus()
            .unwrap_or_else(|e| panic!("sync: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("sync: {e}"));
        Self { conn }
    }

    /// `(keycode, is_press)` received since the last call.
    fn events(&self) -> Vec<(u8, bool)> {
        self.conn
            .get_input_focus()
            .unwrap_or_else(|e| panic!("sync: {e}"))
            .reply()
            .unwrap_or_else(|e| panic!("sync: {e}"));
        let mut got = Vec::new();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                Event::KeyPress(e) => got.push((e.detail, true)),
                Event::KeyRelease(e) => got.push((e.detail, false)),
                _ => {}
            }
        }
        got
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
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    let wm = WindowManager::new(&x.name, desk.root);

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

    let hook = Running::start(&x.name, desk.video, desk.parent, true);
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
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    let (other, _) = connect(&x.name);
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

    let hook = Running::start(&x.name, desk.video, desk.parent, true);
    assert!(
        !hook.installed,
        "AlreadyGrabbed must report install == false"
    );
}

#[test]
fn install_refuses_a_video_window_outside_the_app_window() {
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    let hook = Running::start(&x.name, desk.elsewhere, desk.parent, true);
    assert!(!hook.installed);
}

#[test]
fn virtual_keyboard_input_is_not_forwarded() {
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    let wm = WindowManager::new(&x.name, desk.root);
    let hook = Running::start(&x.name, desk.video, desk.parent, false);
    assert!(hook.installed);
    assert!(desk.keyboard_grabbed());

    // Production path: an XTEST key (possibly our own injector's) is never sent
    // to the remote, and the grab steps aside so the next ones reach the
    // desktop normally.
    desk.tap(SUPER_L);
    assert!(hook.take(0).is_empty(), "XTEST keys are not forwarded");
    wait_until("grab steps aside", || !desk.keyboard_grabbed());
    // The tap the grab took is replayed after the ungrab: not lost.
    let mut seen = Vec::new();
    wait_until("the first tap reaches the WM", || {
        seen.extend(wm.pressed());
        seen.contains(&SUPER_L)
    });
    assert_eq!(seen, vec![SUPER_L]);
    desk.tap(SUPER_L);
    assert_eq!(wm.pressed(), vec![SUPER_L]);

    // A new focus period grabs again.
    desk.focus(desk.elsewhere);
    desk.focus(desk.parent);
    wait_until("grab on focus-in", || desk.keyboard_grabbed());
}

#[test]
fn a_held_virtual_key_is_replayed_without_sticking() {
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    let gtk = Observer::new(&x.name, desk.parent);
    let hook = Running::start(&x.name, desk.video, desk.parent, false);
    assert!(hook.installed);
    assert!(desk.keyboard_grabbed());

    // A Synergy/VNC-style hold: the press is taken by the grab, and the
    // release is only sent once the grab has stepped aside.
    desk.key(SUPER_L, true);
    wait_until("grab steps aside", || !desk.keyboard_grabbed());
    desk.key(SUPER_L, false);

    // The window gets the press back, then the real release (possibly after
    // the replay's stray release).
    let mut seen = Vec::new();
    wait_until("press, then release, at the window", || {
        seen.extend(gtk.events());
        seen.ends_with(&[(SUPER_L, true), (SUPER_L, false)])
    });
    assert!(desk.keys_down().is_empty(), "no key left down");
    assert!(hook.take(0).is_empty(), "XTEST keys are not forwarded");
}

#[test]
fn a_virtual_key_held_when_the_grab_starts_is_released_on_the_remote() {
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    desk.focus(desk.elsewhere);
    // Production path: XTEST keys are not captured.
    let hook = Running::start(&x.name, desk.video, desk.parent, false);
    assert!(hook.installed);

    // Synergy / VNC / our injector holds Alt while the window is unfocused (in
    // the app GTK gets it and the remote sees AltLeft down); then the window
    // is focused, the grab is taken, and Alt's release lands in it.
    desk.key(ALT_L, true);
    desk.focus(desk.parent);
    wait_until("grab on focus-in", || desk.keyboard_grabbed());
    desk.key(ALT_L, false);
    assert_eq!(
        hook.take(1),
        vec![k("AltLeft", false)],
        "the owed key-up, and nothing else"
    );
    wait_until("grab steps aside", || !desk.keyboard_grabbed());
    assert!(desk.keys_down().is_empty(), "no key left down");
}

#[test]
fn a_key_held_when_the_grab_starts_is_released_on_the_remote() {
    let Some(x) = xvfb() else { return };
    let desk = Desk::new(&x.name);
    desk.focus(desk.elsewhere);
    let hook = Running::start(&x.name, desk.video, desk.parent, true);
    assert!(hook.installed, "not focused: installed, waiting");
    assert!(!desk.keyboard_grabbed());

    // Alt goes down before the grab (in the app it would reach the remote
    // through GTK and the surface); its release lands in the grab.
    desk.key(ALT_L, true);
    desk.focus(desk.parent);
    wait_until("grab on focus-in", || desk.keyboard_grabbed());
    desk.key(ALT_L, false);
    assert_eq!(hook.take(1), vec![k("AltLeft", false)], "no stuck Alt");
}
