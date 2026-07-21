//! Native D3D11 video child window (Plan 04 §7, Option A). Tauri's `wry` uses
//! the *windowed* WebView2 controller, so a native child HWND sits as an opaque
//! rectangle over/under the web content ("airspace" limit). We create a child
//! window parented to the Tauri window; the web UI frames it (chrome in the top
//! bar, not over the pixels). The swapchain ([`crate::Renderer`]) is created on
//! this child HWND, not the WebView2 window, so the two never fight for a surface.
//!
//! The window is created on the caller's thread (the app's UI thread, which owns
//! the message pump) so `WM_SIZE`/paint are serviced by the parent's loop.

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
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_CHILD,
    WS_CLIPSIBLINGS,
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

/// Install the input sink (viewer session start).
pub fn set_input_sink(tx: Sender<VideoInput>) {
    *INPUT_SINK.lock().unwrap() = Some(tx);
}

/// Remove the input sink (session end / view-only).
pub fn clear_input_sink() {
    *INPUT_SINK.lock().unwrap() = None;
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
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_SHOWWINDOW,
    };
    let hwnd = HWND(hwnd_raw as *mut _);
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

/// Hide the video surface (back to the home screen).
pub fn hide(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut _);
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
    // Normalized cursor position from lParam (signed 16-bit lo/hi).
    let norm_pos = |lp: LPARAM| -> (f64, f64) {
        let x = (lp.0 & 0xffff) as i16 as f64;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f64;
        let mut rc = RECT::default();
        // SAFETY: valid HWND.
        let _ = unsafe { GetClientRect(hwnd, &mut rc) };
        let w = (rc.right - rc.left).max(1) as f64;
        let h = (rc.bottom - rc.top).max(1) as f64;
        ((x / w).clamp(0.0, 1.0), (y / h).clamp(0.0, 1.0))
    };

    match msg {
        WM_MOUSEMOVE => {
            let (nx, ny) = norm_pos(lparam);
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
            emit(VideoInput::Key {
                scancode,
                extended,
                down,
            });
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
