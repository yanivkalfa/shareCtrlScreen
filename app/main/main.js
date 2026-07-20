'use strict';

const path = require('path');
const { app, BrowserWindow, ipcMain, session, desktopCapturer, screen } = require('electron');
const config = require('./config');

// --- §4.1 startup order: --profile BEFORE app.whenReady() -------------------
// Deliberately no requestSingleInstanceLock: two instances on one machine is
// the supported test setup.
(function applyProfile() {
  const argv = process.argv;
  const i = argv.indexOf('--profile');
  if (i !== -1 && argv[i + 1]) {
    const name = argv[i + 1];
    app.setPath('userData', app.getPath('userData') + '-' + name);
  }
})();

// Belt-and-braces so the incoming <video> always plays (§4.3).
app.commandLine.appendSwitch('autoplay-policy', 'no-user-gesture-required');

let win = null;

// --- §4.2 display-media handler (host screen capture) -----------------------
function installDisplayMediaHandler() {
  session.defaultSession.setDisplayMediaRequestHandler(
    (request, callback) => {
      // Everything is wrapped: an unhandled rejection here hangs the
      // renderer's getDisplayMedia() promise forever (§11.3).
      (async () => {
        try {
          const sources = await desktopCapturer.getSources({ types: ['screen'] });
          if (!sources.length) return callback({});

          const cfg = config.load();
          const primaryId = String(screen.getPrimaryDisplay().id);

          // Share the configured monitor, falling back to the primary if it is
          // unset or no longer present (unplugged / id changed).
          let chosen = null;
          if (cfg.shareDisplayId) chosen = sources.find((s) => s.display_id === cfg.shareDisplayId);
          if (!chosen) chosen = sources.find((s) => s.display_id === primaryId) || sources[0];

          // Provide Windows system-loopback audio when the host has it enabled.
          // 'loopback' is an Electron extension to the media-request callback.
          callback(cfg.shareAudio ? { video: chosen, audio: 'loopback' } : { video: chosen });
        } catch (err) {
          console.error('[display-media] handler failed:', err);
          callback({});
        }
      })();
    },
    { useSystemPicker: false }
  );
}

// --- §4.3 window & security -------------------------------------------------
function createWindow() {
  win = new BrowserWindow({
    width: 1050,
    height: 720,
    minWidth: 800,
    minHeight: 560,
    backgroundColor: '#14161a',
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload.js'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false
    }
  });

  win.webContents.on('will-navigate', (event) => event.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));

  win.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
}

// --- §4.4 IPC surface -------------------------------------------------------

// Every handler drops calls that did not come from our own main frame.
function fromMainFrame(event) {
  return !!win && !win.isDestroyed() && event.senderFrame === win.webContents.mainFrame;
}

function publicConfig() {
  const c = config.load();
  return {
    uuid: c.uuid,
    serverUrl: c.serverUrl,
    mode: c.mode,
    passwordPermission: c.passwordPermission,
    hasPassword: !!c.passwordHash, // never expose the hash itself
    iceServers: c.iceServers,
    shareAudio: c.shareAudio,
    shareDisplayId: c.shareDisplayId
  };
}

function registerIpc() {
  ipcMain.handle('config:get', (event) => {
    if (!fromMainFrame(event)) return;
    return publicConfig();
  });

  ipcMain.handle('config:set', (event, patch) => {
    if (!fromMainFrame(event)) return;
    if (!patch || typeof patch !== 'object') throw new Error('invalid payload');

    const next = {};

    if (patch.serverUrl !== undefined) {
      const u = String(patch.serverUrl).trim();
      if (!/^wss?:\/\//.test(u)) throw new Error('serverUrl must start with ws:// or wss://');
      next.serverUrl = u;
    }

    if (patch.mode !== undefined) {
      if (patch.mode !== 'approve' && patch.mode !== 'password') throw new Error('invalid mode');
      next.mode = patch.mode;
    }

    if (patch.passwordPermission !== undefined) {
      if (patch.passwordPermission !== 'view' && patch.passwordPermission !== 'control') {
        throw new Error('invalid passwordPermission');
      }
      next.passwordPermission = patch.passwordPermission;
    }

    if (patch.password !== undefined) {
      const plain = String(patch.password);
      // Empty string clears the password (§3).
      next.passwordHash = plain === '' ? null : config.hash(plain);
    }

    if (patch.iceServers !== undefined) {
      if (!Array.isArray(patch.iceServers)) throw new Error('iceServers must be an array');
      // A malformed entry would make `new RTCPeerConnection` throw mid-handshake;
      // reject it here instead.
      for (const s of patch.iceServers) {
        const ok =
          s &&
          typeof s === 'object' &&
          (typeof s.urls === 'string' ||
            (Array.isArray(s.urls) && s.urls.every((u) => typeof u === 'string')));
        if (!ok) throw new Error('each iceServers entry needs a urls string or string array');
      }
      next.iceServers = patch.iceServers;
    }

    if (patch.shareAudio !== undefined) {
      if (typeof patch.shareAudio !== 'boolean') throw new Error('shareAudio must be a boolean');
      next.shareAudio = patch.shareAudio;
    }

    if (patch.shareDisplayId !== undefined) {
      // null / '' => primary; otherwise a display id string.
      if (patch.shareDisplayId === null || patch.shareDisplayId === '') {
        next.shareDisplayId = null;
      } else if (typeof patch.shareDisplayId === 'string') {
        next.shareDisplayId = patch.shareDisplayId;
      } else {
        throw new Error('shareDisplayId must be a string or null');
      }
    }

    config.save(next);
    return publicConfig();
  });

  ipcMain.handle('password:verify', async (event, plain) => {
    if (!fromMainFrame(event)) return false;
    const ok = config.verifyPassword(plain);
    // Contract §6: 2 s artificial delay before answering a wrong password.
    if (!ok) await new Promise((r) => setTimeout(r, 2000));
    return ok;
  });

  // Challenge-response verify (§3.2). Verification stays in the privileged main
  // process because the renderer never holds passwordHash.
  ipcMain.handle('password:verifyProof', async (event, nonce, proof) => {
    if (!fromMainFrame(event)) return false;
    const ok = config.verifyProof(nonce, proof);
    if (!ok) await new Promise((r) => setTimeout(r, 2000)); // brute-force damper
    return ok;
  });

  ipcMain.handle('screen:size', (event) => {
    if (!fromMainFrame(event)) return;
    const d = screen.getPrimaryDisplay();
    return {
      w: Math.round(d.size.width * d.scaleFactor),
      h: Math.round(d.size.height * d.scaleFactor)
    };
  });

  // Monitors available to share, for the Settings picker.
  ipcMain.handle('screen:list', (event) => {
    if (!fromMainFrame(event)) return [];
    const primaryId = String(screen.getPrimaryDisplay().id);
    return screen.getAllDisplays().map((d, i) => ({
      id: String(d.id),
      primary: String(d.id) === primaryId,
      label:
        `Monitor ${i + 1} — ${Math.round(d.size.width * d.scaleFactor)}×${Math.round(
          d.size.height * d.scaleFactor
        )}` + (String(d.id) === primaryId ? ' (primary)' : '')
    }));
  });

  // Point the injector at the display being shared. Called at session start
  // (with the shared display id) and at teardown (null -> back to primary).
  ipcMain.handle('input:setDisplay', (event, displayId) => {
    if (!fromMainFrame(event)) return;

    const primaryId = String(screen.getPrimaryDisplay().id);
    // No id, or the primary, uses the original verified ABSOLUTE path.
    if (!displayId || String(displayId) === primaryId) {
      if (input) input.setTargetRect(null);
      return;
    }

    const d = screen.getAllDisplays().find((x) => String(x.id) === String(displayId));
    if (!d) {
      if (input) input.setTargetRect(null);
      return;
    }

    // Convert the display's DIP bounds to physical screen coordinates — the same
    // space Win32's virtual desktop uses — so the VIRTUALDESK math is correct
    // even under mixed-DPI. dipToScreenRect handles the per-monitor scaling.
    let phys;
    try {
      phys = screen.dipToScreenRect(null, d.bounds);
    } catch (err) {
      console.warn('[input] dipToScreenRect failed; using primary path', err);
      if (input) input.setTargetRect(null);
      return;
    }
    getInput().setTargetRect({ x: phys.x, y: phys.y, w: phys.width, h: phys.height });
  });

  ipcMain.handle('input:inject', (event, msg) => {
    if (!fromMainFrame(event)) return;
    injectValidated(msg);
  });

  ipcMain.handle('input:releaseAll', (event) => {
    if (!fromMainFrame(event)) return;
    // Only if the injector was ever loaded — never pull in koffi just to
    // release nothing.
    if (input) input.releaseAll();
  });
}

// §4.4 input:inject — strict validation BEFORE input.js is touched.
// input.js is required lazily so M1-M4 run even without koffi installed.
let input = null;
function getInput() {
  if (input === null) input = require('./input');
  return input;
}

const finite = (v) => typeof v === 'number' && Number.isFinite(v);
const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

function injectValidated(msg) {
  if (!msg || typeof msg !== 'object') return;

  const t = msg.t;
  if (!['mm', 'md', 'mu', 'wh', 'kd', 'ku'].includes(t)) return;

  if (t === 'mm' || t === 'md' || t === 'mu') {
    if (!finite(msg.x) || !finite(msg.y)) return;
    const x = clamp(msg.x, 0, 1);
    const y = clamp(msg.y, 0, 1);

    if (t === 'mm') return getInput().mouseMove(x, y);

    if (msg.b !== 0 && msg.b !== 1 && msg.b !== 2) return;
    return getInput().mouseButton(msg.b, t === 'md', x, y);
  }

  if (t === 'wh') {
    if (!finite(msg.dx) || !finite(msg.dy)) return;
    return getInput().wheel(clamp(msg.dx, -1200, 1200), clamp(msg.dy, -1200, 1200));
  }

  // kd / ku
  if (typeof msg.code !== 'string' || msg.code.length > 32) return;
  return getInput().key(msg.code, t === 'kd');
}

// --- boot -------------------------------------------------------------------
app.whenReady().then(() => {
  installDisplayMediaHandler();
  createWindow();
  registerIpc();
});

app.on('window-all-closed', () => {
  app.quit();
});
