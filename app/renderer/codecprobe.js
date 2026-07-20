'use strict';

// Codec capability detection + per-session negotiation (App.Caps).
//
// Goal: never hard-code a codec for one GPU. Each machine detects what it can do
// and the two peers agree on the best common codec per session.
//
// Two detectors, because WebCodecs alone can't tell hardware from software (the
// spec has no "require-hardware"):
//   * isConfigSupported()  — BREADTH: which codecs this machine supports at all
//     (used for the viewer's decode list).
//   * encode-timing probe  — GROUND TRUTH for the host: actually encode a few
//     1080p frames per codec and time it. Hardware ≈ 2-5 ms/frame; software is
//     15 ms+ and is what makes video fall behind. This runs once at boot.
//
// Negotiation (host side): pick the best codec the host can HARDWARE-encode and
// the viewer can decode. Conservative order so we never regress the common case:
//   1. Hardware H.264 (near-universal GPU encode + viewer decode) — the default.
//   2. If no H.264 hardware: any other codec the host hardware-encodes.
//   3. If no hardware at all: software, H.264 first (lightest).
App.Caps = (function () {
  // WebRTC can only actually use these four (HEVC isn't exposed to WebRTC), so
  // there is no point negotiating anything else.
  const CANDIDATES = [
    { name: 'H264', probe: 'avc1.42E01F', mime: 'video/h264' },
    { name: 'AV1', probe: 'av01.0.04M.08', mime: 'video/av1' },
    { name: 'VP9', probe: 'vp09.00.10.08', mime: 'video/vp9' },
    { name: 'VP8', probe: 'vp8', mime: 'video/vp8' }
  ];

  const HW_MS_THRESHOLD = 8; // avg ms/frame at 1080p below which we call it hardware

  let decodeSupported = []; // names the local machine can decode (viewer side)
  let encodeSupported = []; // names the local machine can encode at all (host side)
  let hwEncode = {}; // name -> boolean (hardware encode), filled by the timing probe
  let hwReadyResolve = null;
  const hwReady = new Promise((res) => (hwReadyResolve = res));

  async function decSupported(codec) {
    if (typeof VideoDecoder === 'undefined' || !VideoDecoder.isConfigSupported) return false;
    try {
      const r = await VideoDecoder.isConfigSupported({
        codec,
        codedWidth: 1920,
        codedHeight: 1080
      });
      return !!(r && r.supported);
    } catch (_) {
      return false;
    }
  }

  async function encSupported(codec) {
    if (typeof VideoEncoder === 'undefined' || !VideoEncoder.isConfigSupported) return false;
    try {
      const r = await VideoEncoder.isConfigSupported({
        codec,
        width: 1920,
        height: 1080,
        bitrate: 8000000,
        framerate: 30,
        latencyMode: 'realtime'
      });
      return !!(r && r.supported);
    } catch (_) {
      return false;
    }
  }

  // Actually encode N 1080p frames and time them. Returns avg ms/frame or null.
  async function encodeTiming(codec) {
    if (typeof VideoEncoder === 'undefined' || typeof VideoFrame === 'undefined') return null;
    if (typeof OffscreenCanvas === 'undefined') return null;

    const W = 1920;
    const H = 1080;
    const N = 10;
    let enc = null;
    try {
      const canvas = new OffscreenCanvas(W, H);
      const ctx = canvas.getContext('2d');
      enc = new VideoEncoder({ output: () => {}, error: () => {} });
      enc.configure({
        codec,
        width: W,
        height: H,
        bitrate: 8000000,
        framerate: 30,
        hardwareAcceleration: 'prefer-hardware',
        latencyMode: 'realtime'
      });

      const drawEncode = (i) => {
        // Change the frame each iteration so the encoder does real work.
        ctx.fillStyle = i % 2 ? '#1b3b5f' : '#3a6ea5';
        ctx.fillRect(0, 0, W, H);
        ctx.fillStyle = '#c8d6e5';
        ctx.fillRect((i * 137) % W, (i * 71) % H, 400, 300);
        const frame = new VideoFrame(canvas, { timestamp: i * 33333 });
        enc.encode(frame, { keyFrame: i === 0 });
        frame.close();
      };

      const t0 = performance.now();
      // Early-bail: measure the first 2 frames. Hardware clears them in a few ms;
      // a clearly-slow codec (software AV1/VP9) is settled here instead of
      // grinding through all N frames (which could take seconds).
      const EARLY = 2;
      for (let i = 0; i < EARLY; i++) drawEncode(i);
      await enc.flush();
      const earlyAvg = (performance.now() - t0) / EARLY;
      if (earlyAvg > HW_MS_THRESHOLD * 2) return earlyAvg; // definitely software

      for (let i = EARLY; i < N; i++) drawEncode(i);
      await enc.flush();
      return (performance.now() - t0) / N;
    } catch (_) {
      return null;
    } finally {
      try {
        if (enc && enc.state !== 'closed') enc.close();
      } catch (_) {}
    }
  }

  // Probe hardware encode. Optimised: H.264 first; if it's hardware we stop
  // (H.264 hardware is our preferred pick anyway), so the common case is one
  // fast probe instead of also running the slow software AV1 path.
  async function probeEncodeHardware() {
    const result = {};
    for (const c of CANDIDATES) {
      if (!encodeSupported.includes(c.name)) {
        result[c.name] = false;
        continue;
      }
      /* eslint-disable no-await-in-loop */
      const ms = await encodeTiming(c.probe);
      /* eslint-enable no-await-in-loop */
      result[c.name] = ms != null && ms < HW_MS_THRESHOLD;
      if (c.name === 'H264' && result.H264) break; // good enough — stop probing
    }
    return result;
  }

  async function init() {
    // Fast breadth check first (both roles need this quickly).
    for (const c of CANDIDATES) {
      /* eslint-disable no-await-in-loop */
      if (await decSupported(c.probe)) decodeSupported.push(c.name);
      if (await encSupported(c.probe)) encodeSupported.push(c.name);
      /* eslint-enable no-await-in-loop */
    }
    console.log('[caps] decode:', decodeSupported, 'encode:', encodeSupported);

    // Hardware encode probe in the background — only the host consumes it, and
    // negotiation awaits it with a timeout, so a slow probe never blocks a call.
    probeEncodeHardware()
      .then((hw) => {
        hwEncode = hw;
        console.log('[caps] hardware encode:', hw);
      })
      .catch(() => {})
      .finally(() => hwReadyResolve());
  }

  // Viewer's decode list, sent in connect-request.
  function decodeList() {
    return decodeSupported.slice();
  }

  // Wait for the hardware probe, but never longer than timeoutMs.
  function whenReady(timeoutMs) {
    return Promise.race([
      hwReady,
      new Promise((res) => setTimeout(res, timeoutMs || 1500))
    ]);
  }

  // Host: order codecs best-first given the viewer's decode capabilities.
  function negotiate(viewerDecode) {
    // If the viewer didn't advertise, assume it can decode anything WebRTC would
    // normally offer (safe: H.264 decode is universal).
    const canDecode =
      Array.isArray(viewerDecode) && viewerDecode.length
        ? (n) => viewerDecode.includes(n)
        : () => true;
    const canUse = (n) => canDecode(n) && encodeSupported.includes(n);
    const isHw = (n) => !!hwEncode[n];

    const order = [];
    const push = (n) => {
      if (canUse(n) && !order.includes(n)) order.push(n);
    };

    // 1. Hardware H.264 — the safe, fast default.
    if (isHw('H264')) push('H264');
    // 2. Otherwise any other hardware encoder the viewer supports (quality order).
    if (!order.length) {
      for (const n of ['AV1', 'VP9', 'VP8']) if (isHw(n)) push(n);
    }
    // 3. Software fallback — H.264 first (lightest), then the rest.
    for (const n of ['H264', 'VP9', 'VP8', 'AV1']) push(n);

    return order;
  }

  // Apply a best-first codec name order to the peer connection's video sender.
  function applyPreferences(pc, names) {
    try {
      if (!pc || !names || !names.length) return;
      const caps = RTCRtpSender.getCapabilities('video');
      if (!caps || !caps.codecs) return;

      const mimeRank = {};
      names.forEach((n, i) => {
        const c = CANDIDATES.find((x) => x.name === n);
        if (c) mimeRank[c.mime] = i;
      });
      const rank = (codec) => {
        const mt = (codec.mimeType || '').toLowerCase();
        return mt in mimeRank ? mimeRank[mt] : names.length + 1;
      };

      const ordered = caps.codecs.slice().sort((a, b) => rank(a) - rank(b));
      const tx = pc
        .getTransceivers()
        .find((t) => t.sender && t.sender.track && t.sender.track.kind === 'video');
      if (tx && tx.setCodecPreferences) tx.setCodecPreferences(ordered);
    } catch (err) {
      console.warn('[caps] applyPreferences failed', err);
    }
  }

  return { init, decodeList, whenReady, negotiate, applyPreferences };
})();
