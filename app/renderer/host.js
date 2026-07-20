'use strict';

// Host role — plan §9.1. Handles incoming connect-requests, screen capture,
// the WebRTC offer, and permission enforcement.
App.Host = (function () {
  const UI = App.UI;
  const $ = UI.$;

  const INCOMING_TIMEOUT_MS = 30000;

  let peer = null;            // uuid of the connected/pending viewer
  let permission = 'view';    // live permission for the active session
  let cancelCountdown = null;
  let incomingTimer = null;

  let pc = null;
  let stream = null;
  let ctl = null;             // reliable channel
  let mmChan = null;          // unreliable mouse-move channel
  let negotiating = false;    // true from accept() until teardown
  let disconnectTimer = null; // 8 s grace for ICE 'disconnected'
  let negotiationTimer = null; // accept -> ctl-open watchdog
  let pendingIce = [];        // candidates that arrived before the remote description
  let challenge = null;       // { from, nonce } outstanding password challenge
  let pendingViewerCaps = null; // viewer's decode codec list from connect-request

  // 16 random bytes as lower-case hex — the per-attempt password nonce (§3.2).
  function randomHex() {
    const a = new Uint8Array(16);
    crypto.getRandomValues(a);
    return Array.from(a, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  function isPeer(uuid) {
    return !!peer && uuid === peer;
  }

  // ---- step 1-4: incoming request ---------------------------------------
  async function onConnectRequest(msg) {
    const from = msg.from;
    if (!UI.validUuid(from)) return;

    // Remember the viewer's decode capabilities for codec negotiation (§3.2).
    pendingViewerCaps =
      msg.caps && Array.isArray(msg.caps.decode) ? msg.caps.decode : null;

    // 1. Busy if we are not idle. State alone is not enough: between accept()
    //    and the ctl channel opening (password path) the state is still READY,
    //    so also key off negotiating/peer or a second request would clobber
    //    the in-flight session.
    if (UI.getState() !== 'READY' || negotiating || peer) {
      App.Signaling.send({ type: 'connect-response', to: from, accepted: false, reason: 'busy' });
      return;
    }

    // 2. Re-read config on every request so Settings changes apply to the
    //    NEXT incoming request (plan §6).
    const cfg = await UI.reloadConfig();

    // Two requests arriving within the reloadConfig await window would both
    // pass the busy check at the top; re-check now that we are synchronous
    // again so the second is refused instead of overwriting the first.
    if (UI.getState() !== 'READY' || negotiating || peer) {
      App.Signaling.send({ type: 'connect-response', to: from, accepted: false, reason: 'busy' });
      return;
    }

    // Contract §5 fail-safe: password mode without a password behaves as approve.
    const effectiveMode = cfg.mode === 'password' && cfg.hasPassword ? 'password' : 'approve';

    if (effectiveMode === 'approve') {
      showIncoming(from);
      return;
    }

    // 4. Password mode — challenge-response (§3.2). The plaintext password
    //    never crosses the wire.
    if (msg.password == null) {
      // First contact: issue a fresh nonce and wait for the proof.
      challenge = { from, nonce: randomHex() };
      App.Signaling.send({ type: 'password-required', to: from, nonce: challenge.nonce });
      return; // stay READY
    }

    // msg.password now carries the proof. If we have no outstanding challenge
    // for this peer (e.g. the host restarted), re-issue one rather than failing.
    if (!challenge || challenge.from !== from) {
      challenge = { from, nonce: randomHex() };
      App.Signaling.send({ type: 'password-required', to: from, nonce: challenge.nonce });
      return;
    }

    // The 2 s damper for a wrong proof happens inside the main process. Keep the
    // challenge on failure so the viewer can retry with the same nonce.
    const ok = await window.native.passwordVerifyProof(challenge.nonce, msg.password);
    if (!ok) {
      App.Signaling.send({
        type: 'connect-response',
        to: from,
        accepted: false,
        reason: 'bad-password'
      });
      return;
    }
    challenge = null;

    // While we awaited the verify, another request may have claimed the host
    // (popup shown, or another password accept). Re-check before accepting.
    if (UI.getState() !== 'READY' || negotiating || peer) {
      App.Signaling.send({ type: 'connect-response', to: from, accepted: false, reason: 'busy' });
      return;
    }

    accept(from, cfg.passwordPermission);
  }

  // 3. approve mode: modal with Deny / view / control + 30 s auto-deny.
  function showIncoming(from) {
    peer = from;
    $('incoming-peer').textContent = from;
    UI.setState('INCOMING');

    cancelCountdown = UI.countdown('incoming-count', INCOMING_TIMEOUT_MS / 1000, null);
    incomingTimer = setTimeout(() => resolveIncoming('timeout'), INCOMING_TIMEOUT_MS);
  }

  function clearIncoming() {
    if (cancelCountdown) cancelCountdown();
    if (incomingTimer) clearTimeout(incomingTimer);
    cancelCountdown = incomingTimer = null;
  }

  // choice: 'deny' | 'timeout' | 'view' | 'control'
  function resolveIncoming(choice) {
    if (UI.getState() !== 'INCOMING') return;
    clearIncoming();
    const from = peer;

    if (choice === 'deny' || choice === 'timeout') {
      App.Signaling.send({
        type: 'connect-response',
        to: from,
        accepted: false,
        reason: choice === 'timeout' ? 'timeout' : 'denied'
      });
      peer = null;
      UI.setState('READY');
      return;
    }

    accept(from, choice);
  }

  // ---- step 5: accept -----------------------------------------------------
  function accept(from, perm) {
    peer = from;
    permission = perm === 'control' ? 'control' : 'view';

    // Contract: send the decision FIRST, then start negotiating.
    const sent = App.Signaling.send({
      type: 'connect-response',
      to: from,
      accepted: true,
      permission: permission
    });

    // Socket down at exactly this moment: the viewer never learns it was
    // accepted, so don't start a session it can't join.
    if (!sent) {
      UI.toast('Not connected to the server', 'error');
      peer = null;
      permission = 'view';
      if (UI.getState() !== 'READY') UI.setState(App.Signaling.isRegistered() ? 'READY' : 'OFFLINE');
      return;
    }

    startSession();
  }

  // ---- step 5: capture + offer -------------------------------------------
  async function startSession() {
    negotiating = true;
    const cfg = UI.getConfig();

    // Watchdog: if the ctl channel hasn't opened within 30 s of accepting
    // (viewer crashed, answer lost, ICE dead), give up instead of sitting in
    // a half-open session forever.
    negotiationTimer = setTimeout(() => {
      negotiationTimer = null;
      UI.toast('Connection timed out', 'error');
      endSession(true);
    }, 30000);

    // a. Capture the primary screen. The main-process handler answers this
    //    with no picker; if it ever threw, this promise would hang (§11.3).
    //    Audio is best-effort: if loopback capture fails, fall back to
    //    video-only rather than failing the whole session.
    try {
      const videoConstraints = { frameRate: { ideal: 30, max: 60 } };
      if (cfg.shareAudio) {
        try {
          stream = await navigator.mediaDevices.getDisplayMedia({ video: videoConstraints, audio: true });
        } catch (audioErr) {
          console.warn('[host] audio capture failed; retrying video-only', audioErr);
          stream = await navigator.mediaDevices.getDisplayMedia({ video: videoConstraints, audio: false });
        }
      } else {
        stream = await navigator.mediaDevices.getDisplayMedia({ video: videoConstraints, audio: false });
      }
    } catch (err) {
      console.error('[host] getDisplayMedia failed', err);
      UI.toast('Could not capture the screen', 'error');
      if (peer) App.Signaling.send({ type: 'end-session', to: peer });
      cleanup();
      // The approve path arrives here in INCOMING state — restore it.
      if (UI.getState() !== 'READY') UI.setState(App.Signaling.isRegistered() ? 'READY' : 'OFFLINE');
      return;
    }

    // b. Hint the encoder that this is screen content, not camera motion.
    const track = stream.getVideoTracks()[0];
    try {
      track.contentHint = 'detail';
    } catch (_) {}

    // The user can stop sharing from Windows' own indicator.
    track.addEventListener('ended', () => endSession(true));

    // Point the injector at whichever monitor we are sharing, so control lands
    // on the right screen. null/primary keeps the original verified path.
    window.native.inputSetDisplay(cfg.shareDisplayId || null).catch(() => {});

    // c-e. Peer connection, BOTH data channels before the offer, then the track.
    try {
      pc = new RTCPeerConnection({ iceServers: cfg.iceServers });
    } catch (err) {
      // Only reachable with a hand-corrupted iceServers config.
      console.error('[host] RTCPeerConnection failed', err);
      UI.toast('Invalid ICE server configuration', 'error');
      if (peer) App.Signaling.send({ type: 'end-session', to: peer });
      cleanup();
      if (UI.getState() !== 'READY') UI.setState(App.Signaling.isRegistered() ? 'READY' : 'OFFLINE');
      return;
    }

    ctl = pc.createDataChannel('ctl', { ordered: true });
    mmChan = pc.createDataChannel('mm', { ordered: false, maxRetransmits: 0 });
    wireChannels();

    const sender = pc.addTrack(track, stream);

    // Add any captured system-audio track to the same stream. The viewer's
    // <video> element plays it automatically (it is not muted). Guarded so an
    // audio hiccup never blocks the video session.
    try {
      stream.getAudioTracks().forEach((a) => pc.addTrack(a, stream));
    } catch (audioErr) {
      console.warn('[host] adding audio track failed', audioErr);
    }

    // f. Tuning — each guard is independent; the app must work if all fail (§11.4).
    tuneSender(sender);
    await negotiateCodecs(); // pick the best codec both peers can handle

    // g. ICE + offer.
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

    pc.onconnectionstatechange = onConnectionStateChange;

    try {
      const offer = await pc.createOffer();
      // Tune Opus for system audio (music), not speech: stereo, higher bitrate,
      // in-band FEC for loss resilience, and DTX OFF (DTX on continuous audio is
      // a common source of clicking/ticking). Fail-safe: unchanged if no match.
      const sdp = tuneOpus(offer.sdp);
      await pc.setLocalDescription({ type: 'offer', sdp });
      App.Signaling.send({
        type: 'signal',
        to: peer,
        data: { kind: 'offer', sdp: pc.localDescription.sdp }
      });
    } catch (err) {
      console.error('[host] offer failed', err);
      endSession(true);
    }
  }

  // Rewrite the Opus fmtp line for high-quality stereo system audio.
  function tuneOpus(sdp) {
    try {
      if (!sdp) return sdp;
      const rtpmap = sdp.match(/^a=rtpmap:(\d+) opus\/48000\/2.*$/im);
      if (!rtpmap) return sdp;
      const pt = rtpmap[1];
      const params =
        'minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1;maxaveragebitrate=128000;usedtx=0';

      const fmtpRe = new RegExp('^a=fmtp:' + pt + ' .*$', 'im');
      if (fmtpRe.test(sdp)) {
        return sdp.replace(fmtpRe, 'a=fmtp:' + pt + ' ' + params);
      }
      // No fmtp line yet — add one right after the rtpmap line.
      return sdp.replace(rtpmap[0], rtpmap[0] + '\r\na=fmtp:' + pt + ' ' + params);
    } catch (_) {
      return sdp; // never break the offer over an audio tweak
    }
  }

  function tuneSender(sender) {
    try {
      const params = sender.getParameters();
      params.degradationPreference = 'maintain-resolution';
      if (!params.encodings || !params.encodings.length) params.encodings = [{}];
      // H.264 needs more bitrate than VP9/AV1 for equally sharp text, so give
      // the (now hardware) encoder more headroom to keep quality up.
      params.encodings[0].maxBitrate = 8000000;
      params.encodings[0].scalabilityMode = 'L1T1';
      sender.setParameters(params).catch((e) => console.warn('[host] setParameters', e));
    } catch (err) {
      console.warn('[host] sender tuning skipped', err);
    }
  }

  // Negotiate the video codec: the best one this host can HARDWARE-encode and
  // the viewer advertised it can decode. App.Caps does the hardware detection
  // (encode-timing probe) and the intersection; we just wait for it (bounded)
  // and apply the result to the sender before the offer is built.
  async function negotiateCodecs() {
    try {
      if (!App.Caps) return;
      await App.Caps.whenReady(1500); // don't block a session if the probe is slow
      const order = App.Caps.negotiate(pendingViewerCaps);
      App.Caps.applyPreferences(pc, order);
      console.log('[host] negotiated codec order:', order, 'viewer decode:', pendingViewerCaps);
    } catch (err) {
      console.warn('[host] codec negotiation skipped', err);
    }
  }

  // ---- steps 6-8: channels, permission, input ----------------------------
  function wireChannels() {
    ctl.onopen = () => {
      if (negotiationTimer) {
        clearTimeout(negotiationTimer);
        negotiationTimer = null;
      }
      // 6. Announce the initial permission as soon as the channel is usable.
      sendPerm();
      UI.setState('HOST_ACTIVE');
      $('host-peer').textContent = peer || '';
      $('host-perm').value = permission;
    };
    ctl.onmessage = (e) => onInput(e.data);
    ctl.onclose = () => {
      // 9. A closed control channel means the session is over.
      if (negotiating) endSession(true);
    };

    mmChan.onmessage = (e) => onInput(e.data);
  }

  function sendPerm() {
    if (ctl && ctl.readyState === 'open') {
      ctl.send(JSON.stringify({ t: 'perm', value: permission }));
    }
  }

  const INPUT_TYPES = ['mm', 'md', 'mu', 'wh', 'kd', 'ku'];

  // 7. Input handling — the privileged main process re-validates, but the
  //    permission gate lives here.
  function onInput(raw) {
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch (_) {
      return;
    }
    if (!msg || typeof msg !== 'object') return;

    if (msg.t === 'bye') return endSession(false);

    if (permission !== 'control') return;         // view-only: drop input
    if (!INPUT_TYPES.includes(msg.t)) return;

    window.native.inputInject(msg).catch(() => {}); // fire-and-forget
  }

  // 8. Live permission switch from the session panel.
  function setPermission(next) {
    const prev = permission;
    permission = next === 'control' ? 'control' : 'view';

    // On a control->view drop the viewer sends ku/mu for everything it holds,
    // but our own gate above now discards those messages — so release whatever
    // this side has injected, or keys held at the moment of the switch stay
    // stuck on the host OS.
    if (prev === 'control' && permission === 'view') {
      window.native.inputReleaseAll().catch(() => {});
    }

    sendPerm();
  }

  // ---- step 9: teardown ---------------------------------------------------
  function onConnectionStateChange() {
    if (!pc) return;
    const st = pc.connectionState;

    if (st === 'failed' || st === 'closed') return endSession(true);

    if (st === 'disconnected') {
      // Transient blips are normal; only give up if it persists 8 s.
      if (disconnectTimer) return;
      disconnectTimer = setTimeout(() => {
        disconnectTimer = null;
        if (pc && pc.connectionState === 'disconnected') endSession(true);
      }, 8000);
      return;
    }

    if (disconnectTimer) {
      clearTimeout(disconnectTimer);
      disconnectTimer = null;
    }
  }

  // ---- signaling hooks ----------------------------------------------------
  function wantsSignal(msg) {
    return negotiating && isPeer(msg.from);
  }

  async function onSignal(msg) {
    const data = msg.data;
    if (!data || !pc) return;

    try {
      if (data.kind === 'answer') {
        await pc.setRemoteDescription({ type: 'answer', sdp: data.sdp });
        await flushPendingIce();
      } else if (data.kind === 'ice' && data.candidate) {
        // addIceCandidate rejects (and the candidate is lost) if it lands
        // while setRemoteDescription is still in flight — queue until then.
        if (pc.remoteDescription) {
          await pc.addIceCandidate(data.candidate);
        } else {
          pendingIce.push(data.candidate);
        }
      }
    } catch (err) {
      console.warn('[host] signal handling failed', err);
    }
  }

  async function flushPendingIce() {
    const queued = pendingIce;
    pendingIce = [];
    for (const c of queued) {
      try {
        await pc.addIceCandidate(c);
      } catch (err) {
        console.warn('[host] queued candidate failed', err);
      }
    }
  }

  function onRemoteEnd() {
    endSession(false);
  }

  function cleanup() {
    if (disconnectTimer) clearTimeout(disconnectTimer);
    if (negotiationTimer) clearTimeout(negotiationTimer);
    disconnectTimer = negotiationTimer = null;
    negotiating = false;
    pendingIce = [];
    challenge = null;
    pendingViewerCaps = null;

    // Whatever the session injected and never released (viewer died mid-press,
    // mid-drag, etc.) must not stay stuck on this machine.
    window.native.inputReleaseAll().catch(() => {});
    // Reset the injector to the primary-display path for the next session.
    window.native.inputSetDisplay(null).catch(() => {});

    if (stream) {
      stream.getTracks().forEach((t) => {
        try {
          t.stop();
        } catch (_) {}
      });
    }
    stream = null;

    // Detach handlers BEFORE closing: pc.close() fires channel close events
    // asynchronously, and a stale onclose landing after a new request started
    // would tear that new request down.
    if (ctl) ctl.onopen = ctl.onmessage = ctl.onclose = null;
    if (mmChan) mmChan.onmessage = null;
    if (pc) {
      pc.onicecandidate = null;
      pc.onconnectionstatechange = null;
      try {
        pc.close();
      } catch (_) {}
    }
    pc = ctl = mmChan = null;
    peer = null;
    permission = 'view';
  }

  function endSession(notify) {
    clearIncoming();
    if (!negotiating && !peer) return; // already torn down

    if (ctl && ctl.readyState === 'open') {
      try {
        ctl.send(JSON.stringify({ t: 'bye' }));
      } catch (_) {}
    }
    if (notify && peer) App.Signaling.send({ type: 'end-session', to: peer });

    cleanup();
    if (UI.getState() !== 'READY') UI.setState(App.Signaling.isRegistered() ? 'READY' : 'OFFLINE');
  }

  function wire() {
    $('btn-incoming-deny').addEventListener('click', () => resolveIncoming('deny'));
    $('btn-incoming-view').addEventListener('click', () => resolveIncoming('view'));
    $('btn-incoming-control').addEventListener('click', () => resolveIncoming('control'));
    $('btn-host-end').addEventListener('click', () => endSession(true));
    $('host-perm').addEventListener('change', (e) => setPermission(e.target.value));

    // If the signaling socket drops while the popup is up, the UI jumps to
    // OFFLINE without going through resolveIncoming — clear the leftover
    // timers/peer so a later request starts clean. (While negotiating, the
    // 30 s watchdog owns the cleanup instead.)
    UI.onStateChange((next, prev) => {
      if (prev === 'INCOMING' && next === 'OFFLINE' && !negotiating) {
        clearIncoming();
        peer = null;
      }
    });
  }

  return {
    wire,
    onConnectRequest,
    wantsSignal,
    onSignal,
    onRemoteEnd,
    endSession,
    isPeer,
    setPermission,
    // exposed for tests / later milestones
    getPermission: () => permission,
    getPeer: () => peer,
    getPc: () => pc
  };
})();
