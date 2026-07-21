//! Client configuration schema + challenge-response (contract §5, Plan 04 §9).
//!
//! Ported from `app/main/config.js`: same schema, same defaults, same
//! normalization (fill missing keys), and the same SHA-256 challenge-response so
//! the plaintext password never crosses the wire. Persisted as JSON in the user
//! data dir (atomic tmp-file-then-rename write).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Host access mode (contract §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Mode {
    #[default]
    Approve,
    Password,
}

/// Granted permission — the string `"view"` or `"control"`, never anything else
/// (contract terminology table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Permission {
    #[default]
    View,
    Control,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::View => "view",
            Permission::Control => "control",
        }
    }
}

/// One ICE server entry, passed verbatim to the transport (contract §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Persisted configuration (contract §5 + host-local extensions from config.js).
///
/// `camelCase` on the wire so the JSON file matches contract §5 byte-for-byte
/// (`serverUrl`, `passwordHash`, `passwordPermission`, `iceServers`), and
/// `default` so an older/partial file still loads instead of being discarded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Generated once, never changes — the machine's address.
    pub uuid: String,
    pub server_url: String,
    pub mode: Mode,
    /// SHA-256 hex of the password, or `None` if unset.
    pub password_hash: Option<String>,
    pub password_permission: Permission,
    pub ice_servers: Vec<IceServer>,
    // --- host-local extensions (not part of contract §5) ---
    pub share_audio: bool,
    pub share_display_id: Option<String>,
    /// Remote IDs connected to, most-recent-first (autocomplete). Capped.
    pub recent_ids: Vec<String>,
    /// Viewer-side: capture OS-reserved shortcuts (Alt+Tab/Win) locally.
    pub capture_shortcuts: bool,
}

const MAX_RECENTS: usize = 10;

impl Default for Config {
    fn default() -> Self {
        Config {
            uuid: uuid::Uuid::new_v4().to_string(),
            server_url: "wss://sharectrl-signal.netameta.workers.dev/ws".to_string(),
            mode: Mode::Approve,
            password_hash: None,
            password_permission: Permission::View,
            ice_servers: vec![IceServer {
                urls: "stun:stun.l.google.com:19302".to_string(),
                username: None,
                credential: None,
            }],
            share_audio: true,
            share_display_id: None,
            recent_ids: Vec::new(),
            capture_shortcuts: false,
        }
    }
}

impl Config {
    /// SHA-256 hex of a plaintext (contract §5 / config.js `hash`).
    pub fn hash(plain: &str) -> String {
        let mut h = Sha256::new();
        h.update(plain.as_bytes());
        hex::encode(h.finalize())
    }

    /// Direct password check against the stored hash (constant-time compare).
    pub fn verify_password(&self, plain: &str) -> bool {
        match &self.password_hash {
            Some(stored) => ct_eq_hex(&Self::hash(plain), stored),
            None => false,
        }
    }

    /// Challenge-response verification (contract §3.2, config.js `verifyProof`).
    ///
    /// The viewer sends `proof = SHA256( SHA256(plaintext) + ":" + nonce )`. We
    /// hold only `SHA256(plaintext)` as `password_hash`, so we recompute the same
    /// proof and compare in constant time. Plaintext never transits the relay.
    pub fn verify_proof(&self, nonce: &str, proof: &str) -> bool {
        let stored = match &self.password_hash {
            Some(s) => s,
            None => return false,
        };
        if proof.is_empty() || proof.len() > 128 {
            return false;
        }
        let mut h = Sha256::new();
        h.update(stored.as_bytes());
        h.update(b":");
        h.update(nonce.as_bytes());
        let expected = hex::encode(h.finalize());
        ct_eq_hex(&expected, proof)
    }

    /// Set (or clear) the password, storing only its hash.
    pub fn set_password(&mut self, plain: Option<&str>) {
        self.password_hash = plain.map(Self::hash);
    }

    /// Prepend `id`, de-duplicate, cap the recents list (config.js `addRecent`).
    pub fn add_recent(&mut self, id: &str) {
        let clean = id.trim();
        if clean.is_empty() || clean.len() > 64 {
            return;
        }
        self.recent_ids.retain(|x| x != clean);
        self.recent_ids.insert(0, clean.to_string());
        self.recent_ids.truncate(MAX_RECENTS);
    }

    /// Repair an untrusted/partial deserialized value in place: lower-case the
    /// UUID, clamp enums, cap recents (config.js `normalize`).
    pub fn normalize(&mut self) {
        self.uuid = self.uuid.to_lowercase();
        // Fail-safe: password mode with no hash behaves like approve (§5).
        self.recent_ids.retain(|x| x.len() <= 64);
        self.recent_ids.truncate(MAX_RECENTS);
    }

    /// Effective mode honouring the §5 fail-safe: `password` with no hash ⇒
    /// treat incoming requests as `approve`.
    pub fn effective_mode(&self) -> Mode {
        if self.mode == Mode::Password && self.password_hash.is_none() {
            Mode::Approve
        } else {
            self.mode
        }
    }

    /// Load from `path`, falling back to defaults on missing/corrupt file. A
    /// corrupt file is backed up to `*.bad` before starting fresh (config.js).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Config>(&raw) {
                Ok(mut cfg) => {
                    cfg.normalize();
                    cfg
                }
                Err(_) => {
                    let _ = std::fs::rename(path, path.with_extension("bad"));
                    let cfg = Config::default();
                    let _ = cfg.persist(path);
                    cfg
                }
            },
            Err(_) => {
                let cfg = Config::default();
                let _ = cfg.persist(path);
                cfg
            }
        }
    }

    /// Atomic write: tmp file then rename over the original (config.js `persist`).
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp: PathBuf = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// Constant-time comparison of two equal-purpose hex strings. Returns false if
/// either is not valid hex or lengths differ (mirrors `timingSafeEqual` usage).
fn ct_eq_hex(a: &str, b: &str) -> bool {
    let (Ok(ab), Ok(bb)) = (hex_decode(a), hex_decode(b)) else {
        return false;
    };
    if ab.len() != bb.len() || ab.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    hex::decode(s).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_response_roundtrip() {
        let mut cfg = Config::default();
        cfg.set_password(Some("test123"));
        // Viewer computes proof = SHA256( SHA256(pw) + ":" + nonce ).
        let nonce = "0011223344556677";
        let pw_hash = Config::hash("test123");
        let mut h = Sha256::new();
        h.update(pw_hash.as_bytes());
        h.update(b":");
        h.update(nonce.as_bytes());
        let proof = hex::encode(h.finalize());

        assert!(cfg.verify_proof(nonce, &proof));
        assert!(!cfg.verify_proof(nonce, &"00".repeat(32)));
        assert!(!cfg.verify_proof("wrong-nonce", &proof));
    }

    #[test]
    fn password_mode_without_hash_is_fail_safe_approve() {
        let mut cfg = Config::default();
        cfg.mode = Mode::Password;
        assert_eq!(cfg.effective_mode(), Mode::Approve);
        cfg.set_password(Some("x"));
        assert_eq!(cfg.effective_mode(), Mode::Password);
    }

    #[test]
    fn recents_dedupe_and_cap() {
        let mut cfg = Config::default();
        for i in 0..15 {
            cfg.add_recent(&format!("id-{i}"));
        }
        cfg.add_recent("id-14"); // dup moves to front, no growth
        assert_eq!(cfg.recent_ids.len(), MAX_RECENTS);
        assert_eq!(cfg.recent_ids[0], "id-14");
    }
}
