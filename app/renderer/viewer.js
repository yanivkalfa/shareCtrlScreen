'use strict';

// Viewer role — plan §9.2. Requests a connection, answers the host's offer,
// shows the video and captures input.
App.Viewer = (function () {
  const UI = App.UI;
  const $ = UI.$;

  const REQUEST_TIMEOUT_MS = 30000;

  let peer = null;            // uuid of the host we are talking to
  let permission = 'view';
  let requestTimer = null;
  let cancelCountdown = null;
  let lastPassword = null;    // resent verbatim if the host asks again

  let pc = null;
  let ctl = null;
  let mmChan = null;
  let accepted = false;       // true between connect-response and teardown
  let disconnectTimer = null; // 8 s grace for ICE 'disconnected'
  let pendingIce = [];        // candidates that arrived before the remote description
  let challengeNonce = null;  // nonce from the host's password-required (§3.2)

  // proof = SHA256( SHA256(plaintext) + ':' + nonce ), all lower-case hex.
  async function sha256hex(str) {
    const buf = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(str));
    return Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, '0')).join('');
  }
  async function computeProof(plaintext, nonce) {
    const pwHash = await sha256hex(plaintext);
    return sha256hex(pwHash + ':' + nonce);
  }

  function isPeer(uuid) {
    return !!peer && uuid === peer;
  }

  // ---- step 1: send the request ------------------------------------------
  function startRequest(target, password) {
    peer = target;
    lastPassword = password === undefined ? null : password;

    const sent = App.Signaling.send({ type: 'connect-request', to: target, password: lastPassword });
    if (!sent) {
      // Socket down at this instant — fail loudly now instead of a silent
      // 30 s wait for an answer that was never asked for.
      UI.toast('Not connected to the server', 'error');
      reset();
      return;
    }

    $('requesting-peer').textContent = target;
    UI.setState('REQUESTING');
    startRequestTimer();
  }

  function startRequestTimer() {
    clearRequestTimer();
    cancelCountdown = UI.countdown('requesting-count', REQUEST_TIMEOUT_MS / 1000, null);
    requestTimer = setTimeout(() => {
      // Viewer-owned timeout (contract §3.3). teardown (not reset): by now a
      // pc may exist, and notifying lets the host abandon its half too.
      clearRequestTimer();
      UI.toast('No answer / timed out', 'error');
      teardown(true);
    }, REQUEST_TIMEOUT_MS);
  }

  function clearRequestTimer() {
    if (requestTimer) clearTimeout(requestTimer);
    if (cancelCountdown) cancelCountdown();
    requestTimer = cancelCountdown = null;
  }

  function reset() {
    clearRequestTimer();
    peer = null;
    lastPassword = null;
    accepted = false;
    challengeNonce = null;
    if (UI.getState() !== 'READY') UI.setState(App.Signaling.isRegistered() ? 'READY' : 'OFFLINE');
  }

  // ---- step 2: password prompt -------------------------------------------
  function onPasswordRequired(msg) {
    // Only meaningful while a request is actually outstanding — a stale or
    // duplicate message must not yank the UI into a prompt.
    if (!isPeer(msg.from) || UI.getState() !== 'REQUESTING') return;
    if (typeof msg.nonce !== 'string' || !msg.nonce) return; // malformed challenge
    challengeNonce = msg.nonce;
    clearRequestTimer();
    $('password-input').value = '';
    UI.setState('PASSWORD_PROMPT');
    setTimeout(() => $('password-input').focus(), 50);
  }

  async function submitPassword() {
    const pw = $('password-input').value;
    if (!peer || !challengeNonce) return;
    // Send the proof, never the plaintext (§3.2).
    const proof = await computeProof(pw, challengeNonce);
    startRequest(peer, proof); // fresh 30 s window
  }

  // ---- step 3: the host's decision ---------------------------------------
  const REASON_TEXT = {
    denied: 'Denied by remote user',
    busy: 'Remote is in another session',
    'bad-password': 'Wrong password',
    timeout: 'No answer'
  };

  function onConnectResponse(msg) {
    // A response is only ever expected while REQUESTING (stale/duplicate guard).
    if (!isPeer(msg.from) || UI.getState() !== 'REQUESTING') return;
    clearRequestTimer();

    if (!msg.accepted) {
      UI.toast(REASON_TEXT[msg.reason] || 'Connection refused', 'error');

      if (msg.reason === 'bad-password') {
        // Reopen the prompt rather than dropping the whole attempt.
        $('password-input').value = '';
        UI.setState('PASSWORD_PROMPT');
        setTimeout(() => $('password-input').focus(), 50);
        return;
      }

      reset();
      return;
    }

    permission = msg.permission === 'control' ? 'control' : 'view';
    accepted = true;
    // The host is the offerer — stay in REQUESTING, but with a FRESH 30 s
    // watchdog until the ctl channel opens. Without it, a host that dies right
    // after accepting (or a lost offer) leaves this modal up forever.
    startRequestTimer();
  }

  // ---- step 4: answer the host's offer -----------------------------------
  function wantsSignal(msg) {
    return accepted && isPeer(msg.from);
  }

  async function onSignal(msg) {
    const data = msg.data;
    if (!data) return;

    try {
      if (data.kind === 'offer') {
        await handleOffer(data.sdp);
      } else if (data.kind === 'ice' && data.candidate) {
        // addIceCandidate rejects (losing the candidate) before the remote
        // description is set — queue until handleOffer gets there.
        if (pc && pc.remoteDescription) {
          await pc.addIceCandidate(data.candidate);
        } else {
          pendingIce.push(data.candidate);
        }
      }
    } catch (err) {
      console.warn('[viewer] signal handling failed', err);
    }
  }

  async function handleOffer(sdp) {
    const cfg = UI.getConfig();
    try {
      pc = new RTCPeerConnection({ iceServers: cfg.iceServers });
    } catch (err) {
      // Only reachable with a hand-corrupted iceServers config.
      console.error('[viewer] RTCPeerConnection failed', err);
      UI.toast('Invalid ICE server configuration', 'error');
      teardown(true);
      return;
    }

    // The host creates both channels; match them by label (§4 of the contract).
    pc.ondatachannel = (e) => {
      if (e.channel.label === 'ctl') {
        ctl = e.channel;
        ctl.onopen = onCtlOpen;
        ctl.onmessage = (ev) => onCtlMessage(ev.data);
        ctl.onclose = () => teardown(false);
      } else if (e.channel.label === 'mm') {
        mmChan = e.channel;
      }
    };

    pc.ontrack = (e) => {
      // Fires once per track (video, and audio when the host shares it); both
      // carry the same stream, so the <video> element gets picture + sound.
      $('video').srcObject = e.streams[0];
      // Latency tuning applies to VIDEO only — zeroing the audio jitter buffer
      // starves it and causes crackle. Guarded (§11.4).
      if (e.track && e.track.kind === 'video') {
        try {
          e.receiver.jitterBufferTarget = 0;
        } catch (_) {}
      }
    };

    pc.onicecandidate = (e) => {
      if (!e.candidate || !peer) return;
      App.Signaling.send({
        type: 'signal',
        to: peer,
        data: {
          kind: 'ice',
          candidate: {
            candidate: e.candidate.candidate,
            sdpMid: e.candidate.sdpMid,
            sdpMLineIndex: e.candidate.sdpMLineIndex
          }
        }
      });
    };

    pc.onconnectionstatechange = () => {
      if (!pc) return;
      const st = pc.connectionState;

      if (st === 'failed' || st === 'closed') return teardown(false);

      if (st === 'disconnected') {
        // Mirrors the host's rule (§9.1 step 9): tolerate transient blips, but
        // give up if it persists 8 s. Without this, a peer whose process was
        // killed is only noticed when ICE finally reports 'failed'.
        if (disconnectTimer) return;
        disconnectTimer = setTimeout(() => {
          disconnectTimer = null;
          if (pc && pc.connectionState === 'disconnected') teardown(false);
        }, 8000);
        return;
      }

      if (disconnectTimer) {
        clearTimeout(disconnectTimer);
        disconnectTimer = null;
      }
    };

    await pc.setRemoteDescription({ type: 'offer', sdp });
    await flushPendingIce();
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);

    App.Signaling.send({
      type: 'signal',
      to: peer,
      data: { kind: 'answer', sdp: pc.localDescription.sdp }
    });
  }

  async function flushPendingIce() {
    const queued = pendingIce;
    pendingIce = [];
    for (const c of queued) {
      try {
        await pc.addIceCandidate(c);
      } catch (err) {
        console.warn('[viewer] queued candidate failed', err);
      }
    }
  }

  function onCtlOpen() {
    clearRequestTimer();
    $('view-peer').textContent = peer || '';
    applyPermission(permission);
    UI.setState('VIEW_ACTIVE');

    // A real connection was established — remember this remote for autocomplete.
    if (peer) {
      window.native
        .recentsAdd(peer)
        .then((list) => {
          const cfg = UI.getConfig();
          if (cfg) cfg.recentIds = list;
          UI.renderRecents();
        })
        .catch(() => {});
    }
  }

  // ---- step 5: host control messages -------------------------------------
  function onCtlMessage(raw) {
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch (_) {
      return;
    }
    if (!msg || typeof msg !== 'object') return;

    if (msg.t === 'perm') applyPermission(msg.value);
    else if (msg.t === 'bye') teardown(false);
  }

  function applyPermission(value) {
    permission = value === 'control' ? 'control' : 'view';

    const badge = $('view-badge');
    badge.textContent = permission === 'control' ? 'CONTROL' : 'VIEW ONLY';
    badge.className = 'badge ' + (permission === 'control' ? 'badge-control' : 'badge-view');
    $('video-wrap').classList.toggle('control', permission === 'control');

    setCaptureEnabled(permission === 'control');
    updateShortcutCapture();
  }

  // Ask main to (dis)arm the shortcut hook. It only runs when VIEW_ACTIVE +
  // control + the setting is on; main further gates on window focus.
  function updateShortcutCapture() {
    const cfg = UI.getConfig();
    const want =
      UI.getState() === 'VIEW_ACTIVE' &&
      permission === 'control' &&
      !!(cfg && cfg.captureShortcuts);
    window.native.keyhookSet(want).catch(() => {});
  }

  // ---- step 6: teardown ---------------------------------------------------
  function onRemoteEnd() {
    teardown(false);
  }

  function teardown(notify) {
    setCaptureEnabled(false);
    window.native.keyhookSet(false).catch(() => {}); // always disarm the hook

    if (disconnectTimer) clearTimeout(disconnectTimer);
    disconnectTimer = null;
    pendingIce = [];

    if (ctl && ctl.readyState === 'open') {
      try {
        ctl.send(JSON.stringify({ t: 'bye' }));
      } catch (_) {}
    }
    if (notify && peer) App.Signaling.send({ type: 'end-session', to: peer });

    // Detach handlers BEFORE closing: pc.close() fires channel close events
    // asynchronously, and a stale onclose landing after a new request started
    // would tear that new request down.
    if (ctl) ctl.onopen = ctl.onmessage = ctl.onclose = null;
    if (pc) {
      pc.ondatachannel = null;
      pc.ontrack = null;
      pc.onicecandidate = null;
      pc.onconnectionstatechange = null;
      try {
        pc.close();
      } catch (_) {}
    }
    pc = ctl = mmChan = null;

    const v = $('video');
    v.srcObject = null;

    reset();
  }

  // ---- §9.3 viewer input capture -----------------------------------------
  let captureEnabled = false;
  const keysDown = new Set();     // stuck-key prevention
  const buttonsDown = new Set();  // stuck-button prevention (mirror of keysDown)
  let lastPos = { nx: 0.5, ny: 0.5 }; // last in-picture position, for forced button releases
  let pendingMove = null;         // latest {nx,ny}, flushed once per frame
  let rafId = null;

  function setCaptureEnabled(on) {
    const next = !!on;
    if (next === captureEnabled) return;
    captureEnabled = next;

    if (!captureEnabled) {
      releaseAllInput();
      pendingMove = null;
      if (rafId) cancelAnimationFrame(rafId);
      rafId = null;
    } else {
      rafId = requestAnimationFrame(flushMove);
    }
  }

  function sendCtl(obj) {
    if (ctl && ctl.readyState === 'open') ctl.send(JSON.stringify(obj));
  }

  // Map a pointer event into normalized [0,1] coords inside the *picture*,
  // skipping the letterbox/pillarbox bars (object-fit: contain).
  // `clampToEdge`: for button-UP, clamp out-of-picture coords instead of
  // returning null — a mouseup must NEVER be dropped or the button stays stuck.
  function normalize(e, clampToEdge) {
    const video = $('video');
    if (!video.videoWidth || !video.videoHeight) return null;

    const rect = video.getBoundingClientRect();
    const va = video.videoWidth / video.videoHeight;
    const ba = rect.width / rect.height;

    let contentW, contentH;
    if (ba > va) {
      // pillarbox: bars on the left/right
      contentH = rect.height;
      contentW = contentH * va;
    } else {
      // letterbox: bars on the top/bottom
      contentW = rect.width;
      contentH = contentW / va;
    }

    const offX = rect.left + (rect.width - contentW) / 2;
    const offY = rect.top + (rect.height - contentH) / 2;

    let nx = (e.clientX - offX) / contentW;
    let ny = (e.clientY - offY) / contentH;

    if (nx < 0 || nx > 1 || ny < 0 || ny > 1) {
      if (!clampToEdge) return null; // over the bars -> ignore
      nx = Math.min(1, Math.max(0, nx));
      ny = Math.min(1, Math.max(0, ny));
    }
    return { nx, ny };
  }

  // One mouse-move per animation frame, on the unreliable channel.
  function flushMove() {
    rafId = captureEnabled ? requestAnimationFrame(flushMove) : null;
    if (!pendingMove) return;

    const { nx, ny } = pendingMove;
    pendingMove = null;

    if (mmChan && mmChan.readyState === 'open' && mmChan.bufferedAmount < 65536) {
      mmChan.send(JSON.stringify({ t: 'mm', x: nx, y: ny }));
    }
  }

  // Release every key AND mouse button this viewer is holding down — on blur,
  // on a control->view drop, and on teardown. Buttons need coordinates, so use
  // the last known in-picture position.
  function releaseAllInput() {
    keysDown.forEach((code) => sendCtl({ t: 'ku', code }));
    keysDown.clear();
    buttonsDown.forEach((b) => sendCtl({ t: 'mu', b: b, x: lastPos.nx, y: lastPos.ny }));
    buttonsDown.clear();
  }

  function wireCapture() {
    const wrap = $('video-wrap');

    wrap.addEventListener('mousemove', (e) => {
      if (!captureEnabled) return;
      const p = normalize(e);
      if (p) {
        pendingMove = p;
        lastPos = p;
      }
    });

    wrap.addEventListener('mousedown', (e) => {
      if (!captureEnabled || e.button > 2) return;
      const p = normalize(e);
      if (p) {
        lastPos = p;
        buttonsDown.add(e.button);
        sendCtl({ t: 'md', b: e.button, x: p.nx, y: p.ny });
      }
    });

    wrap.addEventListener('mouseup', (e) => {
      if (!captureEnabled || e.button > 2) return;
      // clampToEdge: a release started inside the picture but ended over the
      // bars must still be delivered, or the host's button stays stuck.
      const p = normalize(e, true);
      if (p) {
        lastPos = p;
        buttonsDown.delete(e.button);
        sendCtl({ t: 'mu', b: e.button, x: p.nx, y: p.ny });
      }
    });

    // Right-click must reach the remote machine, not open a local menu.
    wrap.addEventListener('contextmenu', (e) => e.preventDefault());

    wrap.addEventListener(
      'wheel',
      (e) => {
        if (!captureEnabled) return;
        e.preventDefault();
        // DOM deltas -> Windows wheel units. Note the Y sign flip.
        const dy = e.deltaY === 0 ? 0 : -Math.sign(e.deltaY) * 120;
        const dx = e.deltaX === 0 ? 0 : Math.sign(e.deltaX) * 120;
        sendCtl({ t: 'wh', dx: dx, dy: dy });
      },
      { passive: false }
    );

    window.addEventListener('keydown', (e) => {
      if (!captureEnabled || UI.getState() !== 'VIEW_ACTIVE') return;
      if (e.code === 'F11') return; // leave the local fullscreen toggle alone

      e.preventDefault();
      e.stopPropagation();

      keysDown.add(e.code);
      // Auto-repeat must be forwarded: the remote host won't generate it itself.
      sendCtl({ t: 'kd', code: e.code });
    });

    window.addEventListener('keyup', (e) => {
      if (!captureEnabled || UI.getState() !== 'VIEW_ACTIVE') return;
      if (e.code === 'F11') return;

      e.preventDefault();
      e.stopPropagation();

      keysDown.delete(e.code);
      sendCtl({ t: 'ku', code: e.code });
    });

    // Alt-Tabbing away must not leave keys or buttons stuck down on the host.
    window.addEventListener('blur', releaseAllInput);

    // Keys suppressed locally by the main-process hook (Alt+Tab, Win, …) arrive
    // here instead of as DOM events. Route them through the same send + stuck-key
    // tracking so a lost 'up' is still released on blur/teardown.
    window.native.onPassthroughKey((code, down) => {
      if (!captureEnabled) return;
      if (down) {
        keysDown.add(code);
        sendCtl({ t: 'kd', code });
      } else {
        keysDown.delete(code);
        sendCtl({ t: 'ku', code });
      }
    });
  }

  function cancelRequest() {
    if (peer) App.Signaling.send({ type: 'end-session', to: peer });
    reset();
  }

  function wire() {
    $('btn-connect').addEventListener('click', () => {
      const target = $('remote-id').value.trim();
      if (!UI.validUuid(target)) return UI.toast('That does not look like a valid ID.', 'error');
      const cfg = UI.getConfig();
      if (target === cfg.uuid) return UI.toast('That is this machine’s own ID.', 'error');
      startRequest(target, null);
    });

    $('btn-cancel-request').addEventListener('click', cancelRequest);
    $('btn-password-cancel').addEventListener('click', cancelRequest);
    $('btn-password-ok').addEventListener('click', submitPassword);
    $('password-input').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') submitPassword();
    });
    $('btn-view-end').addEventListener('click', () => teardown(true));
    wireCapture();
  }

  return {
    wire,
    startRequest,
    onConnectResponse,
    onPasswordRequired,
    wantsSignal,
    onSignal,
    onRemoteEnd,
    teardown,
    isPeer,
    updateShortcutCapture,
    getPermission: () => permission,
    getPeer: () => peer,
    getPc: () => pc,
    // channels used by the input-capture module
    getCtl: () => ctl,
    getMm: () => mmChan
  };
})();
