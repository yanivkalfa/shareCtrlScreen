//! Wayland capture through a real xdg-desktop-portal + PipeWire (task L1 acceptance).
//!
//! Needs a Wayland session with PipeWire, xdg-desktop-portal and a ScreenCast backend,
//! and something moving on screen (the "test pattern window"). In CI (task T4) that is
//! headless sway + xdg-desktop-portal-wlr with `chooser_type=none` and
//! `output_name=HEADLESS-1` in its config, so no dialog needs an answer; on a desktop
//! (GNOME, KDE) answer the dialog once, the second run must not show it. Run:
//! `cargo test -p platform-linux --test portal_capture -- --ignored --nocapture`
//! (see `plans/checklists/L1.md`).
//!
//! One test function on purpose: it points `XDG_STATE_HOME` at a temporary directory
//! (so the developer's own restore token is never touched), and parallel tests in
//! this binary would race on it.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use platform_api::{FrameData, PixelFormat, Platform, ScreenCapture};
use platform_linux::LinuxPlatform;

const FRAMES: usize = 100;
const LIMIT: Duration = Duration::from_secs(60);

/// Capture `FRAMES` frames with new pixels and check each one's shape.
fn capture_frames(cap: &mut dyn ScreenCapture) {
    let (w, h) = cap.dimensions();
    assert!(w > 0 && h > 0, "dimensions {w}x{h}");
    let started = Instant::now();
    let mut pictures = 0;
    while pictures < FRAMES {
        assert!(
            started.elapsed() < LIMIT,
            "only {pictures} frames in {} s: is something moving on screen?",
            LIMIT.as_secs()
        );
        let frame = cap
            .acquire(Duration::from_millis(100))
            .expect("acquire failed");
        let Some(frame) = frame else {
            continue;
        };
        if !frame.pointer_only {
            assert_eq!((frame.frame.width, frame.frame.height), (w, h));
            let FrameData::Cpu { format, plane0, .. } = frame.frame.data else {
                panic!("the portal capture produces CPU frames");
            };
            assert_eq!(format, PixelFormat::Bgra8);
            assert_eq!(plane0.stride, w as usize * 4);
            assert_eq!(plane0.data.len(), plane0.stride * h as usize);
            for r in &frame.dirty {
                assert!(r.x >= 0 && r.y >= 0 && r.x as u32 + r.w <= w && r.y as u32 + r.h <= h);
            }
            pictures += 1;
        }
        drop(frame);
        cap.release();
    }
    eprintln!(
        "{FRAMES} frames of {w}x{h} in {:.1} s",
        started.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "needs a Wayland desktop with xdg-desktop-portal, a ScreenCast backend and PipeWire"]
fn captures_frames_and_restores_the_session_without_a_dialog() {
    let state = tempfile::tempdir().expect("temp dir");
    std::env::set_var("XDG_STATE_HOME", state.path());
    let platform = LinuxPlatform::new();
    let token_path = platform.paths().log_dir.join("portal.token");
    assert!(token_path.starts_with(state.path()));

    // First run: the dialog (if the backend shows one) grants the screen and the portal
    // hands back a restore token.
    let mut cap = platform.create_capture(None).expect("first capture");
    capture_frames(cap.as_mut());
    drop(cap);
    let first = std::fs::read_to_string(&token_path)
        .expect("the portal returned no restore token (ScreenCast version < 4?)");

    // Second run restores from the token: no dialog, and the single-use token is
    // replaced by a new one.
    let started = Instant::now();
    let mut cap = platform.create_capture(None).expect("restored capture");
    eprintln!("restored in {:.1} s", started.elapsed().as_secs_f64());
    capture_frames(cap.as_mut());
    drop(cap);
    let second = std::fs::read_to_string(&token_path).expect("no token after the restore");
    assert_ne!(first, second, "a restore must hand back a fresh token");
}
