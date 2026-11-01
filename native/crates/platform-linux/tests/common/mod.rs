//! Shared by the headless video-surface tests (`x11_surface`, `wayland_surface`):
//! a throwaway display server, a GTK window standing in for the Tauri one, the
//! viewer's present loop, and input collection.

// Each test binary compiles this module on its own and uses a different
// subset of it (only X11 waits for a socket path, for example).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use platform_api::{Frame, VideoInput, VideoSurface};

/// Remote frame size the tests present (letterboxed into the window).
pub const FRAME: (u32, u32) = (1280, 720);
/// BT.709 limited-range pure red: Y = 16 + 219 × 0.2126, Cb = 128 − 224 ×
/// 0.1146, Cr = 128 + 224 × 0.5.
pub const RED_YUV: (u8, u8, u8) = (63, 102, 240);

/// A test failure, reported by `main` with a non-zero exit code.
pub type Outcome = Result<(), String>;

pub fn check(ok: bool, what: impl Into<String>) -> Outcome {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

/// `program` on `PATH`, if installed.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// A child process killed (and reaped) on drop.
pub struct Server(pub Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn spawn(cmd: &mut Command) -> Result<Server, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Server)
        .map_err(|e| format!("cannot start {cmd:?}: {e}"))
}

/// Wait until `path` exists (a display socket), or fail.
pub fn wait_for_path(server: &mut Server, path: &Path, what: &str) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        if let Ok(Some(status)) = server.0.try_wait() {
            return Err(format!("{what} exited early: {status}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("{what} did not create {}", path.display()))
}

/// Run GTK's main loop until nothing is pending.
pub fn pump() {
    while gtk::events_pending() {
        gtk::main_iteration();
    }
}

/// Pump GTK for `duration` (so signals, layout and keys are processed).
pub fn pump_for(duration: Duration) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The stand-in for the Tauri window: a toplevel with one content child (the
/// widget the video covers), shown and laid out.
pub fn app_window(size: (i32, i32)) -> Result<(gtk::Window, gtk::Box), String> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("ShareCtrlScreen test");
    // No CSD shadow or header bar: the content starts at the surface origin,
    // so screen coordinates are easy to script.
    window.set_decorated(false);
    window.set_default_size(size.0, size.1);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&content);
    window.show_all();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        pump_for(Duration::from_millis(20));
        if window.is_visible() && window.window().is_some() && content.allocated_width() > 1 {
            pump_for(Duration::from_millis(200));
            return Ok((window, content));
        }
    }
    Err("the GTK test window was never mapped".into())
}

/// One solid red NV12 frame.
pub struct Nv12 {
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl Nv12 {
    pub fn red() -> Self {
        let (w, h) = (FRAME.0 as usize, FRAME.1 as usize);
        let (y, cb, cr) = RED_YUV;
        Self {
            y: vec![y; w * h],
            uv: [cb, cr].repeat(w / 2 * (h / 2)),
        }
    }

    pub fn frame(&self) -> Frame<'_> {
        Frame::nv12(
            FRAME.0,
            FRAME.1,
            &self.y,
            FRAME.0 as usize,
            &self.uv,
            FRAME.0 as usize,
        )
    }
}

/// The viewer media thread's part: create the presenter, show, present `n`
/// frames. Runs on its own thread (as in `engine::media::viewer`) while the
/// caller keeps GTK's loop running; returns the number of successful presents.
pub fn present_frames(surface: &Arc<dyn VideoSurface>, n: usize) -> Result<usize, String> {
    let worker = {
        let surface = surface.clone();
        std::thread::spawn(move || -> Result<usize, String> {
            let mut presenter = surface
                .create_presenter()
                .map_err(|e| format!("create_presenter: {e}"))?;
            surface.show();
            let frame = Nv12::red();
            let mut ok = 0;
            for i in 0..n {
                // The cursor sprite is drawn near the top-left corner, away
                // from the centre the pixel checks sample.
                match presenter.present(&frame.frame(), Some((0.05, 0.05))) {
                    Ok(()) => ok += 1,
                    Err(e) => return Err(format!("present {i}: {e}")),
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(ok)
        })
    };
    while !worker.is_finished() {
        pump_for(Duration::from_millis(10));
    }
    worker
        .join()
        .map_err(|_| "the present thread panicked".to_string())?
}

/// Collect input for `duration` while pumping GTK.
pub fn collect(rx: &Receiver<VideoInput>, duration: Duration) -> Vec<VideoInput> {
    let end = Instant::now() + duration;
    let mut out = Vec::new();
    while Instant::now() < end {
        pump();
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(ev) => out.push(ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

pub fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.02
}

/// Whether `events` contains a move near `(nx, ny)`.
pub fn has_move(events: &[VideoInput], nx: f64, ny: f64) -> bool {
    events
        .iter()
        .any(|e| matches!(*e, VideoInput::Move { nx: x, ny: y } if near(x, nx) && near(y, ny)))
}

pub fn has_button(events: &[VideoInput], button: u8, down: bool) -> bool {
    events.iter().any(
        |e| matches!(*e, VideoInput::Button { button: b, down: d, .. } if b == button && d == down),
    )
}
