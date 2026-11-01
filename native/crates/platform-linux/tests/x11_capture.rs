//! X11 capture against real X servers (task L2 acceptance).
//!
//! Each test starts its own `Xvfb` (and, for the resize test, an `Xephyr`
//! nested in it) on its own display number (`support/xserver.rs`), so tests
//! run in parallel and never touch the developer's desktop.
//!
//! A test whose server binary (or `xterm`, `xrandr`) is missing prints why
//! and passes, so a developer box without X tools stays green. Set
//! `SCT_X11_TESTS_REQUIRED=1` (the CI Linux job does) to turn every such skip
//! into a failure. Packages: `xvfb xserver-xephyr x11-xserver-utils xterm`.
//!
//! Why Xephyr for the resize: Xvfb's RandR offers exactly one mode, so
//! `xrandr` cannot change its size; Xephyr implements RandR 1.0 size changes
//! (`xrandr -s`) like a real driver.
//!
//! Servers are reached over Unix sockets only (the file socket, or the
//! abstract one where `/tmp/.X11-unix` is read-only, as under WSLg — see
//! `platform_linux::x11_connect`), so every capture here must use MIT-SHM.

#![cfg(target_os = "linux")]

#[path = "support/xserver.rs"]
mod xserver;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use platform_api::{CaptureError, CapturedFrame, FrameData, PixelFormat, Rect, ScreenCapture};
use platform_linux::{x11_connect, x11_displays, X11Capture};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{ConnectionExt as _, MonitorInfo};
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, MapState, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};
use xserver::{installed, XServer};

const RED: [u8; 4] = [0, 0, 255, 255];

/// Capture tests never give their servers TCP: TCP cannot pass the MIT-SHM
/// fd, and Unix sockets (file or abstract) always reach the server.
const TCP: bool = false;

/// An `Xvfb` with one screen of `size` (`WxHxDepth`).
fn xvfb(size: &str) -> Option<XServer> {
    XServer::start("Xvfb", &["-noreset", "-screen", "0", size], None, TCP)
}

/// What these tests do with a test server.
trait TestServer {
    fn client(&self) -> (RustConnection, Window);
    fn capture(&self, monitor: Option<&str>) -> X11Capture;
}

impl TestServer for XServer {
    fn client(&self) -> (RustConnection, Window) {
        let (conn, screen) = x11_connect(&self.name).expect("connect to the test server");
        let root = conn.setup().roots[screen].root;
        (conn, root)
    }

    /// Capture `monitor` of this server, checked to be on the MIT-SHM path.
    fn capture(&self, monitor: Option<&str>) -> X11Capture {
        let cap = X11Capture::open(Some(&self.name), monitor).expect("open capture");
        eprintln!(
            "{}: capture open, MIT-SHM {}",
            self.name,
            cap.uses_mit_shm()
        );
        assert!(cap.uses_mit_shm(), "a Unix-socket server must give MIT-SHM");
        cap
    }
}

/// A mapped window filled with `pixel` (its background), drawn by the server.
fn window(conn: &RustConnection, root: Window, rect: (i16, i16, u16, u16), pixel: u32) -> Window {
    let id = conn.generate_id().expect("id");
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        id,
        root,
        rect.0,
        rect.1,
        rect.2,
        rect.3,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new().background_pixel(pixel),
    )
    .expect("create window");
    conn.map_window(id).expect("map window");
    conn.flush().expect("flush");
    id
}

fn pixel(f: &CapturedFrame<'_>, x: u32, y: u32) -> [u8; 4] {
    let FrameData::Cpu {
        format: PixelFormat::Bgra8,
        plane0,
        ..
    } = f.frame.data
    else {
        panic!("X11 frames are CPU BGRA");
    };
    let i = y as usize * plane0.stride + x as usize * 4;
    plane0.data[i..i + 4].try_into().expect("4 bytes")
}

fn covers(dirty: &[Rect], x: i32, y: i32) -> bool {
    dirty
        .iter()
        .any(|r| x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32)
}

/// Acquire until `hit` accepts a frame or `within` elapses; releases every
/// frame (trait invariant 3). Panics on a capture error.
fn wait_for(
    cap: &mut X11Capture,
    within: Duration,
    mut hit: impl FnMut(&CapturedFrame<'_>) -> bool,
) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        let matched = match cap.acquire(Duration::from_millis(100)) {
            Ok(Some(f)) => hit(&f),
            Ok(None) => false,
            Err(e) => panic!("capture failed: {e}"),
        };
        cap.release();
        if matched {
            return true;
        }
    }
    false
}

/// Take the session's first (full) frame so later frames are deltas.
fn drain_first(cap: &mut X11Capture) {
    assert!(
        wait_for(cap, Duration::from_secs(5), |f| !f.pointer_only),
        "a first frame"
    );
}

#[test]
fn first_frame_is_the_whole_screen_then_a_static_screen_is_idle() {
    let Some(x) = xvfb("1920x1080x24") else {
        return;
    };
    let mut cap = x.capture(None);
    assert_eq!(cap.dimensions(), (1920, 1080));

    let mut first = false;
    assert!(wait_for(&mut cap, Duration::from_secs(5), |f| {
        first = true;
        assert!(!f.pointer_only);
        assert_eq!((f.frame.width, f.frame.height), (1920, 1080));
        assert_eq!(
            f.dirty,
            vec![Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }]
        );
        let FrameData::Cpu { plane0, .. } = f.frame.data else {
            panic!("CPU frame");
        };
        assert_eq!(plane0.stride, 1920 * 4);
        assert_eq!(pixel(f, 960, 540)[3], 255, "alpha is opaque");
        assert!(f.cursor.is_some(), "the first frame reports the cursor");
        true
    }));
    assert!(first);

    // Nothing drawn, pointer still: every acquire times out with Ok(None).
    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        let got = cap.acquire(Duration::from_millis(100));
        assert!(matches!(got, Ok(None)), "static screen gave {got:?}");
        cap.release();
    }
}

#[test]
fn drawing_produces_dirty_rects_with_the_new_pixels() {
    let Some(x) = xvfb("1280x720x24") else {
        return;
    };
    let mut cap = x.capture(None);
    drain_first(&mut cap);
    let (conn, root) = x.client();

    let win = window(&conn, root, (100, 200, 300, 150), 0xff0000);
    assert!(
        wait_for(&mut cap, Duration::from_secs(5), |f| {
            !f.pointer_only && covers(&f.dirty, 150, 250) && pixel(f, 150, 250) == RED
        }),
        "the red window is captured"
    );

    // Move it: both the uncovered and the newly covered area are dirty, and
    // the dirty list stays far smaller than the screen.
    conn.configure_window(win, &ConfigureWindowAux::new().x(700))
        .expect("move");
    conn.flush().expect("flush");
    let mut old_seen = false;
    let mut new_seen = false;
    assert!(
        wait_for(&mut cap, Duration::from_secs(5), |f| {
            if f.pointer_only {
                return false;
            }
            let area: u64 = f.dirty.iter().map(Rect::area).sum();
            assert!(area < 1280 * 720 / 2, "dirty {:?}", f.dirty);
            old_seen |= covers(&f.dirty, 150, 250) && pixel(f, 150, 250) != RED;
            new_seen |= covers(&f.dirty, 750, 250) && pixel(f, 750, 250) == RED;
            old_seen && new_seen
        }),
        "old position repainted and new one drawn"
    );
}

#[test]
fn a_running_xterm_produces_dirty_rects() {
    if !installed("xterm") {
        return;
    }
    let Some(x) = xvfb("1920x1080x24") else {
        return;
    };
    let mut cap = x.capture(None);
    drain_first(&mut cap);
    let mut term = Command::new("xterm")
        .args([
            "-geometry",
            "60x15+40+40",
            "-e",
            "sh",
            "-c",
            "while :; do date +%s%N; sleep 0.05; done",
        ])
        .env("DISPLAY", &x.name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start xterm");

    // Scrolling output: a stream of small updates inside the terminal.
    let mut frames = 0;
    wait_for(&mut cap, Duration::from_secs(4), |f| {
        if !f.pointer_only && !f.dirty.is_empty() {
            frames += 1;
            assert!(
                f.dirty.iter().all(|r| r.x < 1000 && r.y < 600),
                "{:?}",
                f.dirty
            );
        }
        frames >= 10
    });
    assert!(frames >= 10, "only {frames} frames from a busy xterm");

    // Moving the terminal window dirties its new place.
    let (conn, root) = x.client();
    let tree = conn
        .query_tree(root)
        .expect("query tree")
        .reply()
        .expect("tree");
    let top = tree
        .children
        .iter()
        .copied()
        .find(|&w| {
            conn.get_window_attributes(w)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|a| a.map_state == MapState::VIEWABLE)
        })
        .expect("the xterm window");
    conn.configure_window(top, &ConfigureWindowAux::new().x(900).y(500))
        .expect("move");
    conn.flush().expect("flush");
    assert!(
        wait_for(&mut cap, Duration::from_secs(4), |f| covers(
            &f.dirty, 950, 550
        )),
        "moved xterm is captured at its new place"
    );
    let _ = term.kill();
    let _ = term.wait();
}

#[test]
fn pointer_moves_are_cursor_only_frames() {
    let Some(x) = xvfb("1280x720x24") else {
        return;
    };
    let mut cap = x.capture(None);
    let mut shape = false;
    assert!(wait_for(&mut cap, Duration::from_secs(5), |f| {
        shape = f.cursor.as_ref().is_some_and(|c| c.shape.is_some());
        !f.pointer_only
    }));
    assert!(shape, "the first frame carries the cursor image");

    let (conn, root) = x.client();
    conn.warp_pointer(NONE, root, 0, 0, 0, 0, 500, 400)
        .expect("warp");
    conn.flush().expect("flush");
    assert!(
        wait_for(&mut cap, Duration::from_secs(3), |f| {
            f.pointer_only
                && f.dirty.is_empty()
                && f.cursor.as_ref().is_some_and(|c| {
                    c.visible && c.position.x == 500 && c.position.y == 400 && c.shape.is_none()
                })
        }),
        "a pointer-only frame at the warped position"
    );
}

#[test]
fn randr_resize_is_lost_then_reinit_has_the_new_size() {
    if !installed("xrandr") {
        return;
    }
    let Some(host) = xvfb("1920x1080x24") else {
        return;
    };
    let Some(x) = XServer::start(
        "Xephyr",
        &["-noreset", "-screen", "1280x1024x24"],
        Some(&host.name),
        TCP,
    ) else {
        return;
    };
    let mut cap = x.capture(None);
    assert_eq!(cap.dimensions(), (1280, 1024));
    drain_first(&mut cap);

    let status = Command::new("xrandr")
        .args(["-s", "1024x768"])
        .env("DISPLAY", &x.name)
        .status()
        .expect("run xrandr");
    assert!(status.success(), "xrandr -s 1024x768");

    let end = Instant::now() + Duration::from_secs(5);
    let mut lost = false;
    while Instant::now() < end && !lost {
        match cap.acquire(Duration::from_millis(100)) {
            Err(CaptureError::Lost) => lost = true,
            Ok(_) => {}
            Err(e) => panic!("expected Lost, got {e}"),
        }
        cap.release();
    }
    assert!(lost, "the size change is reported as Lost");
    cap.reinit().expect("reinit");
    assert_eq!(cap.dimensions(), (1024, 768));
    assert!(wait_for(&mut cap, Duration::from_secs(5), |f| {
        (f.frame.width, f.frame.height) == (1024, 768)
            && f.dirty
                == vec![Rect {
                    x: 0,
                    y: 0,
                    w: 1024,
                    h: 768,
                }]
    }));
}

#[test]
fn server_gone_is_lost_and_reinit_says_why() {
    let Some(x) = xvfb("640x480x24") else {
        return;
    };
    let mut cap = x.capture(None);
    drain_first(&mut cap);
    drop(x);

    let end = Instant::now() + Duration::from_secs(5);
    let mut lost = false;
    while Instant::now() < end && !lost {
        lost = matches!(
            cap.acquire(Duration::from_millis(100)),
            Err(CaptureError::Lost)
        );
        cap.release();
    }
    assert!(lost, "a vanished server is Lost");
    match cap.reinit() {
        Err(CaptureError::Backend(m)) => {
            assert!(m.contains("cannot connect to the X server"), "{m}")
        }
        other => panic!("expected an actionable error, got {other:?}"),
    }
    // Backing off: idle first, then Lost again so the host retries.
    assert!(matches!(cap.acquire(Duration::from_millis(10)), Ok(None)));
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(
        cap.acquire(Duration::from_millis(10)),
        Err(CaptureError::Lost)
    ));
}

#[test]
fn displays_list_the_monitors_with_ids_capture_accepts() {
    let Some(x) = xvfb("1920x1080x24") else {
        return;
    };
    let displays = x11_displays(Some(&x.name)).expect("list displays");
    assert_eq!(displays.len(), 1, "{displays:?}");
    let d = &displays[0];
    assert!(d.id.starts_with("x11:") && d.primary, "{d:?}");
    assert_eq!((d.x, d.y, d.width, d.height), (0, 0, 1920, 1080));
    assert_eq!(d.scale, 1.0);

    let cap = x.capture(Some(&d.id));
    assert_eq!(cap.dimensions(), (1920, 1080));
    for foreign in ["dxgi:0", "x11:DP-9", "cg:610-a050-0"] {
        assert!(
            matches!(
                X11Capture::open(Some(&x.name), Some(foreign)),
                Err(CaptureError::NoOutput)
            ),
            "{foreign}"
        );
    }
}

#[test]
fn a_second_monitor_is_captured_as_its_own_rectangle() {
    let Some(x) = xvfb("1920x1080x24") else {
        return;
    };
    let (conn, root) = x.client();
    let name = conn
        .intern_atom(false, b"RIGHT")
        .expect("intern")
        .reply()
        .expect("atom")
        .atom;
    conn.randr_set_monitor(
        root,
        MonitorInfo {
            name,
            primary: false,
            automatic: false,
            x: 960,
            y: 0,
            width: 960,
            height: 1080,
            width_in_millimeters: 254,
            height_in_millimeters: 286,
            outputs: Vec::new(),
        },
    )
    .expect("set monitor")
    .check()
    .expect("server accepts the monitor");

    let displays = x11_displays(Some(&x.name)).expect("list displays");
    let right = displays
        .iter()
        .find(|d| d.id == "x11:RIGHT")
        .unwrap_or_else(|| panic!("RIGHT listed: {displays:?}"));
    assert_eq!((right.x, right.width, right.height), (960, 960, 1080));
    assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);

    let mut cap = x.capture(Some("x11:RIGHT"));
    assert_eq!(cap.dimensions(), (960, 1080));
    drain_first(&mut cap);

    // Drawing on the left half is not this monitor's business.
    window(&conn, root, (100, 100, 50, 50), 0x00ff00);
    let got = wait_for(&mut cap, Duration::from_millis(800), |f| !f.pointer_only);
    assert!(!got, "damage on the other monitor produced a frame");

    // Drawing on the right half appears in monitor coordinates.
    window(&conn, root, (1000, 100, 50, 50), 0xff0000);
    assert!(wait_for(&mut cap, Duration::from_secs(5), |f| {
        !f.pointer_only && covers(&f.dirty, 50, 120) && pixel(f, 50, 120) == RED
    }));
}

#[test]
fn xwayland_is_refused() {
    // Only meaningful where a real Xwayland runs (a Wayland desktop, WSLg), so
    // its skips stay skips even under SCT_X11_TESTS_REQUIRED: CI has no
    // Wayland session.
    let Some(display) = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()) else {
        eprintln!("skipping: no DISPLAY");
        return;
    };
    let Ok((conn, _)) = x11_connect(&display) else {
        eprintln!("skipping: {display} is not reachable");
        return;
    };
    // Recognised the three ways an Xwayland can be: the XWAYLAND extension
    // (Xwayland 22.1+), `XWAYLAND<n>` outputs (every version, e.g. WSLg's),
    // or a Wayland session around it.
    let extension = conn
        .extension_information("XWAYLAND")
        .ok()
        .flatten()
        .is_some();
    let outputs = x11_displays(Some(&display))
        .is_ok_and(|ds| ds.iter().any(|d| d.name.starts_with("XWAYLAND")));
    let session = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    eprintln!(
        "{display}: extension {extension}, XWAYLAND outputs {outputs}, WAYLAND_DISPLAY {session}"
    );
    if !(extension || outputs || session) {
        eprintln!("skipping: {display} is not Xwayland");
        return;
    }
    match X11Capture::open(Some(&display), None) {
        Err(CaptureError::Backend(m)) => assert!(m.contains("Xwayland"), "{m}"),
        Err(e) => panic!("expected the Xwayland refusal, got {e}"),
        Ok(_) => panic!("capture opened on Xwayland"),
    }
}

#[test]
fn an_unreachable_display_is_an_actionable_error() {
    // Display 4321 has no socket: nothing listens there.
    match X11Capture::open(Some(":4321"), None) {
        Err(CaptureError::Backend(m)) => {
            assert!(m.contains("cannot connect to the X server at :4321"), "{m}")
        }
        Err(e) => panic!("expected Backend, got {e}"),
        Ok(_) => panic!("connected to :4321"),
    }
}

// ---- real desktop (plans/checklists/L2.md) ----------------------------------

/// CPU seconds this process has used (user + system), from `/proc/self/stat`
/// fields 14 and 15, in clock ticks of 1/100 s (Linux's `USER_HZ` on every
/// mainstream architecture).
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // The command name (field 2) is parenthesised and may contain spaces.
    let after = &stat[stat.rfind(')').expect("comm field") + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks: u64 = f[11].parse::<u64>().expect("utime") + f[12].parse::<u64>().expect("stime");
    ticks as f64 / 100.0
}

/// The monitors of the desktop this test runs in.
#[test]
#[ignore = "needs a real X11 desktop session (run from a terminal on the desktop)"]
fn desktop_lists_displays() {
    let displays = x11_displays(None).expect("list the desktop's displays");
    for d in &displays {
        println!("{d:?}");
    }
    assert!(!displays.is_empty());
    assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);
}

/// Every listed display opens and its first frame has the listed size.
#[test]
#[ignore = "needs a real X11 desktop session (run from a terminal on the desktop)"]
fn desktop_each_display_captures() {
    for d in x11_displays(None).expect("list displays") {
        let mut cap = X11Capture::open(None, Some(&d.id)).expect("open the display");
        assert_eq!(cap.dimensions(), (d.width, d.height), "{}", d.id);
        assert!(
            wait_for(&mut cap, Duration::from_secs(5), |f| !f.pointer_only
                && (f.frame.width, f.frame.height) == (d.width, d.height)),
            "{}: a first frame",
            d.id
        );
        println!("{}: {}x{} ok", d.id, d.width, d.height);
    }
}

/// 10 s of motion (the tester keeps something moving), then 10 s hands off.
#[test]
#[ignore = "needs a real X11 desktop session and a tester moving a window or playing video"]
fn desktop_busy_then_idle() {
    let mut cap = X11Capture::open(None, None).expect("open the primary display");
    let (mut w, mut h) = cap.dimensions();
    println!(
        "capturing the primary display at {w}x{h}, MIT-SHM {}",
        cap.uses_mit_shm()
    );
    assert!(cap.uses_mit_shm(), "a local desktop must give MIT-SHM");
    for (phase, label) in [(1, "BUSY: keep a window moving"), (2, "IDLE: hands off")] {
        println!("PHASE {phase}: {label} (10 s)");
        let (mut frames, mut pointer, mut idle, mut area) = (0u64, 0u64, 0u64, 0u64);
        let cpu0 = cpu_seconds();
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            let lost = match cap.acquire(Duration::from_millis(100)) {
                Ok(Some(f)) if f.pointer_only => {
                    pointer += 1;
                    false
                }
                Ok(Some(f)) => {
                    frames += 1;
                    area += f.dirty.iter().map(Rect::area).sum::<u64>();
                    false
                }
                Ok(None) => {
                    idle += 1;
                    false
                }
                Err(CaptureError::Lost) => true,
                Err(e) => panic!("capture failed: {e}"),
            };
            cap.release();
            if lost {
                // A resolution or monitor change (checklist step 4): rebuild,
                // exactly as the host does.
                cap.reinit().expect("rebuild after a screen change");
                (w, h) = cap.dimensions();
                println!("lost -> rebuilt at {w}x{h}");
            }
        }
        let cpu = (cpu_seconds() - cpu0) / 10.0 * 100.0;
        let dirty = if frames > 0 {
            area as f64 / frames as f64 / (w as f64 * h as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "=> {:.1} fps, {pointer} pointer-only, {idle} idle, dirty {dirty:.1} % of the \
             screen per frame, process CPU {cpu:.1} %",
            frames as f64 / 10.0
        );
        if phase == 2 {
            assert!(idle > 0, "a static desktop must yield Ok(None)");
        }
    }
}
