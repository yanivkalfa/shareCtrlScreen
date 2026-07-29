'use strict';

// UI controller for the Tauri shell (Plan 04 §7). Mirrors the original
// app/renderer/ui.js state machine and markup 1:1 so the look and behaviour are
// unchanged; only the plumbing differs:
//   ipcRenderer.invoke(...) -> window.__TAURI__.core.invoke(...)
//   ipcRenderer.on(...)     -> window.__TAURI__.event.listen(...)
// WebRTC/signaling/capture all live in the Rust engine now.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// §6 — one state variable; every state maps to exactly one screen/modal.
const STATES = [
  'OFFLINE',
  'READY',
  'REQUESTING',
  'PASSWORD_PROMPT',
  'INCOMING',
  'HOST_ACTIVE',
  'VIEW_ACTIVE'
];

let state = 'OFFLINE';
let config = null;
let incomingFrom = null;   // peer awaiting our approve/deny
let passwordFrom = null;   // host that demanded a password
let cancelCountdown = null;

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const validUuid = (s) => typeof s === 'string' && UUID_RE.test(s);

function setState(next) {
  if (!STATES.includes(next)) throw new Error('unknown state ' + next);
  state = next;
  render();
}

function render() {
  const home =
    state === 'READY' || state === 'OFFLINE' || state === 'REQUESTING' ||
    state === 'PASSWORD_PROMPT' || state === 'INCOMING';

  $('screen-home').classList.toggle('hidden', !home);
  $('screen-host').classList.toggle('hidden', state !== 'HOST_ACTIVE');
  $('screen-view').classList.toggle('hidden', state !== 'VIEW_ACTIVE');

  $('modal-requesting').classList.toggle('hidden', state !== 'REQUESTING');
  $('modal-password').classList.toggle('hidden', state !== 'PASSWORD_PROMPT');
  $('modal-incoming').classList.toggle('hidden', state !== 'INCOMING');

  // The "connection lost" panel only belongs to a live session — never leave it
  // up on the home screen. (Only ever hides; showing it is the engine's call.)
  if (state !== 'VIEW_ACTIVE' && state !== 'HOST_ACTIVE') {
    $('modal-link').classList.add('hidden');
  }

  // Connect is only possible from a fully idle, registered app.
  $('btn-connect').disabled = state !== 'READY' || !validUuid($('remote-id').value.trim());
}

// ---- status / banner / toasts ----------------------------------------------
function setStatus(color, text) {
  $('status-dot').className = 'dot dot-' + color;
  $('status-text').textContent = text;
}

function setBanner(text) {
  const el = $('banner');
  if (!text) {
    el.classList.add('hidden');
    el.textContent = '';
  } else {
    el.textContent = text;
    el.classList.remove('hidden');
  }
}

function toast(msg, kind) {
  const el = document.createElement('div');
  el.className = 'toast' + (kind === 'error' ? ' toast-error' : '');
  el.textContent = msg;
  $('toasts').appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

// ---- countdown helper (shared by REQUESTING and INCOMING) ------------------
function countdown(elId, seconds, onDone) {
  let left = seconds;
  $(elId).textContent = String(left);
  const iv = setInterval(() => {
    left -= 1;
    $(elId).textContent = String(Math.max(0, left));
    if (left <= 0) {
      clearInterval(iv);
      if (onDone) onDone();
    }
  }, 1000);
  return () => clearInterval(iv);
}

function stopCountdown() {
  if (cancelCountdown) cancelCountdown();
  cancelCountdown = null;
}

// ---- config / recents ------------------------------------------------------
async function reloadConfig() {
  config = await invoke('get_config');
  $('my-uuid').textContent = config.uuid;
  renderRecents();
  return config;
}

const shortId = (id) => (id.length > 14 ? id.slice(0, 8) + '…' : id);

function renderRecents() {
  const ids = (config && config.recentIds) || [];

  const dl = $('recent-ids');
  dl.innerHTML = '';
  for (const id of ids) {
    const o = document.createElement('option');
    o.value = id;
    dl.appendChild(o);
  }

  const row = $('recents-row');
  row.innerHTML = '';
  if (!ids.length) return;

  const label = document.createElement('span');
  label.className = 'recents-label';
  label.textContent = 'Recent:';
  row.appendChild(label);

  for (const id of ids) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'recent-chip';
    chip.textContent = shortId(id);
    chip.title = id;
    chip.addEventListener('click', () => {
      const input = $('remote-id');
      input.value = id;
      input.dispatchEvent(new Event('input'));
      input.focus();
    });
    row.appendChild(chip);
  }

  const clear = document.createElement('button');
  clear.type = 'button';
  clear.className = 'recent-clear';
  clear.textContent = 'Clear';
  clear.addEventListener('click', async () => {
    try {
      config.recentIds = await invoke('clear_recents');
    } catch (_) {
      config.recentIds = [];
    }
    renderRecents();
  });
  row.appendChild(clear);
}

// ---- settings modal --------------------------------------------------------
function openSettings() {
  $('set-server').value = config.serverUrl;
  $('set-mode-approve').checked = config.mode === 'approve';
  $('set-mode-password').checked = config.mode === 'password';
  $('set-password').value = '';
  $('set-password').placeholder = config.hasPassword ? '(unchanged)' : '(none set)';
  $('set-password-clear').checked = false;
  $('set-password-perm').value = config.passwordPermission;
  $('set-share-audio').checked = config.shareAudio !== false;
  $('set-share-display').value = config.shareDisplayId || '';
  $('set-auto-login').checked = !!config.autoLogin;
  $('set-clipboard-sync').checked = config.clipboardSync !== false;
  $('set-paste-dropped').checked = config.pasteDroppedFiles !== false;
  $('set-start-with-windows').checked = !!config.startWithWindows;
  const saved = config.savedLoginCount || 0;
  const clear = $('set-clear-logins');
  clear.checked = false;
  clear.parentElement.lastChild.textContent =
    saved > 0
      ? ` Forget ${saved} saved password${saved === 1 ? '' : 's'}`
      : ' Forget all saved passwords';
  clear.disabled = saved === 0;
  $('modal-settings').classList.remove('hidden');
}

function closeSettings() {
  $('modal-settings').classList.add('hidden');
  // If settings were opened mid-session, the native video surface was hidden so
  // the modal could be seen (it sits above the WebView) — bring it back.
  if (state === 'VIEW_ACTIVE') invoke('set_video_visible', { visible: true });
}

async function saveSettings() {
  const patch = {
    serverUrl: $('set-server').value.trim(),
    mode: $('set-mode-password').checked ? 'password' : 'approve',
    passwordPermission: $('set-password-perm').value,
    shareAudio: $('set-share-audio').checked,
    shareDisplayId: $('set-share-display').value || null,
    autoLogin: $('set-auto-login').checked,
    clipboardSync: $('set-clipboard-sync').checked,
    pasteDroppedFiles: $('set-paste-dropped').checked,
    startWithWindows: $('set-start-with-windows').checked,
    clearSavedLogins: $('set-clear-logins').checked
  };

  // Only touch the password when the user actually asked to.
  if ($('set-password-clear').checked) {
    patch.password = '';
  } else if ($('set-password').value !== '') {
    patch.password = $('set-password').value;
  }

  try {
    await invoke('save_settings', { patch });
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
    return;
  }

  closeSettings();
  toast('Settings saved');
  await reloadConfig();
}

// ---- wiring ----------------------------------------------------------------
$('btn-settings').addEventListener('click', openSettings);
$('btn-settings-cancel').addEventListener('click', closeSettings);
$('btn-settings-save').addEventListener('click', saveSettings);

// Session menu bar (revealed by hovering the top edge of the video).
$('btn-view-refresh').addEventListener('click', () => {
  invoke('request_refresh');
  toast('Requested a fresh frame');
});
$('btn-view-settings').addEventListener('click', async () => {
  // Take the native video down BEFORE painting the modal — it sits above the web
  // UI, so opening the dialog first means it appears underneath the video (and,
  // if the hover bar was open, sliced to that strip).
  try {
    await invoke('set_video_visible', { visible: false });
  } catch (_) {
    /* fall through — the dialog is still better than nothing */
  }
  openSettings();
});

$('btn-copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(config.uuid);
    toast('ID copied');
  } catch (_) {
    toast('Could not copy', 'error');
  }
});

$('remote-id').addEventListener('input', () => {
  const v = $('remote-id').value.trim();
  $('connect-hint').textContent = v && !validUuid(v) ? 'That does not look like a valid ID.' : '';
  render();
});

$('btn-connect').addEventListener('click', () => {
  const id = $('remote-id').value.trim();
  if (!validUuid(id)) return;
  $('requesting-peer').textContent = id;
  setState('REQUESTING');
  stopCountdown();
  // Viewer owns the 30 s timeout (contract §3.3).
  cancelCountdown = countdown('requesting-count', 30, () => {
    toast('No answer — timed out', 'error');
    setState('READY');
  });
  invoke('connect_to', { id });
});

$('btn-cancel-request').addEventListener('click', () => {
  stopCountdown();
  invoke('end_session');
  setState('READY');
});

// password prompt (viewer)
function cancelPassword() {
  passwordFrom = null;
  $('password-input').value = '';
  setState('READY');
}
function submitPassword() {
  const pw = $('password-input').value;
  if (!passwordFrom) return;
  invoke('submit_password', {
    host: passwordFrom,
    password: pw,
    remember: $('password-remember').checked
  });
  $('password-input').value = '';
  setState('REQUESTING');
  stopCountdown();
  cancelCountdown = countdown('requesting-count', 30, () => {
    toast('No answer — timed out', 'error');
    setState('READY');
  });
}
$('btn-password-cancel').addEventListener('click', cancelPassword);
$('btn-password-ok').addEventListener('click', submitPassword);
// Enter submits, Escape cancels — no reaching for the mouse.
$('password-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    submitPassword();
  } else if (e.key === 'Escape') {
    e.preventDefault();
    cancelPassword();
  }
});

// incoming approval (host)
function answerIncoming(decision) {
  stopCountdown();
  if (incomingFrom) invoke('approve', { from: incomingFrom, decision });
  incomingFrom = null;
  if (decision === 'deny') setState('READY');
}
$('btn-incoming-deny').addEventListener('click', () => answerIncoming('deny'));
$('btn-incoming-view').addEventListener('click', () => answerIncoming('view'));
$('btn-incoming-control').addEventListener('click', () => answerIncoming('control'));

// host session controls
$('host-perm').addEventListener('change', () => {
  invoke('set_permission', { value: $('host-perm').value });
});
$('btn-host-end').addEventListener('click', () => invoke('end_session'));
$('btn-view-end').addEventListener('click', () => invoke('end_session'));
// Don't wait out the grace period — give up now.
$('btn-link-end').addEventListener('click', () => {
  $('modal-link').classList.add('hidden');
  invoke('end_session');
});

// ---- engine events ---------------------------------------------------------
listen('server-status', (e) => {
  const s = e.payload.status;
  if (s === 'connected') {
    setStatus('green', 'server: connected');
    setBanner('');
    if (state === 'OFFLINE') setState('READY');
  } else if (s === 'reconnecting') {
    // A live session keeps running on its own WebRTC leg — yellow, not offline.
    if (state === 'HOST_ACTIVE' || state === 'VIEW_ACTIVE') {
      setStatus('yellow', 'server: reconnecting (session active)');
    } else {
      setStatus('red', 'server: disconnected (retrying)');
      if (state !== 'OFFLINE') setState('OFFLINE');
    }
  }
});

listen('approval-request', (e) => {
  incomingFrom = e.payload.from;
  $('incoming-peer').textContent = incomingFrom;
  setState('INCOMING');
  stopCountdown();
  // The Rust decider auto-denies at 30 s too; this just mirrors the countdown.
  cancelCountdown = countdown('incoming-count', 30, () => {
    incomingFrom = null;
    setState('READY');
  });
});

listen('password-required', (e) => {
  passwordFrom = e.payload.from;
  stopCountdown();
  $('password-input').value = '';
  // Default the checkbox to whatever auto-login is currently set to, so ticking
  // it once in Settings makes every later prompt remember by default.
  $('password-remember').checked = !!(config && config.autoLogin);
  setState('PASSWORD_PROMPT');
  $('password-input').focus();
});

listen('role-changed', (e) => {
  const p = e.payload;
  stopCountdown();
  // Whatever happens next (reconnected, or dropped back to the home screen),
  // the "connection lost" panel is stale.
  $('modal-link').classList.add('hidden');
  if (p.role === 'host') {
    $('host-peer').textContent = p.peer;
    $('host-perm').value = p.permission;
    setState('HOST_ACTIVE');
  } else if (p.role === 'viewer') {
    $('view-peer').textContent = p.peer;
    const control = p.permission === 'control';
    const badge = $('view-badge');
    badge.textContent = control ? 'CONTROL' : 'VIEW ONLY';
    badge.className = 'badge ' + (control ? 'badge-control' : 'badge-view');
    setState('VIEW_ACTIVE');
  } else {
    setState('READY');
  }
  reloadConfig();
});

listen('toast', (e) => toast(e.payload.message || '', 'error'));

// The peer went quiet. The engine has already hidden the native video (a frozen
// frame is indistinguishable from a live one), so this panel is what the user
// sees while the grace period runs.
listen('link-trouble', (e) => {
  if (state !== 'VIEW_ACTIVE' && state !== 'HOST_ACTIVE') return;
  $('link-count').textContent = String(e.payload.secsLeft ?? 0);
  $('modal-link').classList.remove('hidden');
});

listen('link-restored', () => {
  $('modal-link').classList.add('hidden');
  toast('Connection restored');
});

// ---- boot ------------------------------------------------------------------
(async () => {
  await reloadConfig();
  render();
  // Back-fill the server status: the engine registers ~0.5 s after launch and
  // emits a one-shot 'server-status' event that can fire before this WebView
  // subscribed above. Without this pull the dot would stay stuck on OFFLINE and
  // the Connect button (gated on state === 'READY') would never enable.
  try {
    const s = await invoke('get_server_status');
    if (s === 'connected') {
      setStatus('green', 'server: connected');
      setBanner('');
      if (state === 'OFFLINE') setState('READY');
    }
  } catch (_) {
    /* command unavailable (older engine) — the event path still covers it */
  }
})();
