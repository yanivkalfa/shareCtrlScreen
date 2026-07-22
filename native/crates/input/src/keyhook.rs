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

/// Top-level window of the viewer session (raw HWND). The hook only suppresses
/// combos while this window is FOREGROUND — matching keyhook.js, which installed
/// on focus and removed on blur. Clicking any other window instantly gives the
/// user their keyboard back (§8a guaranteed escape). 0 = gate open never.
static FOCUS_ROOT: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Record the session window: pass the video child HWND (raw); its top-level
/// ancestor becomes the focus gate. Pass 0 to clear (session end).
pub fn set_focus_root(child_hwnd_raw: isize) {
    use std::sync::atomic::Ordering;
    if child_hwnd_raw == 0 {
        FOCUS_ROOT.store(0, Ordering::SeqCst);
        return;
    }
    // SAFETY: valid HWND from the video window; GA_ROOT walk is read-only.
    let root = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetAncestor(
            windows::Win32::Foundation::HWND(child_hwnd_raw as *mut _),
            windows::Win32::UI::WindowsAndMessaging::GA_ROOT,
        )
    };
    FOCUS_ROOT.store(root.0 as isize, Ordering::SeqCst);
}

/// Is the session window currently foreground?
fn session_focused() -> bool {
    use std::sync::atomic::Ordering;
    let root = FOCUS_ROOT.load(Ordering::SeqCst);
    if root == 0 {
        return false;
    }
    // SAFETY: pure state read.
    let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    fg.0 as isize == root
}

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
    if injected {
        return fallthrough();
    }

    // If we synthesized an Alt-down for the remote (see below), mirror the REAL
    // Alt release to the remote — the video window may not have keyboard focus,
    // so its wndproc can't be relied on for the matching key-up. Never
    // suppressed locally.
    {
        use std::sync::atomic::Ordering;
        if info.vkCode as i32 == VK_MENU && is_up && SYNTH_ALT.swap(false, Ordering::SeqCst) {
            forward_key("AltLeft", false);
        }
    }

    // INVARIANT (stuck-key prevention): once we forwarded a key-DOWN to the
    // remote, the matching key-UP is forwarded UNCONDITIONALLY — even if the
    // suppress conditions no longer hold. E.g. quick Alt+Tab releases Alt
    // *before* Tab: Tab-up then fails the alt_down() test; or focus moved off
    // mid-Win-press. Without this the host keeps the key held forever.
    if is_up {
        if let Some(code) = vk_to_code(info.vkCode) {
            if hook_held_remove(code) {
                forward_key(code, false);
                // Suppress the up locally only where the down was suppressed.
                if should_suppress(info.vkCode) && session_focused() {
                    return LRESULT(1);
                }
                return fallthrough();
            }
        }
    }

    if !should_suppress(info.vkCode) {
        return fallthrough();
    }
    // Focus gate: only act while the session window is foreground. Anywhere
    // else on this machine, Alt+Tab/Win behave completely normally.
    if !session_focused() {
        return fallthrough();
    }

    if let Some(code) = vk_to_code(info.vkCode) {
        // Alt+Tab / Alt+Esc: the remote only opens its switcher if it has Alt
        // down. The user's Alt may have gone to the WebView (not the video
        // window), so guarantee it: synthesize AltLeft-down once per hold.
        {
            use std::sync::atomic::Ordering;
            let vk = info.vkCode as i32;
            if (vk == VK_TAB || vk == VK_ESCAPE)
                && is_down
                && !SYNTH_ALT.swap(true, Ordering::SeqCst)
            {
                forward_key("AltLeft", true);
            }
        }
        if is_down {
            hook_held_insert(code);
        }
        forward_key(code, is_down);
    }
    LRESULT(1) // non-zero => suppress locally
}

/// Whether we synthesized an AltLeft-down for the remote that still needs its
/// matching release when the user lets go of the real Alt.
static SYNTH_ALT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keys whose DOWN we forwarded to the remote and whose UP is therefore owed
/// unconditionally (see the invariant in `hook_proc`).
static HOOK_HELD: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn hook_held_insert(code: &'static str) {
    if let Ok(mut held) = HOOK_HELD.lock() {
        if !held.contains(&code) {
            held.push(code);
        }
    }
}

fn hook_held_remove(code: &str) -> bool {
    if let Ok(mut held) = HOOK_HELD.lock() {
        if let Some(i) = held.iter().position(|&c| c == code) {
            held.remove(i);
            return true;
        }
    }
    false
}

fn forward_key(code: &str, down: bool) {
    if let Ok(state) = STATE.lock() {
        if let Some(f) = state.forward.as_ref() {
            // Forwarding must never break the keyboard.
            f(code, down);
        }
    }
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
    set_focus_root(0);
    SYNTH_ALT.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut held) = HOOK_HELD.lock() {
        held.clear(); // host releases everything at session end anyway
    }
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
