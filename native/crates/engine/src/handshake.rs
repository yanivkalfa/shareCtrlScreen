//! Approval / challenge-response helpers for the connection handshake
//! (contract §3.2). Portable and unit-tested.

use protocol::{Config, Permission};
use sha2::{Digest, Sha256};

/// The host's decision for an approve-mode popup (contract §3.2: Deny / View
/// only / View + control).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Deny,
    Allow(Permission),
}

/// The app supplies this to answer approve-mode popups. It is async so the app
/// can await a human clicking the WebView2 modal (30 s auto-deny handled by the
/// app, contract §3.3). Boxed-future form avoids an `async-trait` dependency.
pub trait Decider: Send + Sync {
    fn decide<'a>(
        &'a self,
        from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>>;
}

/// A decider that always denies — the safe default until the app wires the popup.
pub struct DenyAll;
impl Decider for DenyAll {
    fn decide<'a>(
        &'a self,
        _from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async { ApprovalDecision::Deny })
    }
}

/// 16 random bytes as lower-case hex (contract §3.2 nonce).
pub fn random_nonce() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Viewer-side proof: `SHA256( SHA256(plaintext) + ":" + nonce )` (contract
/// §3.2). Matches [`Config::verify_proof`] on the host.
pub fn compute_proof(plaintext: &str, nonce: &str) -> String {
    let pw_hash = Config::hash(plaintext);
    let mut h = Sha256::new();
    h.update(pw_hash.as_bytes());
    h.update(b":");
    h.update(nonce.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_verifies_against_config() {
        let mut cfg = Config::default();
        cfg.set_password(Some("hunter2"));
        let nonce = random_nonce();
        let proof = compute_proof("hunter2", &nonce);
        assert!(cfg.verify_proof(&nonce, &proof));
        assert!(!cfg.verify_proof(&nonce, &compute_proof("wrong", &nonce)));
    }

    #[test]
    fn nonce_is_16_bytes_hex() {
        assert_eq!(random_nonce().len(), 32);
    }
}
