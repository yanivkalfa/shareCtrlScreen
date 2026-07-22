//! Native D3D11 video child window (Plan 04 §7, Option A). Tauri's `wry` uses
//! the *windowed* WebView2 controller, so a native child HWND sits as an opaque
//! rectangle over/under the web content ("airspace" limit). We create a child
//! window parented to the Tauri window; the web UI frames it (chrome in the top
//! bar, not over the pixels). The swapchain ([`crate::Renderer`]) is created on
//! this child HWND, not the WebView2 window, so the two never fight for a surface.
//!
//! The window is created on the caller's thread (the app's UI thread, which owns
//! the message pump) so `WM_SIZE`/paint are serviced by the parent's loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, MoveWindow, RegisterClassW, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, HMENU, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
};

const CLASS_NAME: PCWSTR = w!("ShareCtrlVideoWindow");
static REGISTER: std::sync::Once = std::sync::Once::new();

/// A captured input event from the video window, normalized but not yet mapped
/// to DOM codes (the engine maps `scancode`→`KeyboardEvent.code`, §7/§8a).
#[derive(Debug, Clone, Copy)]
pub enum VideoInput {
    /// Mouse move, normalized `[0,1]` over the video area.
    Move { nx: f64, ny: f64 },
    /// Mouse button (0=left,1=middle,2=right) at a normalized position.
    Button {
        button: u8,
        down: bool,
        nx: f64,
        ny: f64,
    },
    /// Wheel in Windows units (multiples of ±120).
    Wheel { dx: i32, dy: i32 },
    /// Keyboard: Set-1 scan code + extended flag + up/down.
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
}

/// The engine installs a sink while a viewer session with control is active; the
/// wndproc forwards captured input to it. `None` ⇒ input is dropped (view-only
/// or no session), so we never capture when we shouldn't.
static INPUT_SINK: Mutex<Option<Sender<VideoInput>>> = Mutex::new(None);

/// The rendered video's pixel size (`w<<32 | h`), published by the renderer each
/// frame. Input normalization MUST use it: the video is drawn in an
/// aspect-preserving letterbox rect inside the window, so a click's position has
/// to be normalized over that rect — not the whole window — or it lands offset by
/// the black-bar size (the "clicks 1cm off" bug). 0 ⇒ not yet known (full window).
static VIDEO_DIMS: AtomicU64 = AtomicU64::new(0);

/// Renderer publishes the current video size so input maps to the letterbox rect.
pub fn set_video_size(w: u32, h: u32) {
    VIDEO_DIMS.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
}

fn video_dims() -> (f64, f64) {
    let v = VIDEO_DIMS.load(Ordering::Relaxed);
    ((v >> 32) as u32 as f64, (v & 0xffff_ffff) as u32 as f64)
}

/// Hover-reveal state for the session menu bar (§7): the native video surface
/// covers the whole client area *including* the web UI's session bar, so pushing
/// the cursor to the top edge slides the video down to reveal the bar (with its
/// Disconnect/settings buttons), and moving back into the video slides it up.
/// 0 = bar hidden (video at y=0); >0 = revealed by that many physical px.
static REVEAL_OFFSET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// Session menu bar height in CSS px (the web `.view-bar`); scaled by DPI.
const BAR_CSS_PX: i32 = 40;
/// Cursor deeper than this many physical px into the video re-hides the bar.
const UNREVEAL_DEPTH_PX: i32 = 56;

/// The bar height in physical pixels for this window's monitor DPI.
fn bar_phys(hwnd: HWND) -> i32 {
    // SAFETY: valid HWND; returns 96 on failure paths.
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi as i32 };
    BAR_CSS_PX * dpi / 96
}

/// Slide the video down (reveal the web session bar) or back up.
fn set_reveal(hwnd: HWND, reveal: bool) {
    use std::sync::atomic::Ordering;
    let bar = if reveal { bar_phys(hwnd) } else { 0 };
    if REVEAL_OFFSET.swap(bar, Ordering::SeqCst) == bar {
        return; // no change
    }
    // SAFETY: valid child HWND; parent lookup + move on the UI thread (wndproc).
    unsafe {
        if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
            let mut rc = RECT::default();
            if GetClientRect(parent, &mut rc).is_ok() {
                let w = (rc.right - rc.left).max(1);
                let h = (rc.bottom - rc.top).max(1);
                let _ = MoveWindow(hwnd, 0, bar, w, (h - bar).max(1), true);
            }
        }
    }
}

/// Keys/buttons currently held down inside the video window, so a focus loss
/// (Alt+Tab away, Win key, app switch) can synthesize the matching releases.
/// Without this the host receives `Alt down` but never `Alt up` — the classic
/// remote-desktop stuck-modifier: Windows takes the focus mid-combo and the
/// key-up lands in some other window, never reaching our wndproc.
static HELD_KEYS: Mutex<Vec<(u16, bool)>> = Mutex::new(Vec::new());
static HELD_BUTTONS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Last normalized cursor position (for synthesized button releases).
static LAST_POS: Mutex<(f64, f64)> = Mutex::new((0.5, 0.5));

/// Synthesize key-up / button-up for everything currently held (focus loss or
/// session end) so the host never keeps a stuck modifier.
fn release_all_held() {
    let keys: Vec<(u16, bool)> = HELD_KEYS
        .lock()
        .map(|mut v| v.drain(..).collect())
        .unwrap_or_default();
    let buttons: Vec<u8> = HELD_BUTTONS
        .lock()
        .map(|mut v| v.drain(..).collect())
        .unwrap_or_default();
    if keys.is_empty() && buttons.is_empty() {
        return;
    }
    let (nx, ny) = LAST_POS.lock().map(|p| *p).unwrap_or((0.5, 0.5));
    tracing::debug!(
        "video window: focus lost — releasing {} held key(s), {} button(s)",
        keys.len(),
        buttons.len()
    );
    for (scancode, extended) in keys {
        emit(VideoInput::Key {
            scancode,
            extended,
            down: false,
        });
    }
    for button in buttons {
        emit(VideoInput::Button {
            button,
            down: false,
            nx,
            ny,
        });
    }
}

/// Install the input sink (viewer session start).
pub fn set_input_sink(tx: Sender<VideoInput>) {
    *INPUT_SINK.lock().unwrap() = Some(tx);
}

/// Remove the input sink (session end / view-only).
pub fn clear_input_sink() {
    *INPUT_SINK.lock().unwrap() = None;
    // Stale held state must not leak into the next session.
    if let Ok(mut k) = HELD_KEYS.lock() {
        k.clear();
    }
    if let Ok(mut b) = HELD_BUTTONS.lock() {
        b.clear();
    }
}

fn emit(ev: VideoInput) {
    if let Ok(guard) = INPUT_SINK.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(ev);
        }
    }
}

/// A child window used purely as the D3D11 present target.
pub struct VideoWindow {
    hwnd: HWND,
    parent: HWND,
}

// The HWND is only touched from the UI thread, but we send the raw value across
// threads to the render thread as an `isize` (see engine), not this struct.
impl VideoWindow {
    /// Create a hidden child window filling the parent's client area.
    pub fn create(parent: HWND) -> Result<Self, windows::core::Error> {
        register_class();
        // SAFETY: valid parent HWND; class registered above.
        let hwnd = unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let mut rc = RECT::default();
            let _ = GetClientRect(parent, &mut rc);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                PCWSTR::null(),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                (rc.right - rc.left).max(1),
                (rc.bottom - rc.top).max(1),
                Some(parent),
                None::<HMENU>,
                Some(hinstance.into()),
                None,
            )?
        };
        Ok(Self { hwnd, parent })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Raw handle value to hand across threads (to the render thread).
    pub fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Keep a reference to the parent alive in the struct (used for fit).
    pub fn parent(&self) -> HWND {
        self.parent
    }
}

/// Show the video surface and size it to the parent's client area (session
/// start). Safe to call from the render thread — `ShowWindow`/`MoveWindow`
/// marshal to the owning thread.
///
/// The child is raised to the TOP of the sibling z-order: the WebView2 child
/// (created by Tauri *after* this window) otherwise sits above it, and the video
/// presents perfectly — invisibly — behind the web page ("airspace", §7).
pub fn show(hwnd_raw: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_TOP, SWP_SHOWWINDOW};
    let hwnd = HWND(hwnd_raw as *mut _);
    REVEAL_OFFSET.store(0, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: valid child HWND created by `VideoWindow::create`.
    unsafe {
        let parent = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd);
        let mut w = 1;
        let mut h = 1;
        if let Ok(parent) = parent {
            let mut rc = RECT::default();
            if GetClientRect(parent, &mut rc).is_ok() {
                w = (rc.right - rc.left).max(1);
                h = (rc.bottom - rc.top).max(1);
            }
        }
        tracing::info!("video window: show {w}x{h} (raise to top)");
        let _ = MoveWindow(hwnd, 0, 0, w, h, true);
        // Raise above the WebView2 sibling AND show in one call.
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, w, h, SWP_SHOWWINDOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

/// Keep the video child sized to the parent's client area (minus any hover
/// reveal offset). Called periodically from the render loop so mid-session
/// window resizes track without a WM_SIZE hook into the Tauri parent; no-ops
/// when the size already matches.
pub fn fit(hwnd_raw: isize) {
    use std::sync::atomic::Ordering;
    let hwnd = HWND(hwnd_raw as *mut _);
    let offset = REVEAL_OFFSET.load(Ordering::SeqCst);
    // SAFETY: valid child HWND; reads two client rects and conditionally moves.
    unsafe {
        let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) else {
            return;
        };
        let mut prc = RECT::default();
        let mut crc = RECT::default();
        if GetClientRect(parent, &mut prc).is_err() || GetClientRect(hwnd, &mut crc).is_err() {
            return;
        }
        let want_w = (prc.right - prc.left).max(1);
        let want_h = ((prc.bottom - prc.top) - offset).max(1);
        let cur_w = crc.right - crc.left;
        let cur_h = crc.bottom - crc.top;
        if cur_w != want_w || cur_h != want_h {
            let _ = MoveWindow(hwnd, 0, offset, want_w, want_h, true);
        }
    }
}

/// Hide the video surface (back to the home screen / settings overlay).
pub fn hide(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut _);
    REVEAL_OFFSET.store(0, std::sync::atomic::Ordering::SeqCst);
    VIDEO_DIMS.store(0, Ordering::Relaxed); // avoid stale mapping next session
                                            // SAFETY: valid child HWND.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn register_class() {
    REGISTER.call_once(|| {
        // SAFETY: one-time class registration with a static class name.
        unsafe {
            let hinstance = GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: CLASS_NAME,
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    });
}

// The present loop drives D3D; the wndproc captures input over the video area
// (§7) and forwards it to the engine, which relays it to the host. Capture is
// gated by the sink being installed — only during a viewer session with control.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Normalized cursor position from lParam (signed 16-bit lo/hi), mapped over
    // the LETTERBOX rect (must match the renderer's letterbox exactly), so clicks
    // land where the cursor is on the remote — not offset by the black bars.
    let norm_pos = |lp: LPARAM| -> (f64, f64) {
        let px = (lp.0 & 0xffff) as i16 as f64;
        let py = ((lp.0 >> 16) & 0xffff) as i16 as f64;
        let mut rc = RECT::default();
        // SAFETY: valid HWND.
        let _ = unsafe { GetClientRect(hwnd, &mut rc) };
        let cw = (rc.right - rc.left).max(1) as f64;
        let ch = (rc.bottom - rc.top).max(1) as f64;
        let (vw, vh) = video_dims();
        if vw <= 0.0 || vh <= 0.0 {
            return ((px / cw).clamp(0.0, 1.0), (py / ch).clamp(0.0, 1.0));
        }
        // Same letterbox math as the renderer: scale to fit, center.
        let scale = (cw / vw).min(ch / vh);
        let (dispw, disph) = (vw * scale, vh * scale);
        let offx = (cw - dispw) * 0.5;
        let offy = (ch - disph) * 0.5;
        (
            ((px - offx) / dispw).clamp(0.0, 1.0),
            ((py - offy) / disph).clamp(0.0, 1.0),
        )
    };

    match msg {
        WM_MOUSEMOVE => {
            // Hover-reveal of the session menu bar (§7): top edge → slide the
            // video down to expose the web bar; deep re-entry → slide back up.
            {
                use std::sync::atomic::Ordering;
                let y_phys = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                let revealed = REVEAL_OFFSET.load(Ordering::SeqCst) > 0;
                if !revealed && y_phys <= 2 {
                    set_reveal(hwnd, true);
                } else if revealed && y_phys > UNREVEAL_DEPTH_PX {
                    set_reveal(hwnd, false);
                }
            }
            let (nx, ny) = norm_pos(lparam);
            if let Ok(mut p) = LAST_POS.lock() {
                *p = (nx, ny);
            }
            emit(VideoInput::Move { nx, ny });
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            // SAFETY: take keyboard focus so key events flow to this window.
            let _ = unsafe { SetFocus(Some(hwnd)) };
            let (nx, ny) = norm_pos(lparam);
            let button = match msg {
                WM_LBUTTONDOWN => 0,
                WM_MBUTTONDOWN => 1,
                _ => 2,
            };
            if let Ok(mut held) = HELD_BUTTONS.lock() {
                if !held.contains(&button) {
                    held.push(button);
                }
            }
            emit(VideoInput::Button {
                button,
                down: true,
                nx,
                ny,
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (nx, ny) = norm_pos(lparam);
            let button = match msg {
                WM_LBUTTONUP => 0,
                WM_MBUTTONUP => 1,
                _ => 2,
            };
            if let Ok(mut held) = HELD_BUTTONS.lock() {
                held.retain(|&b| b != button);
            }
            emit(VideoInput::Button {
                button,
                down: false,
                nx,
                ny,
            });
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let dy = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
            emit(VideoInput::Wheel { dx: 0, dy });
            return LRESULT(0);
        }
        WM_MOUSEHWHEEL => {
            let dx = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
            emit(VideoInput::Wheel { dx, dy: 0 });
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let scancode = ((lparam.0 >> 16) & 0xff) as u16;
            let extended = (lparam.0 & (1 << 24)) != 0;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            if let Ok(mut held) = HELD_KEYS.lock() {
                if down {
                    if !held.contains(&(scancode, extended)) {
                        held.push((scancode, extended));
                    }
                } else {
                    held.retain(|&k| k != (scancode, extended));
                }
            }
            emit(VideoInput::Key {
                scancode,
                extended,
                down,
            });
            return LRESULT(0);
        }
        // Focus left the video window mid-combo (Alt+Tab away, Win key, app
        // switch): the key-ups will land elsewhere, so synthesize them NOW or
        // the host keeps a stuck modifier.
        WM_KILLFOCUS => {
            release_all_held();
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
