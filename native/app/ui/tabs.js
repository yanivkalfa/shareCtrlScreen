// The tab strip and the tab on screen (plans/08 P4, 08a §2.4, §4, §5.1).
//
// The app's TabManager (src/tabs.rs) is the only authority: this module draws
// the `tabs-changed` snapshot it sends after every change and calls the
// `tab_*` commands. It keeps no tab state of its own beyond what is being
// typed (a rename, a password) and what is being dragged.
//
// The tab on screen shows, below the session bar, either the native video (a
// live tab: the app shows its surface under this page) or a card: connecting,
// its password prompt, why it is disconnected, or — live, but the host says
// its screen cannot be shown (its lock screen, plans/10 U1) — why there is no
// picture.

import * as api from './api.js';
import { peerLabel, reloadConfig } from './connect.js';
import { renderDisplays } from './session.js';
import {
  $,
  S,
  activeTab,
  currentRemote,
  errorText,
  overlayClose,
  overlayOpen,
  render,
  toast,
  windowRemote
} from './state.js';

const shortId = (id) => (id.length > 14 ? id.slice(0, 8) + '…' : id);
const tabLabel = (t) => t.name || shortId(t.remoteId);

// The remote being renamed in place, its text so far (the strip is redrawn on
// every snapshot, which must not lose it), and the one being dragged.
let renaming = null;
let renameDraft = '';
let redrawing = false;
let renameStarting = false;
let dragging = null;
// The password field belongs to this tab: switching tabs clears it.
let passwordFor = null;

// ---- the >8 confirm (D2) ---------------------------------------------------------
// A command that would pass 8 connected tabs answers `{ needsConfirm: n }` and
// does nothing; asked, the user repeats it confirmed.
let confirmAction = null;

async function askToContinue(wouldBe, action) {
  $('tab-confirm-text').textContent =
    `You're about to have ${wouldBe} connected tabs. Each one uses its own connection ` +
    'and video decoder, and this computer may slow down. Continue?';
  confirmAction = action;
  // Shown once the video is out of the way; focus only then (a hidden
  // button takes no focus, and Escape would not reach the dialog).
  await overlayOpen('modal-tab-confirm');
  $('btn-tab-confirm-cancel').focus();
}

function closeConfirm() {
  confirmAction = null;
  overlayClose('modal-tab-confirm');
}

/** Run a connecting command; ask first when it says so, then repeat it. */
export async function startConnecting(command) {
  try {
    const r = await command(false);
    if (r && r.needsConfirm) {
      askToContinue(r.needsConfirm, () => command(true));
    }
  } catch (err) {
    toast(errorText(err), 'error');
  }
}

// ---- the strip -----------------------------------------------------------------------
function dotClass(t) {
  if (t.troubleSecs != null) return 'dot dot-red';
  switch (t.phase) {
    case 'live':
      return 'dot dot-green';
    case 'connecting':
    case 'password':
      return 'dot dot-yellow';
    default:
      return 'dot dot-grey';
  }
}

function renderStrip() {
  const list = $('tab-list');
  // Removing a focused rename field may fire its blur: that is a redraw,
  // not the user leaving the field.
  redrawing = true;
  list.textContent = '';
  redrawing = false;
  for (const t of S.tabs.tabs) {
    const el = document.createElement('div');
    el.className = 'tab' + (t.remoteId === S.tabs.active ? ' active' : '');
    el.setAttribute('role', 'tab');
    el.setAttribute('aria-selected', String(t.remoteId === S.tabs.active));
    el.title = t.closedElsewhere
      ? `${peerLabel(t.remoteId)} — closed on another computer; it goes when disconnected`
      : peerLabel(t.remoteId);
    el.draggable = renaming !== t.remoteId && !t.closedElsewhere;
    el.dataset.remote = t.remoteId;

    const dot = document.createElement('span');
    dot.className = dotClass(t);
    el.appendChild(dot);

    if (renaming === t.remoteId) {
      const input = document.createElement('input');
      input.className = 'tab-name-edit';
      input.value = renameDraft;
      input.maxLength = 40;
      input.addEventListener('input', () => {
        renameDraft = input.value;
      });
      const commit = () => {
        if (redrawing || renaming !== t.remoteId) return;
        renaming = null;
        api.tabRename(t.remoteId, input.value).catch((e) => toast(errorText(e), 'error'));
        renderStrip();
      };
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') commit();
        if (e.key === 'Escape') {
          renaming = null;
          renderStrip();
        }
      });
      input.addEventListener('blur', commit);
      el.appendChild(input);
      setTimeout(() => {
        input.focus();
        // Selected when the rename starts; after a redraw the caret goes
        // back to the end of what was typed.
        if (renameStarting) input.select();
        else input.setSelectionRange(input.value.length, input.value.length);
        renameStarting = false;
      }, 0);
    } else {
      const name = document.createElement('span');
      name.className = 'tab-name';
      name.textContent = tabLabel(t);
      name.addEventListener('dblclick', (e) => {
        e.stopPropagation();
        renaming = t.remoteId;
        renameDraft = t.name;
        renameStarting = true;
        renderStrip();
      });
      el.appendChild(name);
    }

    // Its own window (plans/09); not for a tab closed elsewhere.
    if (!t.closedElsewhere) {
      const detach = document.createElement('button');
      detach.className = 'tab-close tab-detach';
      detach.title = 'Open in its own window';
      detach.textContent = '⧉';
      detach.addEventListener('click', (e) => {
        e.stopPropagation();
        api.tabDetach(t.remoteId).catch((err) => toast(errorText(err), 'error'));
      });
      el.appendChild(detach);
    }

    const close = document.createElement('button');
    close.className = 'tab-close';
    close.title = 'Close tab';
    close.textContent = '×';
    close.addEventListener('click', (e) => {
      e.stopPropagation();
      api.tabClose(t.remoteId);
    });
    el.appendChild(close);

    // Clicking the tab on screen again gives its video the keyboard back.
    el.addEventListener('click', () => api.tabActivate(t.remoteId));
    // Middle click closes, as in browsers.
    el.addEventListener('auxclick', (e) => {
      if (e.button === 1) api.tabClose(t.remoteId);
    });

    el.addEventListener('dragstart', (e) => {
      dragging = t.remoteId;
      e.dataTransfer.effectAllowed = 'move';
    });
    el.addEventListener('dragend', () => {
      dragging = null;
      for (const x of list.children) x.classList.remove('drop-before');
    });
    el.addEventListener('dragover', (e) => {
      if (!dragging || dragging === t.remoteId) return;
      e.preventDefault();
      el.classList.add('drop-before');
    });
    el.addEventListener('dragleave', () => el.classList.remove('drop-before'));
    el.addEventListener('drop', (e) => {
      e.preventDefault();
      el.classList.remove('drop-before');
      if (dragging && dragging !== t.remoteId) {
        api.tabMove(dragging, t.remoteId).catch((err) => toast(errorText(err), 'error'));
      }
      dragging = null;
    });

    list.appendChild(el);
  }
  const someDisconnected = S.tabs.tabs.some((t) => t.phase === 'disconnected');
  $('btn-tab-connect-all').classList.toggle('hidden', !someDisconnected || S.tabs.signedOut);
}

// ---- the tab on screen ----------------------------------------------------------------
function setCard(title, text, primary, secondary) {
  $('tab-card-title').textContent = title;
  $('tab-card-text').textContent = text || '';
  $('tab-card-text').classList.toggle('hidden', !text);
  const p = $('btn-tab-primary');
  const s = $('btn-tab-secondary');
  p.textContent = primary ? primary.label : '';
  p.classList.toggle('hidden', !primary);
  p.onclick = primary ? primary.run : null;
  s.textContent = secondary ? secondary.label : '';
  s.classList.toggle('hidden', !secondary);
  s.onclick = secondary ? secondary.run : null;
}

// ---- the host's screen (plans/10 U1, 10a §3.9) ---------------------------------------
// What the host says its screen is: `{ screen, kind, why }`, or null (an older
// host never says: its screen is the ordinary one). The words follow the
// host's `why` / `kind`; a value this version does not know gets the generic
// words, so a later host can add one.
const BLOCKED_WORDS = {
  // Before the secure-desktop helper exists (plans/10 U3) a Windows host
  // cannot tell its lock screen from a UAC prompt, so the text names both.
  unsupported: [
    'The remote computer is locked',
    'Or it is showing a security prompt, such as User Account Control. ShareCtrlScreen ' +
      'cannot show or control that screen yet. Someone at that computer has to unlock it ' +
      'or answer the prompt; the picture comes back by itself.'
  ],
  disabled: [
    'The remote computer is locked',
    'Control of its lock screen is turned off in ShareCtrlScreen on that computer. ' +
      'Someone there has to unlock it; the picture comes back by itself.'
  ],
  policy: [
    'The remote computer is showing a security prompt',
    "That computer's settings do not allow its security prompts to be shown or answered " +
      'remotely. Someone there has to answer it; the picture comes back by itself.'
  ],
  agent: [
    "The remote computer's lock screen cannot be shown",
    'The part of ShareCtrlScreen that shows it is not answering on that computer. It is ' +
      'restarted automatically; meanwhile someone there can unlock it, and the picture ' +
      'comes back by itself.'
  ],
  'view-only': [
    'The remote computer is showing a secure screen',
    'Lock screens and security prompts are not shown to a view-only connection. The ' +
      'picture comes back once that screen is gone.'
  ]
};
const BLOCKED_GENERIC = [
  "The remote computer's screen cannot be shown right now",
  'It is showing a screen that cannot be shared, such as a lock screen. The picture ' +
    'comes back by itself.'
];
const SECURE_BADGES = { lock: 'LOCK SCREEN', prompt: 'WINDOWS PROMPT', signin: 'SIGN-IN' };
// The host's strings index these tables: only their own entries count (a
// `why` of "constructor" must get the generic words, not Object's).
const own = (table, key) =>
  typeof key === 'string' && Object.prototype.hasOwnProperty.call(table, key) ? table[key] : null;

/** Whether the host says its screen cannot be shown. */
const hostBlocked = (t) => !!(t.hostScreen && t.hostScreen.screen === 'blocked');

function renderHostScreen(t, live) {
  const hs = live ? t.hostScreen : null;
  const badge = $('view-screen-badge');
  const secure = !!(hs && hs.screen === 'secure');
  badge.classList.toggle('hidden', !secure);
  badge.textContent = secure ? own(SECURE_BADGES, hs.kind) || 'SECURE SCREEN' : '';
  if (live && hostBlocked(t)) {
    const words = own(BLOCKED_WORDS, t.hostScreen.why) || BLOCKED_GENERIC;
    // The saved-password line belongs to the connecting card, not this one.
    $('tab-card-auto').classList.add('hidden');
    setCard(words[0], words[1], null, {
      label: 'Disconnect',
      run: () => api.tabDisconnect(t.remoteId)
    });
  }
}

function renderActive() {
  const t = activeTab();
  if (!t) {
    passwordFor = null;
    renderDisplays(null);
    return;
  }
  const live = t.phase === 'live';
  // Live without a picture: the host's screen cannot be shown. The engine
  // hides the video; the card says why, under the session bar.
  const picture = live && !hostBlocked(t);
  $('tab-card').classList.toggle('hidden', picture);
  $('video-wrap').classList.toggle('hidden', !picture);
  // The session bar belongs to a live session; a card has its own buttons.
  $('view-bar').classList.toggle('hidden', !live);

  // The session bar (revealed by the top edge of the video).
  $('view-peer').textContent = peerLabel(t.remoteId);
  const control = t.permission === 'control';
  const badge = $('view-badge');
  badge.textContent = control ? 'CONTROL' : 'VIEW ONLY';
  badge.className = 'badge ' + (control ? 'badge-control' : 'badge-view');
  renderDisplays(live ? t.displays : null);

  // The link went quiet: the video is hidden while the grace period runs.
  if (live && t.troubleSecs != null) {
    $('link-count').textContent = String(t.troubleSecs);
    $('modal-link').classList.remove('hidden');
  } else {
    $('modal-link').classList.add('hidden');
  }

  const isPassword = t.phase === 'password';
  $('tab-card-password').classList.toggle('hidden', !isPassword);
  $('tab-card-auto').classList.toggle('hidden', !t.autoSignIn || isPassword);
  if (!isPassword || passwordFor !== t.remoteId) {
    $('tab-password-input').value = '';
    // Passwords are remembered by default (D7): typed once per host.
    $('tab-password-remember').checked = true;
  }
  const name = tabLabel(t);
  const disconnect = { label: 'Cancel', run: () => api.tabDisconnect(t.remoteId) };
  switch (t.phase) {
    case 'connecting':
      passwordFor = null;
      setCard(`Connecting to ${name}…`, 'Waiting for the other computer to answer.', null, disconnect);
      break;
    case 'password': {
      const first = passwordFor !== t.remoteId;
      passwordFor = t.remoteId;
      setCard(
        `${name} needs a password`,
        t.passwordReason === 'saved-rejected'
          ? 'The saved password no longer works.'
          : 'This computer is protected by a password.',
        { label: 'Connect', run: () => submitPassword(t.remoteId) },
        disconnect
      );
      if (first) setTimeout(() => $('tab-password-input').focus(), 0);
      break;
    }
    case 'disconnected':
      passwordFor = null;
      setCard(
        name,
        t.lastEnd || 'Not connected.',
        { label: 'Connect', run: () => startConnecting((c) => api.tabConnect(t.remoteId, c)) },
        // In its own window, closing is the window's X; this puts it back.
        windowRemote
          ? {
              label: 'Back to the main window',
              run: () => api.tabDock(t.remoteId).catch((err) => toast(errorText(err), 'error'))
            }
          : { label: 'Close tab', run: () => api.tabClose(t.remoteId) }
      );
      break;
    default:
      passwordFor = null;
  }
  renderHostScreen(t, live);
}

function submitPassword(remote) {
  const pw = $('tab-password-input').value;
  if (!pw) return;
  api.tabPassword(remote, pw, $('tab-password-remember').checked);
  $('tab-password-input').value = '';
}

function applySnapshot(snapshot) {
  const before = S.tabs;
  const wasInTrouble = new Set(
    [...before.tabs, ...(before.detached || [])]
      .filter((t) => t.troubleSecs != null)
      .map((t) => t.remoteId)
  );
  S.tabs = snapshot;
  const mine = windowRemote
    ? (snapshot.detached || []).filter((t) => t.remoteId === windowRemote)
    : snapshot.tabs;
  for (const t of mine) {
    if (wasInTrouble.has(t.remoteId) && t.phase === 'live' && t.troubleSecs == null) {
      toast(`${tabLabel(t)}: connection restored`);
    }
  }
  // A rename that is no longer possible (the tab closed) is dropped.
  if (renaming && !snapshot.tabs.some((t) => t.remoteId === renaming)) renaming = null;
  renderStrip();
  renderActive();
  render();
  // Names live with the remotes: a rename shows in the recents too.
  if (before.tabs.length !== snapshot.tabs.length || snapshot.tabs.some((t, i) =>
    !before.tabs[i] || before.tabs[i].name !== t.name)) {
    reloadConfig();
  }
}

// D3: the tab chords when the page has the keyboard (a card, the connect
// page). Inside a live video the engine takes them out of the key stream.
function isTabChord(e) {
  const mac = navigator.platform.toUpperCase().includes('MAC');
  const primary = mac ? e.metaKey : e.ctrlKey;
  return primary && e.altKey && (e.key === 'PageDown' || e.key === 'PageUp');
}

export function wireTabs() {
  $('btn-tab-new').addEventListener('click', () => api.tabActivate(null));
  $('btn-tab-connect-all').addEventListener('click', () => startConnecting(api.tabConnectAll));
  $('btn-tab-forget').addEventListener('click', () => {
    const t = activeTab();
    if (t) api.tabForgetPassword(t.remoteId);
  });
  $('tab-password-input').addEventListener('keydown', (e) => {
    const t = activeTab();
    if (!t) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      submitPassword(t.remoteId);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      api.tabDisconnect(t.remoteId);
    }
  });

  $('btn-tab-confirm-cancel').addEventListener('click', closeConfirm);
  $('btn-tab-confirm-ok').addEventListener('click', async () => {
    const action = confirmAction;
    closeConfirm();
    if (!action) return;
    try {
      await action();
    } catch (err) {
      toast(errorText(err), 'error');
    }
  });
  $('modal-tab-confirm').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeConfirm();
  });

  // Dropped past the last tab: to the end.
  const list = $('tab-list');
  list.addEventListener('dragover', (e) => {
    if (dragging && e.target === list) e.preventDefault();
  });
  list.addEventListener('drop', (e) => {
    if (dragging && e.target === list) {
      e.preventDefault();
      api.tabMove(dragging, null).catch((err) => toast(errorText(err), 'error'));
      dragging = null;
    }
  });

  document.addEventListener('keydown', (e) => {
    // A detached window has no strip to step through (09 Decision 2).
    if (windowRemote || !isTabChord(e)) return;
    e.preventDefault();
    if (e.key === 'PageDown') api.tabNext();
    else api.tabPrev();
  });

  api.on('tabs-changed', (e) => applySnapshot(e.payload));
  api.on('tab-toast', (e) => {
    const p = e.payload;
    // Each window shows its own tab's; the main window those of the strip.
    const inWindow = (S.tabs.detached || []).some((x) => x.remoteId === p.remoteId);
    if (windowRemote ? p.remoteId !== windowRemote : inWindow) return;
    const t = S.tabs.tabs.find((x) => x.remoteId === p.remoteId);
    const prefix = t && p.remoteId !== currentRemote() ? `${tabLabel(t)}: ` : '';
    toast(prefix + p.message);
  });
  api.on('tab-displays', (e) => {
    if (e.payload.remoteId === currentRemote()) renderDisplays(e.payload);
  });
  // The host's screen changed (plans/10 U1). The snapshot that follows says
  // the same; this redraws the tab on screen at once.
  api.on('host-screen', (e) => {
    const p = e.payload;
    const t = [...S.tabs.tabs, ...(S.tabs.detached || [])].find((x) => x.remoteId === p.remoteId);
    if (!t) return;
    t.hostScreen = { screen: p.screen, kind: p.kind, why: p.why };
    if (p.remoteId === currentRemote()) renderActive();
  });
}

/** Draw the current strip (boot, or a page reload). */
export async function loadTabs() {
  try {
    applySnapshot(await api.getSnapshot());
  } catch (err) {
    toast(errorText(err), 'error');
  }
}
