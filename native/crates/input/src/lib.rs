//! Win32 input injection via `SendInput` (Plan 04 §8a) — the only crate (with
//! `elevation`) that drives `SendInput`/desktop switching, mirroring how the old
//! app isolated koffi in `input.js`.
//!
//! Ported directly from `app/main/input.js`: absolute mouse move with
//! virtual-desktop normalization, wheel, and **scan-code** keyboard for layout
//! independence; injected-down state is tracked so a vanished viewer can never
//! leave the host with stuck input (`release_all`). Every injected event carries
//! a sentinel in `dwExtraInfo` so the viewer-side `WH_KEYBOARD_LL` hook
//! (`keyhook`) recognizes and ignores our own synthetic input.

#[cfg(windows)]
pub mod keyhook;
pub mod scancode;

pub use protocol::{Button, InputMsg};
pub use scancode::ScanCode;

/// Sentinel written to `dwExtraInfo` on every injected event so our own
/// low-level keyboard hook can filter it out (prevents feedback loops), exactly
/// as the old `keyhook.js` checked `LLKHF_INJECTED` plus this tag.
pub const INJECT_SENTINEL: usize = 0x5343_5253; // "SCRS"

/// Physical bounds of the shared display, in the Win32 virtual-desktop
/// coordinate space. `None` ⇒ map to the primary display (the verified default
/// path from `input.js`).
#[derive(Debug, Clone, Copy)]
pub struct TargetRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::HashSet;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// Stateful injector. Drive it from one thread (the engine's input thread),
    /// exactly as the old single-module design did.
    #[derive(Default)]
    pub struct Injector {
        down_keys: HashSet<String>,
        down_buttons: HashSet<u8>,
        target: Option<TargetRect>,
    }

    impl Injector {
        pub fn new() -> Self {
            Self::default()
        }

        /// Set the shared display rectangle (session start). `None` ⇒ primary.
        pub fn set_target_rect(&mut self, rect: Option<TargetRect>) {
            self.target = rect.filter(|r| r.w > 0 && r.h > 0);
        }

        /// Dispatch a decoded input message. No-op returns are silent (unknown
        /// key codes, invalid buttons) exactly as `input.js`.
        pub fn dispatch(&mut self, msg: &InputMsg) {
            match msg {
                InputMsg::Move { x, y } => self.mouse_move(*x, *y),
                InputMsg::ButtonDown { b, x, y } => self.mouse_button(*b, true, *x, *y),
                InputMsg::ButtonUp { b, x, y } => self.mouse_button(*b, false, *x, *y),
                InputMsg::Wheel { dx, dy } => self.wheel(*dx, *dy),
                InputMsg::KeyDown { code } => self.key(code, true),
                InputMsg::KeyUp { code } => self.key(code, false),
            }
        }

        fn send_mouse(&self, flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, mouse_data: u32) -> u32 {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: mouse_data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECT_SENTINEL,
                    },
                },
            };
            // windows-rs folds the count into the slice length; cbSize must be
            // size_of::<INPUT>() or SendInput silently returns 0 (§8a gotcha).
            unsafe { SendInput(&[input], core::mem::size_of::<INPUT>() as i32) }
        }

        /// Normalized in, normalized out. Primary (no target): plain ABSOLUTE
        /// over [0,65535]. Non-primary: map within the shared display then
        /// normalize against the whole virtual desktop with VIRTUALDESK. All
        /// geometry is physical px (per-monitor-v2 aware process).
        pub fn mouse_move(&self, nx: f64, ny: f64) {
            let Some(t) = self.target else {
                self.send_mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    (nx * 65535.0).round() as i32,
                    (ny * 65535.0).round() as i32,
                    0,
                );
                return;
            };

            // SAFETY: pure metric reads.
            let (vx, vy, vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            if vw <= 1 || vh <= 1 {
                self.send_mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    (nx * 65535.0).round() as i32,
                    (ny * 65535.0).round() as i32,
                    0,
                );
                return;
            }

            // Do the multiply in f64/i64 space to avoid overflow (§8a).
            let abs_x = t.x as f64 + nx * t.w as f64;
            let abs_y = t.y as f64 + ny * t.h as f64;
            let dx = (((abs_x - vx as f64) * 65535.0) / (vw as f64 - 1.0)).round() as i32;
            let dy = (((abs_y - vy as f64) * 65535.0) / (vh as f64 - 1.0)).round() as i32;
            self.send_mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                dx,
                dy,
                0,
            );
        }

        pub fn mouse_button(&mut self, b: Button, is_down: bool, nx: f64, ny: f64) {
            let (down, up) = match b {
                Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            };
            let id: u8 = b.into();
            if is_down {
                self.down_buttons.insert(id);
            } else {
                self.down_buttons.remove(&id);
            }
            // Move first so the click lands exactly where aimed even if a
            // preceding unreliable 'mm' was dropped.
            self.mouse_move(nx, ny);
            self.send_mouse(if is_down { down } else { up }, 0, 0, 0);
        }

        /// Values arrive already in Windows wheel units (multiples of ±120). The
        /// signed delta rides the u32 `mouseData` bit-for-bit (§8a cast note).
        pub fn wheel(&self, dx: i32, dy: i32) {
            if dy != 0 {
                self.send_mouse(MOUSEEVENTF_WHEEL, 0, 0, dy as u32);
            }
            if dx != 0 {
                self.send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx as u32);
            }
        }

        /// Scan-code injection keeps this layout-independent. Unknown codes are
        /// silently ignored.
        pub fn key(&mut self, code: &str, is_down: bool) {
            let Some(hit) = scancode::lookup(code) else {
                return;
            };
            if is_down {
                self.down_keys.insert(code.to_string());
            } else {
                self.down_keys.remove(code);
            }
            self.emit_scancode(hit, is_down);
        }

        fn emit_scancode(&self, hit: ScanCode, is_down: bool) {
            let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
            if hit.ext {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !is_down {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: hit.sc,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECT_SENTINEL,
                    },
                },
            };
            unsafe { SendInput(&[input], core::mem::size_of::<INPUT>() as i32) };
        }

        /// Release every key/button currently held (session end or control→view
        /// drop). Button-ups are sent without a preceding move — cursor stays put.
        pub fn release_all(&mut self) {
            for code in self.down_keys.drain().collect::<Vec<_>>() {
                if let Some(hit) = scancode::lookup(&code) {
                    self.emit_scancode(hit, false);
                }
            }
            for id in self.down_buttons.drain().collect::<Vec<_>>() {
                let up = match id {
                    0 => MOUSEEVENTF_LEFTUP,
                    1 => MOUSEEVENTF_MIDDLEUP,
                    2 => MOUSEEVENTF_RIGHTUP,
                    _ => continue,
                };
                self.send_mouse(up, 0, 0, 0);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::Injector;

// Non-Windows stub so the crate type-checks on any host (tests of `scancode`
// still run). The real product is Windows-only.
#[cfg(not(windows))]
#[derive(Default)]
pub struct Injector;

#[cfg(not(windows))]
impl Injector {
    pub fn new() -> Self {
        Self
    }
    pub fn set_target_rect(&mut self, _rect: Option<TargetRect>) {}
    pub fn dispatch(&mut self, _msg: &InputMsg) {}
    pub fn release_all(&mut self) {}
}
