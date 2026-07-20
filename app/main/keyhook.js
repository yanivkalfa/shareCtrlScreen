'use strict';

// Low-level keyboard hook (WH_KEYBOARD_LL) via koffi — the ONLY file with the
// hook. Purpose: while the viewer is focused and controlling a remote, capture
// OS-reserved combos (Alt+Tab, Alt+Esc, the Win keys) LOCALLY so Windows does
// not act on them, and forward them to the remote instead.
//
// SAFETY (this is global input machinery — treat with care):
//   * Disabled by default; installed only while enabled AND the window is
//     focused; the caller uninstalls on blur/teardown/exit. "Click away" is a
//     guaranteed escape — losing focus removes the hook.
//   * install() is wrapped so any koffi/Win32 error just disables the feature.
//   * The hook proc body is wrapped and ALWAYS falls through to CallNextHookEx
//     on any error, so a bug can never swallow the keyboard.
//   * Windows' own ~300 ms low-level-hook timeout is a final backstop: a slow
//     proc is bypassed by the OS rather than freezing input.
//   * We never suppress Ctrl+Alt+Del (the kernel handles it below any hook).

const koffi = require('koffi');

const user32 = koffi.load('user32.dll');
const kernel32 = koffi.load('kernel32.dll');

// KBDLLHOOKSTRUCT — the payload behind the hook's lParam.
const KBDLLHOOKSTRUCT = koffi.struct('KBDLLHOOKSTRUCT', {
  vkCode: 'uint32',
  scanCode: 'uint32',
  flags: 'uint32',
  time: 'uint32',
  dwExtraInfo: 'uintptr_t'
});

// LRESULT CALLBACK LowLevelKeyboardProc(int, WPARAM, LPARAM). lParam kept as a
// raw pointer so we can BOTH decode it and pass it on to CallNextHookEx.
const LLKeyProc = koffi.proto(
  'intptr_t __stdcall LLKeyProc(int nCode, uintptr_t wParam, void *lParam)'
);

const SetWindowsHookExW = user32.func(
  'void* SetWindowsHookExW(int idHook, void *lpfn, void *hmod, uint32 dwThreadId)'
);
const UnhookWindowsHookEx = user32.func('bool UnhookWindowsHookEx(void *hhk)');
const CallNextHookEx = user32.func(
  'intptr_t CallNextHookEx(void *hhk, int nCode, uintptr_t wParam, void *lParam)'
);
const GetAsyncKeyState = user32.func('int16 GetAsyncKeyState(int vKey)');
const GetModuleHandleW = kernel32.func('void* GetModuleHandleW(void *lpModuleName)');

const WH_KEYBOARD_LL = 13;
const WM_KEYDOWN = 0x0100;
const WM_KEYUP = 0x0101;
const WM_SYSKEYDOWN = 0x0104;
const WM_SYSKEYUP = 0x0105;
const LLKHF_INJECTED = 0x10;

const VK_TAB = 0x09;
const VK_ESCAPE = 0x1b;
const VK_MENU = 0x12; // Alt
const VK_LWIN = 0x5b;
const VK_RWIN = 0x5c;

// Win32 virtual key -> DOM KeyboardEvent.code, for the small suppressed set.
const VK_TO_CODE = {
  [VK_LWIN]: 'MetaLeft',
  [VK_RWIN]: 'MetaRight',
  [VK_TAB]: 'Tab',
  [VK_ESCAPE]: 'Escape'
};

function altDown() {
  return (GetAsyncKeyState(VK_MENU) & 0x8000) !== 0;
}

// Which keys we grab locally. Win keys always; Tab/Esc only while Alt is held
// (Alt+Tab, Alt+Esc). Everything else passes through untouched.
function shouldSuppress(vk) {
  if (vk === VK_LWIN || vk === VK_RWIN) return true;
  if ((vk === VK_TAB || vk === VK_ESCAPE) && altDown()) return true;
  return false;
}

let hHook = null;
let procPtr = null;
let forwardFn = null;

function hookCallback(nCode, wParam, lParam) {
  try {
    if (nCode < 0 || !hHook) return CallNextHookEx(null, nCode, wParam, lParam);

    const msg = Number(wParam);
    const isDown = msg === WM_KEYDOWN || msg === WM_SYSKEYDOWN;
    const isUp = msg === WM_KEYUP || msg === WM_SYSKEYUP;

    if (isDown || isUp) {
      const info = koffi.decode(lParam, KBDLLHOOKSTRUCT);
      // Ignore our own injected input so we can't feed ourselves a loop.
      const injected = (info.flags & LLKHF_INJECTED) !== 0;
      if (!injected && shouldSuppress(info.vkCode)) {
        const code = VK_TO_CODE[info.vkCode];
        if (code && forwardFn) {
          try {
            forwardFn(code, isDown);
          } catch (_) {
            /* forwarding must never break the keyboard */
          }
        }
        return 1; // non-zero => suppress locally
      }
    }
  } catch (_) {
    // Any failure: fall through and pass the key on untouched.
  }
  return CallNextHookEx(null, nCode, wParam, lParam);
}

// Install the hook. `forward(code, isDown)` is called for each suppressed key so
// the caller can relay it to the remote. Returns true on success. Idempotent.
function install(forward) {
  if (hHook) {
    forwardFn = forward;
    return true;
  }
  try {
    forwardFn = forward;
    if (!procPtr) procPtr = koffi.register(hookCallback, koffi.pointer(LLKeyProc));
    const hmod = GetModuleHandleW(null);
    hHook = SetWindowsHookExW(WH_KEYBOARD_LL, procPtr, hmod, 0);
    if (!hHook) {
      forwardFn = null;
      return false;
    }
    return true;
  } catch (err) {
    console.error('[keyhook] install failed; feature disabled', err);
    hHook = null;
    forwardFn = null;
    return false;
  }
}

function uninstall() {
  if (hHook) {
    try {
      UnhookWindowsHookEx(hHook);
    } catch (_) {}
    hHook = null;
  }
  forwardFn = null;
  // procPtr is kept registered for cheap reinstall on the next focus.
}

function isInstalled() {
  return !!hHook;
}

module.exports = { install, uninstall, isInstalled };
