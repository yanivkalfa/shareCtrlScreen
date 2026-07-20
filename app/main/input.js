'use strict';

// Win32 SendInput via koffi — plan §7. This is the ONLY file that touches koffi.
const koffi = require('koffi');

const user32 = koffi.load('user32.dll');

// --- §7.1 declarations ------------------------------------------------------
// koffi computes x64 alignment; do NOT hand-pack these.
const MOUSEINPUT = koffi.struct('MOUSEINPUT', {
  dx: 'long',
  dy: 'long',
  // Win32 declares DWORD, but int32 lets negative wheel deltas pass straight
  // through — identical byte layout.
  mouseData: 'int32',
  dwFlags: 'uint32',
  time: 'uint32',
  dwExtraInfo: 'uintptr_t'
});

const KEYBDINPUT = koffi.struct('KEYBDINPUT', {
  wVk: 'uint16',
  wScan: 'uint16',
  dwFlags: 'uint32',
  time: 'uint32',
  dwExtraInfo: 'uintptr_t'
});

const HARDWAREINPUT = koffi.struct('HARDWAREINPUT', {
  uMsg: 'uint32',
  wParamL: 'uint16',
  wParamH: 'uint16'
});

const INPUT_U = koffi.union('INPUT_U', {
  mi: MOUSEINPUT,
  ki: KEYBDINPUT,
  hi: HARDWAREINPUT
});

const INPUT = koffi.struct('INPUT', {
  type: 'uint32',
  u: INPUT_U
});

const SendInput = user32.func('uint32 SendInput(uint32 cInputs, INPUT *pInputs, int cbSize)');
const GetSystemMetrics = user32.func('int GetSystemMetrics(int nIndex)');

// Virtual-desktop metrics (physical px for a per-monitor-DPI-aware process).
const SM_XVIRTUALSCREEN = 76;
const SM_YVIRTUALSCREEN = 77;
const SM_CXVIRTUALSCREEN = 78;
const SM_CYVIRTUALSCREEN = 79;

// Must be 40 on x64; a different size means the struct layout is wrong and
// every injected event would be garbage.
const CB_SIZE = koffi.sizeof(INPUT);
if (CB_SIZE !== 40) {
  throw new Error(`koffi sizeof(INPUT) = ${CB_SIZE}, expected 40 (x64 layout mismatch)`);
}

const INPUT_MOUSE = 0;
const INPUT_KEYBOARD = 1;

// --- §7.2 flag constants ----------------------------------------------------
const MOUSEEVENTF_MOVE = 0x0001;
const MOUSEEVENTF_LEFTDOWN = 0x0002;
const MOUSEEVENTF_LEFTUP = 0x0004;
const MOUSEEVENTF_RIGHTDOWN = 0x0008;
const MOUSEEVENTF_RIGHTUP = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN = 0x0020;
const MOUSEEVENTF_MIDDLEUP = 0x0040;
const MOUSEEVENTF_WHEEL = 0x0800;
const MOUSEEVENTF_HWHEEL = 0x1000;
const MOUSEEVENTF_ABSOLUTE = 0x8000;
const MOUSEEVENTF_VIRTUALDESK = 0x4000;

const KEYEVENTF_EXTENDEDKEY = 0x0001;
const KEYEVENTF_KEYUP = 0x0002;
const KEYEVENTF_SCANCODE = 0x0008;

// --- §7.4 DOM KeyboardEvent.code -> Set 1 scan code -------------------------
// Non-extended.
const SC = {
  Escape: 0x01, Digit1: 0x02, Digit2: 0x03, Digit3: 0x04,
  Digit4: 0x05, Digit5: 0x06, Digit6: 0x07, Digit7: 0x08,
  Digit8: 0x09, Digit9: 0x0a, Digit0: 0x0b, Minus: 0x0c,
  Equal: 0x0d, Backspace: 0x0e, Tab: 0x0f, KeyQ: 0x10,
  KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14,
  KeyY: 0x15, KeyU: 0x16, KeyI: 0x17, KeyO: 0x18,
  KeyP: 0x19, BracketLeft: 0x1a, BracketRight: 0x1b, Enter: 0x1c,
  ControlLeft: 0x1d, KeyA: 0x1e, KeyS: 0x1f, KeyD: 0x20,
  KeyF: 0x21, KeyG: 0x22, KeyH: 0x23, KeyJ: 0x24,
  KeyK: 0x25, KeyL: 0x26, Semicolon: 0x27, Quote: 0x28,
  Backquote: 0x29, ShiftLeft: 0x2a, Backslash: 0x2b, KeyZ: 0x2c,
  KeyX: 0x2d, KeyC: 0x2e, KeyV: 0x2f, KeyB: 0x30,
  KeyN: 0x31, KeyM: 0x32, Comma: 0x33, Period: 0x34,
  Slash: 0x35, ShiftRight: 0x36, NumpadMultiply: 0x37, AltLeft: 0x38,
  Space: 0x39, CapsLock: 0x3a, F1: 0x3b, F2: 0x3c,
  F3: 0x3d, F4: 0x3e, F5: 0x3f, F6: 0x40,
  F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44,
  NumLock: 0x45, ScrollLock: 0x46, Numpad7: 0x47, Numpad8: 0x48,
  Numpad9: 0x49, NumpadSubtract: 0x4a, Numpad4: 0x4b, Numpad5: 0x4c,
  Numpad6: 0x4d, NumpadAdd: 0x4e, Numpad1: 0x4f, Numpad2: 0x50,
  Numpad3: 0x51, Numpad0: 0x52, NumpadDecimal: 0x53, IntlBackslash: 0x56,
  F11: 0x57, F12: 0x58
};

// Extended (prefixed 0xE0 on the wire; SendInput wants KEYEVENTF_EXTENDEDKEY).
const SC_EXT = {
  NumpadEnter: 0x1c, ControlRight: 0x1d,
  NumpadDivide: 0x35, AltRight: 0x38,
  Home: 0x47, ArrowUp: 0x48,
  PageUp: 0x49, ArrowLeft: 0x4b,
  ArrowRight: 0x4d, End: 0x4f,
  ArrowDown: 0x50, PageDown: 0x51,
  Insert: 0x52, Delete: 0x53,
  MetaLeft: 0x5b, MetaRight: 0x5c,
  ContextMenu: 0x5d, PrintScreen: 0x37
};

// Deliberately unsupported (§7.4): Pause (0xE1 prefix sequence), media keys, Fn.
function lookup(code) {
  if (Object.prototype.hasOwnProperty.call(SC, code)) return { sc: SC[code], ext: false };
  if (Object.prototype.hasOwnProperty.call(SC_EXT, code)) return { sc: SC_EXT[code], ext: true };
  return null;
}

// --- injected-state tracking ------------------------------------------------
// Everything this module presses down is recorded so a dying/ending session can
// release it. Without this, a viewer that vanishes mid-keypress (or mid-drag)
// leaves the host's OS with a key or mouse button stuck down.
const downKeys = new Set();     // KeyboardEvent.code strings currently injected down
const downButtons = new Set();  // button ids (0/1/2) currently injected down

// --- §7.3 functions ---------------------------------------------------------
function mouseInput(fields) {
  return {
    type: INPUT_MOUSE,
    u: {
      mi: {
        dx: 0,
        dy: 0,
        mouseData: 0,
        dwFlags: 0,
        time: 0,
        dwExtraInfo: 0,
        ...fields
      }
    }
  };
}

function send(input) {
  return SendInput(1, [input], CB_SIZE);
}

// The shared display's physical bounds { x, y, w, h }, in the same coordinate
// space as the Win32 virtual desktop. null => share the primary display (the
// default, verified path). Set via setTargetRect() at session start.
let targetRect = null;

function setTargetRect(rect) {
  if (
    rect &&
    typeof rect.x === 'number' &&
    typeof rect.y === 'number' &&
    rect.w > 0 &&
    rect.h > 0
  ) {
    targetRect = { x: rect.x, y: rect.y, w: rect.w, h: rect.h };
  } else {
    targetRect = null;
  }
}

// Normalized in, normalized out.
//   Primary (targetRect === null): plain ABSOLUTE maps [0,65535] onto the
//   primary display — the original, verified path, left byte-identical.
//   Non-primary (targetRect set): map the normalized point within the shared
//   display to an absolute virtual-desktop pixel, then normalize that against
//   the whole virtual desktop with VIRTUALDESK. All geometry is physical px.
function mouseMove(nx, ny) {
  if (!targetRect) {
    return send(
      mouseInput({
        dx: Math.round(nx * 65535),
        dy: Math.round(ny * 65535),
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
      })
    );
  }

  const vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
  const vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
  const vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
  const vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
  if (vw <= 1 || vh <= 1) {
    // Degenerate metrics — fall back to the primary path.
    return send(
      mouseInput({
        dx: Math.round(nx * 65535),
        dy: Math.round(ny * 65535),
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
      })
    );
  }

  const absX = targetRect.x + nx * targetRect.w;
  const absY = targetRect.y + ny * targetRect.h;

  return send(
    mouseInput({
      dx: Math.round(((absX - vx) * 65535) / (vw - 1)),
      dy: Math.round(((absY - vy) * 65535) / (vh - 1)),
      dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
    })
  );
}

const BUTTON_FLAGS = {
  0: { down: MOUSEEVENTF_LEFTDOWN, up: MOUSEEVENTF_LEFTUP },
  1: { down: MOUSEEVENTF_MIDDLEDOWN, up: MOUSEEVENTF_MIDDLEUP },
  2: { down: MOUSEEVENTF_RIGHTDOWN, up: MOUSEEVENTF_RIGHTUP }
};

function mouseButton(b, isDown, nx, ny) {
  const flags = BUTTON_FLAGS[b];
  if (!flags) return 0;

  if (isDown) downButtons.add(b);
  else downButtons.delete(b);

  // Move first so the click lands exactly where aimed even if a preceding
  // 'mm' was dropped by the unreliable channel.
  mouseMove(nx, ny);
  return send(mouseInput({ dwFlags: isDown ? flags.down : flags.up }));
}

// Values arrive already in Windows wheel units (multiples of ±120) from the viewer.
function wheel(dx, dy) {
  let n = 0;
  if (dy !== 0) n += send(mouseInput({ dwFlags: MOUSEEVENTF_WHEEL, mouseData: dy }));
  if (dx !== 0) n += send(mouseInput({ dwFlags: MOUSEEVENTF_HWHEEL, mouseData: dx }));
  return n;
}

// Scan-code injection keeps this layout-independent.
function key(code, isDown) {
  const hit = lookup(code);
  if (!hit) return 0; // unknown code -> silently ignored

  if (isDown) downKeys.add(code);
  else downKeys.delete(code);

  return send({
    type: INPUT_KEYBOARD,
    u: {
      ki: {
        wVk: 0,
        wScan: hit.sc,
        dwFlags:
          KEYEVENTF_SCANCODE |
          (hit.ext ? KEYEVENTF_EXTENDEDKEY : 0) |
          (isDown ? 0 : KEYEVENTF_KEYUP),
        time: 0,
        dwExtraInfo: 0
      }
    }
  });
}

// Release every key and mouse button this module currently holds down.
// Called on session end and on a control->view permission drop, so a viewer
// that vanished (or was cut off) can never leave the host with stuck input.
// Button-ups are sent WITHOUT a preceding move — the cursor stays where it is.
function releaseAll() {
  let n = 0;

  for (const code of Array.from(downKeys)) {
    n += key(code, false); // key() removes it from the set
  }
  downKeys.clear();

  for (const b of Array.from(downButtons)) {
    const flags = BUTTON_FLAGS[b];
    if (flags) n += send(mouseInput({ dwFlags: flags.up }));
  }
  downButtons.clear();

  return n;
}

module.exports = {
  mouseMove,
  mouseButton,
  wheel,
  key,
  releaseAll,
  setTargetRect,
  // exposed for diagnostics/tests
  lookup,
  CB_SIZE
};
