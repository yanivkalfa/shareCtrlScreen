'use strict';

// Shared namespace for the renderer scripts (no bundler, plain globals).
window.App = window.App || {};

App.UI = (function () {
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
  const listeners = [];

  function getState() {
    return state;
  }

  function onStateChange(fn) {
    listeners.push(fn);
  }

  function setState(next) {
    if (!STATES.includes(next)) throw new Error('unknown state ' + next);
    const prev = state;
    state = next;
    render();
    listeners.forEach((fn) => {
      try {
        fn(next, prev);
      } catch (err) {
        console.error(err);
      }
    });
  }

  function render() {
    const home = state === 'READY' || state === 'OFFLINE' || state === 'REQUESTING' ||
      state === 'PASSWORD_PROMPT' || state === 'INCOMING';

    $('screen-home').classList.toggle('hidden', !home);
    $('screen-host').classList.toggle('hidden', state !== 'HOST_ACTIVE');
    $('screen-view').classList.toggle('hidden', state !== 'VIEW_ACTIVE');

    $('modal-requesting').classList.toggle('hidden', state !== 'REQUESTING');
    $('modal-password').classList.toggle('hidden', state !== 'PASSWORD_PROMPT');
    $('modal-incoming').classList.toggle('hidden', state !== 'INCOMING');

    // Connect is only possible from a fully idle, registered app.
    $('btn-connect').disabled = state !== 'READY' || !validUuid($('remote-id').value.trim());
  }

  // ---- connection status dot -------------------------------------------
  // 'red' offline, 'green' registered, 'yellow' socket down but session alive.
  function setStatus(color, text) {
    const dot = $('status-dot');
    dot.className = 'dot dot-' + color;
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

  // ---- toasts ------------------------------------------------------------
  function toast(msg, kind) {
    const el = document.createElement('div');
    el.className = 'toast' + (kind === 'error' ? ' toast-error' : '');
    el.textContent = msg;
    $('toasts').appendChild(el);
    setTimeout(() => el.remove(), 4000);
  }

  // ---- countdown helper (shared by REQUESTING and INCOMING) --------------
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

  const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  function validUuid(s) {
    return typeof s === 'string' && UUID_RE.test(s);
  }

  // ---- config ------------------------------------------------------------
  function getConfig() {
    return config;
  }

  async function reloadConfig() {
    config = await window.native.configGet();
    $('my-uuid').textContent = config.uuid;
    renderRecents();
    return config;
  }

  // A remote id shown compactly on a chip (UUIDs are long).
  function shortId(id) {
    return id.length > 14 ? id.slice(0, 8) + '…' : id;
  }

  // Populate the autocomplete <datalist> and the clickable "Recent:" chips row
  // from config.recentIds.
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
        input.dispatchEvent(new Event('input')); // re-validate + enable Connect
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
        config.recentIds = await window.native.recentsClear();
      } catch (_) {
        config.recentIds = [];
      }
      renderRecents();
    });
    row.appendChild(clear);
  }

  // ---- settings modal ----------------------------------------------------
  async function openSettings() {
    $('set-server').value = config.serverUrl;
    $('set-mode-approve').checked = config.mode === 'approve';
    $('set-mode-password').checked = config.mode === 'password';
    $('set-password').value = '';
    $('set-password').placeholder = config.hasPassword ? '(unchanged)' : '(none set)';
    $('set-password-clear').checked = false;
    $('set-password-perm').value = config.passwordPermission;
    $('set-share-audio').checked = config.shareAudio !== false;
    $('set-capture-shortcuts').checked = config.captureShortcuts === true;
    await populateDisplays();
    $('modal-settings').classList.remove('hidden');
  }

  // Rebuild the monitor dropdown from the live display list ('' = primary).
  async function populateDisplays() {
    const sel = $('set-share-display');
    let displays = [];
    try {
      displays = await window.native.screenList();
    } catch (_) {
      displays = [];
    }
    sel.innerHTML = '<option value="">Primary monitor</option>';
    for (const d of displays) {
      if (d.primary) continue; // the primary is the '' option already
      const opt = document.createElement('option');
      opt.value = d.id;
      opt.textContent = d.label;
      sel.appendChild(opt);
    }
    // Select the saved choice if it is still present; else fall back to primary.
    const saved = config.shareDisplayId || '';
    sel.value = saved;
    if (sel.value !== saved) sel.value = '';
  }

  function closeSettings() {
    $('modal-settings').classList.add('hidden');
  }

  async function saveSettings() {
    const patch = {
      serverUrl: $('set-server').value.trim(),
      mode: $('set-mode-password').checked ? 'password' : 'approve',
      passwordPermission: $('set-password-perm').value,
      shareAudio: $('set-share-audio').checked,
      captureShortcuts: $('set-capture-shortcuts').checked,
      shareDisplayId: $('set-share-display').value || null
    };

    // Only touch the password when the user actually asked to.
    if ($('set-password-clear').checked) {
      patch.password = '';
    } else if ($('set-password').value !== '') {
      patch.password = $('set-password').value;
    }

    const prevUrl = config.serverUrl;

    try {
      config = await window.native.configSet(patch);
    } catch (err) {
      toast(String(err.message || err).replace(/^Error: /, ''), 'error');
      return;
    }

    closeSettings();
    toast('Settings saved');

    // §6: a serverUrl change triggers a signaling reconnect.
    if (config.serverUrl !== prevUrl && App.Signaling) App.Signaling.reconnectNow();

    // Apply a captureShortcuts toggle immediately if a session is live.
    if (App.Viewer) App.Viewer.updateShortcutCapture();
  }

  function wire() {
    $('btn-settings').addEventListener('click', openSettings);
    $('btn-settings-cancel').addEventListener('click', closeSettings);
    $('btn-settings-save').addEventListener('click', saveSettings);

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
  }

  return {
    $,
    wire,
    getState,
    setState,
    onStateChange,
    setStatus,
    setBanner,
    toast,
    countdown,
    validUuid,
    getConfig,
    reloadConfig,
    renderRecents
  };
})();
