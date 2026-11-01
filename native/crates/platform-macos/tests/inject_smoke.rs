//! CGEvent input injection on a real Mac (plan task M5). Needs an interactive
//! login session and the Accessibility grant for the app that runs
//! `cargo test` (Terminal / iTerm: System Settings > Privacy & Security >
//! Accessibility), and it moves the real pointer and types into the focused
//! window, so every test is `#[ignore]`d and run by hand — see
//! plans/checklists/M5.md:
//!
//! ```text
//! cargo test -p platform-macos --test inject_smoke -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "macos")]

use std::thread::sleep;
use std::time::Duration;

use objc2_core_graphics::CGEvent;
use platform_api::{InputInjector, Platform, Rect};
use platform_macos::MacPlatform;
use protocol::InputMsg;

fn injector(platform: &MacPlatform) -> Box<dyn InputInjector> {
    match platform.create_injector() {
        Ok(inj) => inj,
        Err(e) => panic!("grant Accessibility to the terminal running the test: {e}"),
    }
}

fn cursor() -> (f64, f64) {
    let probe = CGEvent::new(None).expect("CoreGraphics event for the cursor position");
    let p = CGEvent::location(Some(&probe));
    (p.x, p.y)
}

fn key(inj: &mut dyn InputInjector, code: &str) {
    inj.dispatch(&InputMsg::KeyDown { code: code.into() });
    inj.dispatch(&InputMsg::KeyUp { code: code.into() });
}

fn countdown(what: &str) {
    println!("{what}");
    for s in (1..=5).rev() {
        println!("  {s}…");
        sleep(Duration::from_secs(1));
    }
}

/// Every display's corners and centre are reachable, in points, including
/// displays left of or above the main one (negative coordinates).
#[test]
#[ignore = "needs a Mac with Accessibility granted (moves the pointer)"]
fn pointer_reaches_every_display() {
    let platform = MacPlatform::new();
    let mut inj = injector(&platform);
    let displays = platform.displays();
    assert!(!displays.is_empty());
    for d in displays {
        let rect = Rect {
            x: d.x,
            y: d.y,
            w: d.width,
            h: d.height,
        };
        // The engine passes `None` for the main display; check both paths.
        let targets = if d.primary {
            vec![None, Some(rect)]
        } else {
            vec![Some(rect)]
        };
        for target in targets {
            inj.set_target_rect(target);
            for (nx, ny) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0)] {
                inj.dispatch(&InputMsg::Move { x: nx, y: ny });
                sleep(Duration::from_millis(100));
                let want_x = (f64::from(d.x) + nx * f64::from(d.width))
                    .min(f64::from(d.x) + f64::from(d.width) - 1.0);
                let want_y = (f64::from(d.y) + ny * f64::from(d.height))
                    .min(f64::from(d.y) + f64::from(d.height) - 1.0);
                let (x, y) = cursor();
                println!("{} {nx},{ny}: at {x},{y}, want {want_x},{want_y}", d.name);
                assert!(
                    (x - want_x).abs() <= 1.0 && (y - want_y).abs() <= 1.0,
                    "{} ({nx}, {ny}): cursor at ({x}, {y}), expected ({want_x}, {want_y})",
                    d.name
                );
            }
        }
    }
}

/// Open TextEdit with an empty document and click into it during the
/// countdown. Expected text afterwards: `hello world` (the second word comes
/// from the clipboard through Cmd+V).
#[test]
#[ignore = "needs a Mac with Accessibility granted and a focused TextEdit document"]
fn types_hello_then_cmd_v_pastes() {
    let platform = MacPlatform::new();
    let mut inj = injector(&platform);
    assert!(platform.create_clipboard().set_text(" world").is_some());
    countdown("click into an empty TextEdit document");
    for code in ["KeyH", "KeyE", "KeyL", "KeyL", "KeyO"] {
        key(&mut *inj, code);
    }
    let chord = inj.paste_chord();
    assert_eq!(chord, ["MetaLeft", "KeyV"]);
    for code in chord {
        inj.dispatch(&InputMsg::KeyDown {
            code: (*code).into(),
        });
    }
    for code in chord.iter().rev() {
        inj.dispatch(&InputMsg::KeyUp {
            code: (*code).into(),
        });
    }
    println!("TextEdit must now read: hello world");
}

/// Same TextEdit document. Shift and Cmd are pressed and never released by
/// the "viewer"; `release_all` must release them, so the `a` typed after it
/// is a lowercase `a` (not `A`, and not Cmd+A selecting everything).
#[test]
#[ignore = "needs a Mac with Accessibility granted and a focused TextEdit document"]
fn release_all_leaves_no_modifier_held() {
    let platform = MacPlatform::new();
    let mut inj = injector(&platform);
    countdown("click at the end of the TextEdit text");
    inj.dispatch(&InputMsg::KeyDown {
        code: "ShiftLeft".into(),
    });
    inj.dispatch(&InputMsg::KeyDown {
        code: "MetaLeft".into(),
    });
    inj.release_all();
    key(&mut *inj, "KeyA");
    println!("TextEdit must end in a lowercase 'a' with nothing selected");
}

/// Drop target: put a TextEdit window and a Finder window side by side, then
/// hover the pointer over TextEdit during the countdown while Finder is
/// active. `focus_window_at` the pointer's position must make TextEdit
/// active without clicking (no caret move, no selection change).
#[test]
#[ignore = "needs a Mac with Accessibility granted and two app windows"]
fn focus_window_at_activates_the_window_under_the_point() {
    let platform = MacPlatform::new();
    let mut inj = injector(&platform);
    countdown("activate Finder, then rest the pointer over the TextEdit window");
    let main = platform
        .displays()
        .into_iter()
        .find(|d| d.primary)
        .expect("a main display");
    let (x, y) = cursor();
    let nx = (x - f64::from(main.x)) / f64::from(main.width);
    let ny = (y - f64::from(main.y)) / f64::from(main.height);
    inj.set_target_rect(None);
    assert!(inj.focus_window_at(nx, ny), "no window under the pointer");
    println!("TextEdit must now be the active app (its name in the menu bar)");
}
