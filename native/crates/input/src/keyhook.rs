//! Low-level keyboard hook (`WH_KEYBOARD_LL`) ported from `app/main/keyhook.js`
//! (Plan 04 §8a). While the viewer is focused and controlling a remote, capture
//! OS-reserved combos (Alt+Tab, Alt+Esc, the Win keys) **locally** so Windows
//! does not act on them, and forward them to the remote instead.
//!
//! SAFETY posture (global input machinery — matches the JS original):
//!   * Disabled by default; installed only while the caller wants it and the
//!     window is focused; uninstall on blur/teardown/exit. Losing focus removes
//!     the hook — "click away" is a guaranteed escape.
//!   * The hook proc always falls through to `CallNextHookEx` on any error, so a
//!     bug can never swallow the keyboard. Windows' ~300 ms hook timeout is the
//!     final backstop.
//!   * We never suppress Ctrl+Alt+Del (the kernel handles it below any hook).
//!   * Our own injected input is ignored (via `LLKHF_INJECTED` and the
//!     `INJECT_SENTINEL` tag) so we cannot feed ourselves a loop.

use std::sync::Mutex;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const VK_TAB: i32 = 0x09;
const VK_ESCAPE: i32 = 0x1b;
const VK_MENU: i32 = 0x12; // Alt
const VK_LWIN: i32 = 0x5b;
const VK_RWIN: i32 = 0x5c;

/// Callback invoked for each suppressed key so the caller can relay it. Boxed
/// behind a mutex because the OS calls our `extern "system"` proc with no state.
type Forward = Box<dyn Fn(&str, bool) + Send>;

struct HookState {
    /// Raw `HHOOK` value (isize) so the static stays `Send`; 0 = not installed.
    hook: isize,
    forward: Option<Forward>,
}

static STATE: Mutex<HookState> = Mutex::new(HookState {
    hook: 0,
    forward: None,
});

/// Win32 virtual key → DOM `KeyboardEvent.code` for the small suppressed set.
fn vk_to_code(vk: u32) -> Option<&'static str> {
    match vk as i32 {
        VK_LWIN => Some("MetaLeft"),
        VK_RWIN => Some("MetaRight"),
        VK_TAB => Some("Tab"),
        VK_ESCAPE => Some("Escape"),
        _ => None,
    }
}

fn alt_down() -> bool {
    // SAFETY: pure key-state read.
    (unsafe { GetAsyncKeyState(VK_MENU) } as u16 & 0x8000) != 0
}

/// Win keys always; Tab/Esc only while Alt is held. Everything else passes
/// through untouched.
fn should_suppress(vk: u32) -> bool {
    let vk = vk as i32;
    if vk == VK_LWIN || vk == VK_RWIN {
        return true;
    }
    if (vk == VK_TAB || vk == VK_ESCAPE) && alt_down() {
        return true;
    }
    false
}

unsafe extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    // Any failure: fall through and pass the key on untouched.
    let fallthrough = || unsafe { CallNextHookEx(None, n_code, w_param, l_param) };

    if n_code < 0 {
        return fallthrough();
    }

    let msg = w_param.0 as u32;
    let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !(is_down || is_up) {
        return fallthrough();
    }

    // SAFETY: for a keyboard hook with n_code >= 0, lParam is a KBDLLHOOKSTRUCT*.
    let info = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };

    // Ignore our own injected input so we can't feed ourselves a loop.
    let injected =
        (info.flags.0 & LLKHF_INJECTED.0) != 0 || info.dwExtraInfo == super::INJECT_SENTINEL;
    if injected || !should_suppress(info.vkCode) {
        return fallthrough();
    }

    if let Some(code) = vk_to_code(info.vkCode) {
        if let Ok(state) = STATE.lock() {
            if let Some(f) = state.forward.as_ref() {
                // Forwarding must never break the keyboard.
                f(code, is_down);
            }
        }
    }
    LRESULT(1) // non-zero => suppress locally
}

/// Install the hook. `forward(code, is_down)` is called for each suppressed key.
/// Idempotent: re-installing just swaps the forward closure. Returns false on
/// failure (feature silently disabled). Must be called on a thread that runs a
/// message pump (see [`crate::keyhook`] module docs).
pub fn install(forward: Forward) -> bool {
    let mut state = match STATE.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if state.hook != 0 {
        state.forward = Some(forward);
        return true;
    }
    // SAFETY: GetModuleHandleW(None) returns this module's base; valid for a
    // global (thread-id 0) low-level hook.
    let hmod: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let hhook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hmod), 0) } {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("keyhook install failed; feature disabled: {e}");
            return false;
        }
    };
    state.hook = hhook.0 as isize;
    state.forward = Some(forward);
    true
}

/// Remove the hook. Safe to call when not installed.
pub fn uninstall() {
    if let Ok(mut state) = STATE.lock() {
        if state.hook != 0 {
            // SAFETY: valid HHOOK we installed.
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(state.hook as *mut _)) };
            state.hook = 0;
        }
        state.forward = None;
    }
}

/// Whether the hook is currently installed.
pub fn is_installed() -> bool {
    STATE.lock().map(|s| s.hook != 0).unwrap_or(false)
}

/// Run a message pump on the current thread until `stop` is set. A low-level
/// keyboard hook only delivers events to a thread that pumps messages, so the
/// thread that called [`install`] must call this. Returns when `stop` is true.
pub fn message_pump(stop: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let mut msg = MSG::default();
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: standard non-blocking message drain.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
