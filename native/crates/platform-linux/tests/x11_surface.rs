//! The X11 video surface under a private Xvfb (plan task L5 acceptance, run by
//! the T3 CI job): presents 100 `Cpu(Nv12)` frames that really reach the
//! window, keeps `client_size` stable under the toolbar clip, and turns
//! scripted XTest pointer, wheel and key events into `VideoInput`. Also checks
//! the `native_handles` / hide-unmaps invariants the L7 shortcut hook needs.
//!
//! Skips (exit 0) when `Xvfb` is not installed (a failure under
//! `SCT_X11_TESTS_REQUIRED=1`). The scenario runs twice: with the default
//! backend choice (lavapipe Vulkan when installed) and again in a child
//! process with `WGPU_BACKEND=gl` + `LIBGL_ALWAYS_SOFTWARE=1`, where only the
//! GL/EGL backend exists — so the GL fallback (llvmpipe) passes the same
//! checks or the test fails.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod common;
// L2's shared private-X-server harness; this binary needs only `XServer`.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "support/xserver.rs"]
mod xserver;

/// Set in the child process that repeats the scenario on the GL backend.
#[cfg(target_os = "linux")]
const GL_PASS: &str = "SCT_L5_GL_PASS";

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let gl_pass = std::env::var_os(GL_PASS).is_some();
    let pass = if gl_pass { "gl" } else { "default backend" };
    let result = x11::run(pass).and_then(|ran| {
        // The child only repeats; the parent starts it once its own run
        // passed (and was not skipped).
        if gl_pass || !ran {
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let status = std::process::Command::new(exe)
            .env(GL_PASS, "1")
            .env("WGPU_BACKEND", "gl")
            .status()
            .map_err(|e| format!("cannot start the GL pass: {e}"))?;
        common::check(status.success(), format!("the GL pass failed ({status})"))
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("x11_surface ({pass}): FAILED: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::ffi::{c_void, CString};
    use std::ptr::NonNull;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    // `::x11`: the Xlib crate, not this test module.
    use ::x11::xlib;

    use gtk::prelude::*;
    use platform_api::{Platform, RawDisplayHandle, RawWindowHandle, VideoInput};
    use platform_linux::LinuxPlatform;
    use raw_window_handle::{XlibDisplayHandle, XlibWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::shape::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{
        Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
        ImageFormat, InputFocus, MapState, PropMode, SelectionNotifyEvent, Window, WindowClass,
        SELECTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::CURRENT_TIME;

    use crate::common::{self, check, Outcome};
    use crate::xserver::XServer;

    // X11 core event codes for XTest fake input.
    const KEY_PRESS: u8 = 2;
    const KEY_RELEASE: u8 = 3;
    const BUTTON_PRESS: u8 = 4;
    const BUTTON_RELEASE: u8 = 5;
    const MOTION_NOTIFY: u8 = 6;
    /// X keycode of the A key: evdev KEY_A (30) + 8.
    const KEYCODE_A: u8 = 38;

    struct Xt {
        conn: RustConnection,
        root: Window,
    }

    impl Xt {
        fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Outcome {
            self.conn
                .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)
                .map_err(|e| format!("XTest: {e}"))?;
            self.conn.flush().map_err(|e| format!("flush: {e}"))?;
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }

        fn move_to(&self, (x, y): (i16, i16)) -> Outcome {
            self.fake(MOTION_NOTIFY, 0, x, y)
        }

        /// `window`'s `(x, y)` in root coordinates.
        fn root_point(&self, window: Window, x: i16, y: i16) -> Result<(i16, i16), String> {
            let r = self
                .conn
                .translate_coordinates(window, self.root, x, y)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            Ok((r.dst_x, r.dst_y))
        }

        fn map_state(&self, window: Window) -> Result<MapState, String> {
            Ok(self
                .conn
                .get_window_attributes(window)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?
                .map_state)
        }

        fn bounding_rects(&self, window: Window) -> Result<Vec<(i16, i16, u16, u16)>, String> {
            let r = self
                .conn
                .shape_get_rectangles(window, shape::SK::BOUNDING)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            Ok(r.rectangles
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect())
        }

        fn atom(&self, name: &str) -> Result<Atom, String> {
            Ok(self
                .conn
                .intern_atom(false, name.as_bytes())
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?
                .atom)
        }

        /// The window `XdndProxy` on `toplevel` names, if any.
        fn proxy_of(&self, toplevel: Window) -> Result<Option<Window>, String> {
            let proxy = self.atom("XdndProxy")?;
            let r = self
                .conn
                .get_property(false, toplevel, proxy, AtomEnum::WINDOW, 0, 1)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            Ok(r.value32().and_then(|mut v| v.next()))
        }

        /// Act as an XDND source dragging `uri_list` onto `toplevel` at
        /// `root` coordinates, the way GTK and Qt do (through `XdndProxy`).
        /// Returns (proxy window, accepted, finished successfully).
        fn drag_file(
            &self,
            toplevel: Window,
            (x, y): (i16, i16),
            uri_list: &[u8],
        ) -> Result<(Window, bool, bool), String> {
            let e = |e: x11rb::errors::ConnectionError| e.to_string();
            let proxy = self
                .proxy_of(toplevel)?
                .ok_or("no XdndProxy on the app window")?;
            let [enter, position, status, drop, finished, selection, copy, list] = [
                "XdndEnter",
                "XdndPosition",
                "XdndStatus",
                "XdndDrop",
                "XdndFinished",
                "XdndSelection",
                "XdndActionCopy",
                "text/uri-list",
            ]
            .map(|n| self.atom(n));
            let (enter, position, status, drop, finished, selection, copy, list) = (
                enter?, position?, status?, drop?, finished?, selection?, copy?, list?,
            );
            let source = self.conn.generate_id().map_err(|e| e.to_string())?;
            self.conn
                .create_window(
                    0,
                    source,
                    self.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new(),
                )
                .map_err(e)?;
            self.conn
                .set_selection_owner(source, selection, CURRENT_TIME)
                .map_err(e)?;
            let send = |kind: Atom, data: [u32; 5]| -> Outcome {
                // Sent to the proxy, addressed to the toplevel (XDND §proxy).
                let msg = ClientMessageEvent::new(32, toplevel, kind, data);
                self.conn
                    .send_event(false, proxy, EventMask::NO_EVENT, msg)
                    .map_err(|e| e.to_string())?;
                self.conn.flush().map_err(|e| e.to_string())
            };
            send(enter, [source, 5 << 24, list, 0, 0])?;
            let point = (u32::from(x as u16) << 16) | u32::from(y as u16);
            send(position, [source, 0, point, CURRENT_TIME, copy])?;
            let mut accepted = None;
            let mut done = None;
            let deadline = Instant::now() + Duration::from_secs(5);
            while done.is_none() && Instant::now() < deadline {
                let Some(event) = self.conn.poll_for_event().map_err(e)? else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                match event {
                    Event::ClientMessage(m) if m.type_ == status && accepted.is_none() => {
                        let ok = m.data.as_data32()[1] & 1 == 1;
                        accepted = Some(ok);
                        if ok {
                            send(drop, [source, 0, CURRENT_TIME, 0, 0])?;
                        } else {
                            break;
                        }
                    }
                    Event::SelectionRequest(req) => {
                        self.conn
                            .change_property8(
                                PropMode::REPLACE,
                                req.requestor,
                                req.property,
                                req.target,
                                uri_list,
                            )
                            .map_err(e)?;
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: req.time,
                            requestor: req.requestor,
                            selection: req.selection,
                            target: req.target,
                            property: req.property,
                        };
                        self.conn
                            .send_event(false, req.requestor, EventMask::NO_EVENT, notify)
                            .map_err(e)?;
                        self.conn.flush().map_err(e)?;
                    }
                    Event::ClientMessage(m) if m.type_ == finished => {
                        done = Some(m.data.as_data32()[1] & 1 == 1);
                    }
                    _ => {}
                }
            }
            let _ = self.conn.destroy_window(source);
            Ok((proxy, accepted.unwrap_or(false), done.unwrap_or(false)))
        }

        /// The pixel at `(x, y)` of `window` as (r, g, b).
        fn pixel(&self, window: Window, x: i16, y: i16) -> Result<(u8, u8, u8), String> {
            let img = self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, window, x, y, 1, 1, !0)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            // 24-bit TrueColor ZPixmap on little-endian: B, G, R, pad.
            match img.data.as_slice() {
                [b, g, r, ..] => Ok((*r, *g, *b)),
                _ => Err("GetImage returned no pixel".into()),
            }
        }
    }

    /// The whole scenario. `Ok(false)` = skipped (no Xvfb).
    pub fn run(pass: &str) -> Result<bool, String> {
        // The shared private-Xvfb harness: a skip when Xvfb is missing, a
        // failure under SCT_X11_TESTS_REQUIRED=1 (the CI Linux job).
        let Some(xvfb) = XServer::start("Xvfb", &["-screen", "0", "1280x800x24"], None, false)
        else {
            println!("x11_surface: SKIPPED (Xvfb is not installed)");
            return Ok(false);
        };
        let name = xvfb.name.clone();
        // Before GTK (or anything else) reads the environment.
        std::env::set_var("DISPLAY", &name);
        std::env::set_var("GDK_BACKEND", "x11");
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::remove_var("WAYLAND_DISPLAY");
        gtk::init().map_err(|e| format!("gtk::init: {e}"))?;

        let (window, content) = common::app_window((900, 600))?;
        let gdk_window = window.window().ok_or("no GdkWindow")?;
        let toplevel = gdk_window
            .downcast_ref::<gdkx11::X11Window>()
            .ok_or("not an X11 window")?
            .xid() as u32;
        let parent = RawWindowHandle::Xlib(XlibWindowHandle::new(toplevel.into()));
        let platform = LinuxPlatform::new();
        // A handle without a display of its own is refused: GDK's display
        // must never reach wgpu (it is not thread-safe on libX11 < 1.8).
        let refused = platform.create_surface(
            parent,
            RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0)),
        );
        check(
            refused.is_err(),
            "a handle without an Xlib display was accepted",
        )?;
        // What tao does: a private Xlib connection for the handle, never closed.
        let cname = CString::new(name.clone()).map_err(|e| e.to_string())?;
        // SAFETY: a valid NUL-terminated display name; the Display is never
        // closed, so the pointer stays valid for the process.
        let xdisplay = unsafe { xlib::XOpenDisplay(cname.as_ptr()) };
        let xdisplay = NonNull::new(xdisplay.cast::<c_void>()).ok_or("XOpenDisplay failed")?;
        let display = RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(xdisplay), 0));
        let surface = platform
            .create_surface(parent, display)
            .map_err(|e| format!("create_surface: {e}"))?;
        common::pump_for(Duration::from_millis(100));

        let (conn, screen) =
            platform_linux::x11_connect(&name).map_err(|e| format!("test connection: {e}"))?;
        let xt = Xt {
            root: conn.setup().roots[screen].root,
            conn,
        };

        // --- native handles: the live child window under the app window.
        let handles = surface.native_handles().ok_or("native_handles is None")?;
        let RawWindowHandle::Xlib(video) = handles.video else {
            return Err(format!("video handle is not Xlib: {:?}", handles.video));
        };
        let video = u32::try_from(video.window).map_err(|e| e.to_string())?;
        check(
            matches!(handles.parent, RawWindowHandle::Xlib(p) if p.window == u64::from(toplevel)),
            "parent handle is not the app window",
        )?;
        check(
            matches!(handles.display, RawDisplayHandle::Xlib(d) if d.display.is_some()),
            "display handle has no Xlib display",
        )?;
        let tree = xt
            .conn
            .query_tree(video)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("the video window {video:#x} does not exist: {e}"))?;
        check(
            tree.parent == toplevel,
            "the video window is not a child of the app window",
        )?;
        check(
            xt.map_state(video)? == MapState::UNMAPPED,
            "the video window is mapped before show",
        )?;

        // --- client_size follows the content widget.
        let expected = (
            content.allocated_width() as u32 * window.scale_factor() as u32,
            content.allocated_height() as u32 * window.scale_factor() as u32,
        );
        check(
            surface.client_size() == expected,
            format!(
                "client_size {:?} != content {expected:?}",
                surface.client_size()
            ),
        )?;

        // --- 100 frames, really on screen.
        let presented = common::present_frames(&surface, 100)?;
        check(
            presented == 100,
            format!("{presented}/100 frames presented"),
        )?;
        check(
            xt.map_state(video)? == MapState::VIEWABLE,
            "the video window is not viewable after show",
        )?;
        common::pump_for(Duration::from_millis(200));
        let (w, h) = surface.client_size();
        let (r, g, b) = xt.pixel(video, (w / 2) as i16, (h / 2) as i16)?;
        check(
            r > 200 && g < 60 && b < 60,
            format!("the centre of the video is ({r},{g},{b}), not the red frame"),
        )?;

        // --- scripted input.
        let (tx, rx) = channel();
        surface.set_input_sink(Some(tx));
        xt.conn
            .set_input_focus(InputFocus::PARENT, toplevel, CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        let centre = xt.root_point(video, (w / 2) as i16, (h / 2) as i16)?;
        xt.move_to(centre)?;
        xt.fake(BUTTON_PRESS, 1, 0, 0)?;
        xt.fake(BUTTON_RELEASE, 1, 0, 0)?;
        xt.fake(BUTTON_PRESS, 3, 0, 0)?;
        xt.fake(BUTTON_RELEASE, 3, 0, 0)?;
        xt.fake(BUTTON_PRESS, 4, 0, 0)?;
        xt.fake(BUTTON_RELEASE, 4, 0, 0)?;
        xt.fake(BUTTON_PRESS, 5, 0, 0)?;
        xt.fake(BUTTON_RELEASE, 5, 0, 0)?;
        xt.fake(KEY_PRESS, KEYCODE_A, 0, 0)?;
        xt.fake(KEY_RELEASE, KEYCODE_A, 0, 0)?;
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
            events.contains(&VideoInput::Wheel { dx: 0, dy: 120 })
                && events.contains(&VideoInput::Wheel { dx: 0, dy: -120 }),
            format!("wheel notches missing in {events:?}"),
        )?;
        check(
            events.contains(&VideoInput::Key {
                code: "KeyA",
                down: true,
            }) && events.contains(&VideoInput::Key {
                code: "KeyA",
                down: false,
            }),
            format!("KeyA down/up missing in {events:?}"),
        )?;

        // --- toolbar clip: reveal at the top edge, client_size unchanged.
        let before = surface.client_size();
        xt.move_to(xt.root_point(video, (w / 2) as i16, 1)?)?;
        common::pump_for(Duration::from_millis(100));
        let rects = xt.bounding_rects(video)?;
        check(
            rects == vec![(0, 40, w as u16, h as u16 - 40)],
            format!("revealed shape is {rects:?}"),
        )?;
        check(
            surface.client_size() == before,
            "client_size changed under the clip",
        )?;
        // Deeper than 56 px hides the bar again: the whole window is back.
        xt.move_to(xt.root_point(video, (w / 2) as i16, 200)?)?;
        common::pump_for(Duration::from_millis(100));
        let rects = xt.bounding_rects(video)?;
        check(
            rects == vec![(0, 0, w as u16, h as u16)],
            format!("unrevealed shape is {rects:?}"),
        )?;
        check(
            surface.client_size() == before,
            "client_size changed after the clip",
        )?;

        // --- a file dropped on the video reaches the drop sink.
        let (dtx, drx) = channel();
        surface.set_drop_sink(Some(dtx));
        let dragged = xt.drag_file(toplevel, centre, b"file:///tmp/sharectrl%20drop.txt\r\n")?;
        check(
            dragged.0 == video,
            format!("XdndProxy is {:#x}, not the video", dragged.0),
        )?;
        check(dragged.1, "the video did not accept the drag")?;
        check(dragged.2, "the drop did not finish successfully")?;
        let dropped = drx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| format!("no FileDrop: {e}"))?;
        check(
            dropped.paths == vec![std::path::PathBuf::from("/tmp/sharectrl drop.txt")]
                && common::near(dropped.nx, 0.5)
                && common::near(dropped.ny, 0.5),
            format!("wrong drop: {dropped:?}"),
        )?;

        // --- hide unmaps (the L7 grab invariant) and stops input.
        surface.hide();
        check(
            xt.proxy_of(toplevel)?.is_none(),
            "XdndProxy still points at the hidden video",
        )?;
        check(
            xt.map_state(video)? == MapState::UNMAPPED,
            "hide did not unmap the video window",
        )?;
        common::collect(&rx, Duration::from_millis(100));
        xt.move_to(centre)?;
        xt.fake(BUTTON_PRESS, 1, 0, 0)?;
        xt.fake(BUTTON_RELEASE, 1, 0, 0)?;
        let hidden = common::collect(&rx, Duration::from_millis(300));
        check(
            !hidden
                .iter()
                .any(|e| matches!(e, VideoInput::Button { .. })),
            format!("input arrived while hidden: {hidden:?}"),
        )?;
        surface.show();
        check(
            xt.map_state(video)? == MapState::VIEWABLE,
            "show did not map the video window again",
        )?;

        // --- clearing the sink releases what is held.
        xt.fake(KEY_PRESS, KEYCODE_A, 0, 0)?;
        let held = common::collect(&rx, Duration::from_millis(300));
        check(
            held.contains(&VideoInput::Key {
                code: "KeyA",
                down: true,
            }),
            format!("KeyA down missing after show: {held:?}"),
        )?;
        surface.set_input_sink(None);
        let released: Vec<VideoInput> = rx.try_iter().collect();
        check(
            released.contains(&VideoInput::Key {
                code: "KeyA",
                down: false,
            }),
            format!("clearing the sink did not release KeyA: {released:?}"),
        )?;
        xt.fake(KEY_RELEASE, KEYCODE_A, 0, 0)?;

        drop(surface);
        common::pump_for(Duration::from_millis(100));
        println!("x11_surface ({pass}): ok (100 frames, readback, input, clip, drop, hide/show)");
        Ok(true)
    }
}
