'use strict';

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { app } = require('electron');

// Contract §5 — schema and defaults.
function defaults() {
  return {
    uuid: crypto.randomUUID(),
    serverUrl: 'wss://sharectrl-signal.netameta.workers.dev/ws',
    mode: 'approve',
    passwordHash: null,
    passwordPermission: 'view',
    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }],
    // Host-local extension (not part of contract §5; server never sees it):
    // share system loopback audio alongside the screen.
    shareAudio: true,
    // Which monitor to share: a display id (string) or null for the primary.
    shareDisplayId: null
  };
}

function configPath() {
  return path.join(app.getPath('userData'), 'config.json');
}

let cache = null;

// Fill in any missing key from defaults so an older/partial file still works.
function normalize(raw) {
  const d = defaults();
  const out = {
    // Lower-case: the contract's UUID regex (and the server) accept lower-case
    // v4 only, so a hand-edited upper-case UUID would otherwise be rejected.
    uuid: typeof raw.uuid === 'string' && raw.uuid ? raw.uuid.toLowerCase() : d.uuid,
    serverUrl: typeof raw.serverUrl === 'string' ? raw.serverUrl : d.serverUrl,
    mode: raw.mode === 'password' ? 'password' : 'approve',
    passwordHash: typeof raw.passwordHash === 'string' ? raw.passwordHash : null,
    passwordPermission: raw.passwordPermission === 'control' ? 'control' : 'view',
    iceServers: Array.isArray(raw.iceServers) ? raw.iceServers : d.iceServers,
    shareAudio: typeof raw.shareAudio === 'boolean' ? raw.shareAudio : d.shareAudio,
    shareDisplayId: typeof raw.shareDisplayId === 'string' ? raw.shareDisplayId : null
  };
  return out;
}

function load() {
  if (cache) return cache;

  const p = configPath();
  let raw = null;

  if (fs.existsSync(p)) {
    try {
      raw = JSON.parse(fs.readFileSync(p, 'utf8'));
    } catch (err) {
      // Corrupt JSON: back it up, then start fresh (a new UUID is acceptable here).
      try {
        fs.renameSync(p, p + '.bad');
      } catch (_) {
        /* if even the rename fails we still want a working config */
      }
      raw = null;
    }
  }

  if (raw && typeof raw === 'object') {
    cache = normalize(raw);
    // Persist back if normalize filled anything in (e.g. hand-edited file).
    persist(cache);
  } else {
    cache = defaults();
    persist(cache);
  }

  return cache;
}

// Atomic write: tmp file then rename over the original.
function persist(cfg) {
  const p = configPath();
  const tmp = p + '.tmp';
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(tmp, JSON.stringify(cfg, null, 2), 'utf8');
  fs.renameSync(tmp, p);
}

function save(partial) {
  const cfg = load();
  Object.assign(cfg, partial);
  persist(cfg);
  return cfg;
}

function hash(plain) {
  return crypto.createHash('sha256').update(plain, 'utf8').digest('hex');
}

function verifyPassword(plain) {
  const cfg = load();
  if (!cfg.passwordHash) return false;
  if (typeof plain !== 'string') return false;

  const a = Buffer.from(hash(plain), 'hex');
  const b = Buffer.from(cfg.passwordHash, 'hex');
  if (a.length !== b.length) return false;
  return crypto.timingSafeEqual(a, b);
}

// Challenge-response verification (§3.2). The viewer never sends the password;
// it sends proof = SHA256( SHA256(plaintext) + ':' + nonce ), computed from the
// nonce the host issued. The host holds only SHA256(plaintext) as passwordHash,
// so it can compute the same proof and compare — the plaintext never leaves the
// viewer and never transits the relay.
function verifyProof(nonce, proof) {
  const cfg = load();
  if (!cfg.passwordHash) return false;
  if (typeof nonce !== 'string' || typeof proof !== 'string') return false;
  if (proof.length === 0 || proof.length > 128) return false;

  const expected = crypto
    .createHash('sha256')
    .update(cfg.passwordHash + ':' + nonce)
    .digest('hex');

  const a = Buffer.from(expected, 'hex');
  const b = Buffer.from(proof, 'hex');
  if (a.length !== b.length || b.length === 0) return false;
  return crypto.timingSafeEqual(a, b);
}

module.exports = { load, save, verifyPassword, verifyProof, hash, configPath };
