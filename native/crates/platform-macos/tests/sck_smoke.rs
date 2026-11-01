//! ScreenCaptureKit capture on a real Mac (plan task M1). Needs an interactive
//! login session and the Screen Recording grant for the app that runs
//! `cargo test` (Terminal / iTerm), so every test is `#[ignore]`d and run by
//! hand — see plans/checklists/M1.md:
//!
//! ```text
//! cargo test -p platform-macos --test sck_smoke -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use platform_api::{CaptureError, FrameData, Platform, ScreenCapture};
use platform_macos::gpu::CvPixelBufferFrame;
use platform_macos::MacPlatform;

fn logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("platform_macos=debug")
        .with_test_writer()
        .try_init();
}

/// User + system CPU time of this whole process (capture thread and the
/// ScreenCaptureKit delivery queue included).
fn cpu_time() -> Duration {
    // SAFETY: `usage` is a valid out pointer.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed");
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

#[derive(Default, Debug)]
struct Counts {
    frames: u32,
    pointer_only: u32,
    idle: u32,
    cursor_updates: u32,
    shapes: u32,
    dirty_rects: usize,
    full_damage: u32,
}

/// Pull frames for `secs` like the host loop does (16 ms timeout, release on
/// every path) and check every frame against the contract.
fn run(cap: &mut dyn ScreenCapture, secs: u64) -> Result<Counts, CaptureError> {
    let mut c = Counts::default();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let (w, h) = cap.dimensions();
        match cap.acquire(Duration::from_millis(16))? {
            None => c.idle += 1,
            Some(f) => {
                assert_eq!(
                    (f.frame.width, f.frame.height),
                    (w, h),
                    "frame ≠ dimensions()"
                );
                let FrameData::Native(any) = f.frame.data else {
                    panic!("expected a native frame");
                };
                let pb = any
                    .downcast_ref::<CvPixelBufferFrame>()
                    .expect("FrameData::Native must be a CvPixelBufferFrame");
                assert_eq!(pb.size(), (w, h), "pixel buffer ≠ dimensions()");
                assert!(pb.format().is_some(), "unexpected pixel format");
                for r in &f.dirty {
                    assert!(r.x >= 0 && r.y >= 0, "{r:?}");
                    assert!(r.x as u32 + r.w <= w && r.y as u32 + r.h <= h, "{r:?}");
                }
                if f.pointer_only {
                    c.pointer_only += 1;
                    assert!(f.cursor.is_some(), "pointer-only frame without a cursor");
                } else {
                    c.frames += 1;
                    c.dirty_rects += f.dirty.len();
                    if f.dirty.is_empty() {
                        c.full_damage += 1;
                    }
                }
                if let Some(cur) = &f.cursor {
                    c.cursor_updates += 1;
                    if let Some(s) = &cur.shape {
                        c.shapes += 1;
                        assert_eq!(s.bgra.len(), (s.width * s.height * 4) as usize);
                    }
                }
                drop(f);
                cap.release();
                // Idempotent.
                cap.release();
            }
        }
    }
    Ok(c)
}

#[test]
#[ignore = "needs a Mac"]
fn lists_displays_with_stable_ids() {
    logs();
    let p = MacPlatform::new();
    let d = p.displays();
    println!("displays: {d:#?}");
    assert!(!d.is_empty());
    assert_eq!(d.iter().filter(|d| d.primary).count(), 1);
    assert_eq!(p.displays(), d, "ids must be stable between calls");
    println!("permissions: {:?}", p.permissions());
}

/// Acceptance: 60 fps with < 1 % CPU, and `Ok(None)` on a static screen.
/// Play a 60 fps video (or drag a window) for the first phase, leave the
/// screen alone for the second.
#[test]
#[ignore = "needs a Mac"]
fn captures_busy_then_idle_screen() {
    logs();
    let p = MacPlatform::new();
    let mut cap = p.create_capture(None).expect("create_capture");
    let (w, h) = cap.dimensions();
    assert!(w >= 2 && h >= 2 && w % 2 == 0 && h % 2 == 0);
    println!("capturing {w}x{h}");

    println!("PHASE 1 (10 s): keep something moving on screen");
    let cpu0 = cpu_time();
    let t0 = Instant::now();
    let busy = run(cap.as_mut(), 10).expect("busy phase");
    let cpu = (cpu_time() - cpu0).as_secs_f64() / t0.elapsed().as_secs_f64() * 100.0;
    let fps = busy.frames as f64 / t0.elapsed().as_secs_f64();
    println!("busy: {busy:?}\n=> {fps:.1} fps, process CPU {cpu:.2} %");

    println!("PHASE 2 (10 s): do not touch anything (move the mouse only at the end)");
    let idle = run(cap.as_mut(), 10).expect("idle phase");
    println!("idle: {idle:?}");
    assert!(
        idle.idle > idle.frames * 10,
        "a static screen must mostly yield Ok(None)"
    );
}

#[test]
#[ignore = "needs a Mac"]
fn reinit_and_resize_keep_the_contract() {
    logs();
    let p = MacPlatform::new();
    let mut cap = p.create_capture(None).expect("create_capture");
    let native = cap.dimensions();
    run(cap.as_mut(), 2).expect("before reinit");
    cap.reinit().expect("reinit");
    assert_eq!(cap.dimensions(), native);
    let after = run(cap.as_mut(), 2).expect("after reinit");
    println!("after reinit: {after:?}");
    assert!(
        after.frames >= 1,
        "the rebuilt stream must deliver a first frame"
    );

    assert!(cap.request_output_size((1280, 720)));
    let small = cap.dimensions();
    assert!(small.0 <= 1280 && small.1 <= 720, "{small:?}");
    println!(
        "resized to {small:?}: {:?}",
        run(cap.as_mut(), 3).expect("resized")
    );
    assert!(cap.request_output_size((0, 0)));
    assert_eq!(cap.dimensions(), native);
}

/// Soak for `SCK_SOAK_SECS` (default 300): sleep/wake the display, lock the
/// screen, or revoke Screen Recording meanwhile. Recovers from `Lost` the way
/// the host does and stops on `PermissionDenied`.
#[test]
#[ignore = "needs a Mac"]
fn soak_recovers_from_sleep_and_reports_revocation() {
    logs();
    let secs: u64 = std::env::var("SCK_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let p = MacPlatform::new();
    let mut cap = p.create_capture(None).expect("create_capture");
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut lost, mut rebuilt) = (0u32, 0u32);
    while Instant::now() < end {
        match run(cap.as_mut(), 5) {
            Ok(c) => println!("{:?} {c:?}", cap.dimensions()),
            Err(CaptureError::Lost) => {
                lost += 1;
                println!("LOST → reinit");
                match cap.reinit() {
                    Ok(()) => rebuilt += 1,
                    Err(e) => {
                        println!("reinit failed: {e}");
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            Err(CaptureError::PermissionDenied) => {
                println!("PERMISSION DENIED (expected after revoking Screen Recording)");
                break;
            }
            Err(e) => panic!("unexpected capture error: {e}"),
        }
    }
    println!("lost {lost}×, rebuilt {rebuilt}×");
}
