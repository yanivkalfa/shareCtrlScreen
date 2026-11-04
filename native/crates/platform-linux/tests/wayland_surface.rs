//! The Wayland video subsurface under a private headless sway (run by the T4
//! CI job): presents 100 `Cpu(Nv12)` frames, turns real seat pointer events
//! (a wlroots virtual pointer) into `VideoInput`, crops the toolbar strip out
//! of hit-testing without changing `client_size`, and stops taking input when
//! hidden. Keys are not scripted here: wlroots virtual keyboards upload their
//! own keymaps (spike finding 4), and the key path is shared with X11, whose
//! test scripts real keycodes.
//!
//! Skips (exit 0) when `sway` is not installed, unless
//! `SCT_WAYLAND_TESTS_REQUIRED=1` (the T4 job) makes that a failure. File
//! drops are not scripted: a drag needs a real drag source with an implicit
//! grab, which no headless tool provides (device checklist L5 §6).

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod common;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match wayland::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wayland_surface: FAILED: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
mod wayland {
    use std::ffi::c_void;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::ptr::NonNull;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    use gtk::glib::translate::ToGlibPtr;
    use gtk::prelude::*;
    use platform_api::{Platform, RawDisplayHandle, RawWindowHandle, VideoInput};
    use platform_linux::LinuxPlatform;
    use raw_window_handle::{WaylandDisplayHandle, WaylandWindowHandle};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

    use crate::common::{self, check, Outcome, Server};

    /// The headless output, logical px (scale 1).
    const OUTPUT: (u32, u32) = (1280, 800);
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;

    struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ZwlrVirtualPointerManagerV1);
    delegate_noop!(State: ZwlrVirtualPointerV1);

    /// A wlroots virtual pointer: real seat events, in output coordinates.
    struct Pointer {
        conn: Connection,
        queue: EventQueue<State>,
        pointer: ZwlrVirtualPointerV1,
        start: Instant,
    }

    impl Pointer {
        fn new() -> Result<Self, String> {
            let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
            let (globals, queue) =
                registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
            let qh = queue.handle();
            let manager: ZwlrVirtualPointerManagerV1 = globals
                .bind(&qh, 1..=2, ())
                .map_err(|e| format!("no wlr virtual pointer: {e}"))?;
            let pointer = manager.create_virtual_pointer(None, &qh, ());
            Ok(Self {
                conn,
                queue,
                pointer,
                start: Instant::now(),
            })
        }

        fn time(&self) -> u32 {
            self.start.elapsed().as_millis() as u32
        }

        fn send(&mut self) -> Outcome {
            self.pointer.frame();
            self.queue
                .roundtrip(&mut State)
                .map_err(|e| format!("roundtrip: {e}"))?;
            self.conn.flush().map_err(|e| format!("flush: {e}"))?;
            std::thread::sleep(Duration::from_millis(30));
            Ok(())
        }

        fn move_to(&mut self, x: u32, y: u32) -> Outcome {
            self.pointer
                .motion_absolute(self.time(), x, y, OUTPUT.0, OUTPUT.1);
            self.send()
        }

        fn click(&mut self, button: u32) -> Outcome {
            self.pointer
                .button(self.time(), button, ButtonState::Pressed);
            self.send()?;
            self.pointer
                .button(self.time(), button, ButtonState::Released);
            self.send()
        }

        /// One wheel notch towards the user.
        fn notch_down(&mut self) -> Outcome {
            self.pointer.axis_source(AxisSource::Wheel);
            self.pointer
                .axis_discrete(self.time(), Axis::VerticalScroll, 15.0, 1);
            self.send()
        }
    }

    /// A missing prerequisite: a skip normally, a failure when
    /// `SCT_WAYLAND_TESTS_REQUIRED=1` says this run must exercise everything
    /// (the Wayland CI job, T4) — the rule of `tests/support/xserver.rs`.
    fn skip(why: &str) -> Outcome {
        if std::env::var_os("SCT_WAYLAND_TESTS_REQUIRED").is_some_and(|v| v == "1") {
            return Err(format!("{why}, and SCT_WAYLAND_TESTS_REQUIRED=1"));
        }
        println!("wayland_surface: SKIPPED ({why})");
        Ok(())
    }

    fn start_sway(runtime: &Path) -> Result<Option<(Server, String)>, String> {
        let Some(sway) = common::find_program("sway") else {
            return Ok(None);
        };
        let config = runtime.join("sway.conf");
        std::fs::write(
            &config,
            format!(
                "output HEADLESS-1 resolution {}x{} scale 1\ndefault_border none\n",
                OUTPUT.0, OUTPUT.1
            ),
        )
        .map_err(|e| format!("sway config: {e}"))?;
        let mut server = common::spawn(
            Command::new(sway)
                .arg("-c")
                .arg(&config)
                .env("XDG_RUNTIME_DIR", runtime)
                .env("WLR_BACKENDS", "headless")
                .env("WLR_RENDERER", "pixman")
                .env("WLR_LIBINPUT_NO_DEVICES", "1")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DISPLAY"),
        )?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(name) = socket_name(runtime) {
                return Ok(Some((server, name)));
            }
            if let Ok(Some(status)) = server.0.try_wait() {
                return Err(format!("sway exited early: {status}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("sway did not create its Wayland socket".into())
    }

    /// The `wayland-N` socket the compositor created in `runtime`.
    fn socket_name(runtime: &Path) -> Option<String> {
        std::fs::read_dir(runtime)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .find(|n| {
                n.strip_prefix("wayland-")
                    .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
            })
    }

    fn handles(window: &gtk::Window) -> Result<(RawWindowHandle, RawDisplayHandle), String> {
        let gdk_window = window.window().ok_or("no GdkWindow")?;
        let w: *mut gtk::gdk::ffi::GdkWindow = gdk_window.to_glib_none().0;
        // SAFETY: a live, mapped GdkWindow of the Wayland backend.
        let surface = unsafe { gdk_wayland_sys::gdk_wayland_window_get_wl_surface(w.cast()) };
        let display = gdk_window.display();
        let d: *mut gtk::gdk::ffi::GdkDisplay = display.to_glib_none().0;
        // SAFETY: GTK's live Wayland display.
        let wl_display = unsafe { gdk_wayland_sys::gdk_wayland_display_get_wl_display(d.cast()) };
        let surface = NonNull::new(surface.cast::<c_void>()).ok_or("no wl_surface")?;
        let wl_display = NonNull::new(wl_display.cast::<c_void>()).ok_or("no wl_display")?;
        Ok((
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(wl_display)),
        ))
    }

    pub fn run() -> Outcome {
        let runtime = tempfile::tempdir().map_err(|e| format!("runtime dir: {e}"))?;
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("runtime dir: {e}"))?;
        let Some((_sway, socket)) = start_sway(runtime.path())? else {
            return skip("sway is not installed");
        };
        // Before GTK (or anything else) reads the environment.
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        std::env::set_var("WAYLAND_DISPLAY", socket);
        std::env::set_var("GDK_BACKEND", "wayland");
        std::env::remove_var("DISPLAY");
        // The in-window layout is under test, not the fallback window.
        std::env::remove_var("SHARECTRL_VIDEO_WINDOW");
        gtk::init().map_err(|e| format!("gtk::init: {e}"))?;

        // sway tiles the only window over the whole output, so the content
        // (undecorated: at the surface origin) is at output (0, 0).
        let (window, content) = common::app_window((800, 600))?;
        let (parent, display) = handles(&window)?;
        let platform = LinuxPlatform::new();
        let surface = platform
            .create_surface(parent, display)
            .map_err(|e| format!("create_surface: {e}"))?;
        common::pump_for(Duration::from_millis(200));

        let handles = surface.native_handles().ok_or("native_handles is None")?;
        let same_parent = match (handles.parent, parent) {
            (RawWindowHandle::Wayland(a), RawWindowHandle::Wayland(b)) => a.surface == b.surface,
            _ => false,
        };
        check(same_parent, "parent handle is not the app window's surface")?;
        check(
            matches!(handles.video, RawWindowHandle::Wayland(_)),
            "video handle is not a Wayland surface",
        )?;

        let (cw, ch) = (
            content.allocated_width() as u32,
            content.allocated_height() as u32,
        );
        check(
            surface.client_size() == (cw, ch),
            format!(
                "client_size {:?} != content {cw}x{ch}",
                surface.client_size()
            ),
        )?;

        let presented = common::present_frames(&surface, 100)?;
        check(
            presented == 100,
            format!("{presented}/100 frames presented"),
        )?;

        let (tx, rx) = channel();
        surface.set_input_sink(Some(tx));
        let mut pointer = Pointer::new()?;
        // The 16:9 frame letterboxed into the content: the centre is (0.5, 0.5).
        pointer.move_to(cw / 2, ch / 2)?;
        pointer.click(BTN_LEFT)?;
        pointer.click(BTN_RIGHT)?;
        pointer.notch_down()?;
        let events = common::collect(&rx, Duration::from_millis(500));
        check(
            common::has_move(&events, 0.5, 0.5),
            format!("no centre move in {events:?}"),
        )?;
        for (button, down) in [(0, true), (0, false), (2, true), (2, false)] {
            check(
                common::has_button(&events, button, down),
                format!("button {button} down={down} missing in {events:?}"),
            )?;
        }
        check(
            events.contains(&VideoInput::Wheel { dx: 0, dy: -120 }),
            format!("wheel notch missing in {events:?}"),
        )?;

        // Task M15: the top-edge hover reveal is retired on Linux — the surface
        // is always full. A pointer at the very top edge therefore reaches the
        // video like anywhere else (no strip is cut out of hit-testing), and
        // client_size never changes.
        let before = surface.client_size();
        pointer.move_to(cw / 2, 1)?;
        let at_top = common::collect(&rx, Duration::from_millis(300));
        check(
            at_top.iter().any(|e| matches!(e, VideoInput::Move { .. })),
            format!("a top-edge move did not reach the full video: {at_top:?}"),
        )?;
        check(
            surface.client_size() == before,
            "client_size changed at the top edge (no reveal expected)",
        )?;
        pointer.move_to(cw / 2, 20)?;
        let near_top = common::collect(&rx, Duration::from_millis(300));
        check(
            near_top
                .iter()
                .any(|e| matches!(e, VideoInput::Move { .. })),
            format!("a near-top move did not reach the video: {near_top:?}"),
        )?;
        pointer.move_to(cw / 2, 300)?;
        let deep = common::collect(&rx, Duration::from_millis(300));
        check(
            deep.iter().any(|e| matches!(e, VideoInput::Move { .. })),
            format!("no input in the body of the video: {deep:?}"),
        )?;

        // Hidden: not hit-testable.
        surface.hide();
        common::pump_for(Duration::from_millis(100));
        pointer.move_to(cw / 2, ch / 2)?;
        pointer.click(BTN_LEFT)?;
        let hidden = common::collect(&rx, Duration::from_millis(300));
        check(
            !hidden
                .iter()
                .any(|e| matches!(e, VideoInput::Button { down: true, .. })),
            format!("input arrived while hidden: {hidden:?}"),
        )?;
        surface.show();
        common::pump_for(Duration::from_millis(100));
        pointer.move_to(cw / 2 + 10, ch / 2)?;
        let shown = common::collect(&rx, Duration::from_millis(300));
        check(
            shown.iter().any(|e| matches!(e, VideoInput::Move { .. })),
            format!("no input after show: {shown:?}"),
        )?;

        surface.set_input_sink(None);
        drop(surface);
        common::pump_for(Duration::from_millis(100));
        println!("wayland_surface: ok (100 frames, input, full surface, hide/show)");
        Ok(())
    }
}
