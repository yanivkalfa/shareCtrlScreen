'use strict';

// WebSocket signaling client — contract §3.
App.Signaling = (function () {
  const UI = App.UI;

  // Contract §3.3: these two must be byte-exact. Send the hard-coded string,
  // never JSON.stringify of an object (key order / whitespace must match the
  // server's raw-string auto-responder).
  const PING = '{"type":"ping"}';

  const PING_INTERVAL_MS = 25000;
  const PONG_TIMEOUT_MS = 10000;
  const BACKOFF = [1000, 2000, 4000, 8000, 16000, 30000];

  let ws = null;
  let registered = false;
  let attempt = 0;            // index into BACKOFF
  let missedPongs = 0;
  let pingTimer = null;
  let pongTimer = null;
  let retryTimer = null;
  let stopped = false;
  let duplicate = false;      // this UUID is online elsewhere

  // ---- outbound ----------------------------------------------------------
  function send(obj) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(obj));
    return true;
  }

  function isRegistered() {
    return registered;
  }

  // ---- ping / pong watchdog ----------------------------------------------
  function startPingLoop() {
    stopPingLoop();
    pingTimer = setInterval(sendPing, PING_INTERVAL_MS);
  }

  function stopPingLoop() {
    if (pingTimer) clearInterval(pingTimer);
    if (pongTimer) clearTimeout(pongTimer);
    pingTimer = pongTimer = null;
  }

  function sendPing() {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(PING);

    if (pongTimer) clearTimeout(pongTimer);
    pongTimer = setTimeout(() => {
      missedPongs += 1;
      // 2 consecutive misses => assume the connection is dead.
      if (missedPongs >= 2) {
        console.warn('[signaling] 2 missed pongs — closing socket');
        try {
          ws.close();
        } catch (_) {}
      }
    }, PONG_TIMEOUT_MS);
  }

  function onPong() {
    missedPongs = 0;
    if (pongTimer) clearTimeout(pongTimer);
    pongTimer = null;
  }

  // ---- lifecycle ---------------------------------------------------------
  function start() {
    stopped = false;
    connect();
  }

  function connect() {
    if (stopped) return;
    clearRetry();

    const cfg = UI.getConfig();
    registered = false;
    missedPongs = 0;

    try {
      ws = new WebSocket(cfg.serverUrl);
    } catch (err) {
      console.error('[signaling] bad serverUrl', err);
      scheduleRetry();
      return;
    }

    ws.onopen = () => {
      // Contract §3.1: register is the first message after connecting.
      send({ type: 'register', uuid: cfg.uuid, v: 1 });
    };

    ws.onmessage = (ev) => {
      // The pong is matched as a raw string first (cheapest, and it is what
      // the server's auto-responder emits verbatim).
      if (ev.data === '{"type":"pong"}') return onPong();

      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch (_) {
        return; // ignore non-JSON
      }
      if (!msg || typeof msg !== 'object') return;
      if (msg.type === 'pong') return onPong();

      route(msg);
    };

    ws.onerror = () => {
      /* onclose always follows; handled there */
    };

    ws.onclose = () => {
      stopPingLoop();
      registered = false;
      onDisconnected();
      scheduleRetry();
    };
  }

  function onDisconnected() {
    // §8: if a session is active the WebRTC leg is independent and continues —
    // show a yellow dot instead of dropping the user back to OFFLINE.
    const st = UI.getState();
    if (st === 'HOST_ACTIVE' || st === 'VIEW_ACTIVE') {
      UI.setStatus('yellow', 'server: reconnecting (session active)');
    } else {
      UI.setStatus('red', 'server: disconnected (retrying)');
      if (st !== 'OFFLINE') UI.setState('OFFLINE');
    }
  }

  function scheduleRetry() {
    if (stopped) return;
    clearRetry();
    // A duplicate registration is a standing condition — retry slowly.
    const delay = duplicate ? 30000 : BACKOFF[Math.min(attempt, BACKOFF.length - 1)];
    attempt += 1;
    retryTimer = setTimeout(connect, delay);
  }

  function clearRetry() {
    if (retryTimer) clearTimeout(retryTimer);
    retryTimer = null;
  }

  // Force an immediate reconnect (used when serverUrl changes in Settings).
  function reconnectNow() {
    attempt = 0;
    duplicate = false;
    UI.setBanner('');
    clearRetry();
    stopPingLoop();
    if (ws) {
      ws.onclose = null; // we are driving the reconnect ourselves
      try {
        ws.close();
      } catch (_) {}
      ws = null;
    }
    connect();
  }

  // ---- router (§8) --------------------------------------------------------
  function route(msg) {
    switch (msg.type) {
      case 'registered':
        registered = true;
        attempt = 0;
        duplicate = false;
        UI.setBanner('');
        UI.setStatus('green', 'server: connected');
        // Don't yank an active session back to READY on a mid-session reconnect.
        if (UI.getState() === 'OFFLINE') UI.setState('READY');
        startPingLoop();
        break;

      case 'register-error':
        registered = false;
        if (msg.reason === 'duplicate') {
          duplicate = true;
          UI.setBanner('This ID is already online elsewhere. Retrying every 30 s…');
        } else {
          UI.setBanner('Server rejected registration: ' + String(msg.reason || 'unknown'));
        }
        UI.setStatus('red', 'server: rejected');
        break;

      case 'connect-request':
        if (App.Host) App.Host.onConnectRequest(msg);
        break;

      case 'connect-response':
        if (App.Viewer) App.Viewer.onConnectResponse(msg);
        break;

      case 'password-required':
        if (App.Viewer) App.Viewer.onPasswordRequired(msg);
        break;

      case 'signal':
        // Route to whichever role is currently negotiating.
        if (App.Viewer && App.Viewer.wantsSignal(msg)) App.Viewer.onSignal(msg);
        else if (App.Host && App.Host.wantsSignal(msg)) App.Host.onSignal(msg);
        break;

      case 'end-session':
        if (App.Host && App.Host.isPeer(msg.from)) App.Host.onRemoteEnd();
        else if (App.Viewer && App.Viewer.isPeer(msg.from)) App.Viewer.onRemoteEnd();
        break;

      case 'relay-error':
        onRelayError(msg);
        break;

      default:
        break; // contract §3: unknown types ignored silently
    }
  }

  function onRelayError(msg) {
    if (msg.reason === 'peer-offline') {
      UI.toast('Peer offline', 'error');
      if (App.Viewer && App.Viewer.isPeer(msg.to)) App.Viewer.teardown(false);
      if (App.Host && App.Host.isPeer(msg.to)) App.Host.endSession(false);
    }
  }

  return { start, send, isRegistered, reconnectNow, route };
})();
