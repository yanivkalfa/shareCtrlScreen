//! At-rest protection for saved passwords (auto-login) on macOS.
//!
//! The password lives in a **Keychain generic-password item** (service
//! [`SERVICE`], account = a random key id). The sealed string that
//! `config.json` keeps is only that key id: 128 random bits in hex, meaningless
//! on its own.
//!
//! Why an id instead of ciphertext (the DPAPI shape used on Windows):
//! * Copying `config.json` to another Mac or user account yields nothing usable.
//!   The secret is in the Keychain, and the id cannot be turned into a password.
//! * Deleting the Keychain item (Keychain Access, or a Keychain reset) invalidates
//!   the entry: `open` returns `None`, and the user is asked for the password again.
//! * An id the Keychain does not hold is `None`, so no input can yield a password
//!   except one previously stored by [`KeychainSecretStore::seal`].
//!
//! Failure is always `None` and never plaintext: a locked keychain, denied
//! access, or a missing store is logged with `tracing::warn` and the caller then
//! does not save (for `seal`) or asks for the password (for `open`).
//!
//! Limits: the item is readable only while the login keychain is unlocked, so an
//! unattended Mac that has logged out or rebooted cannot auto-login until the
//! user unlocks it. Superseded items (a host saved again) are not deleted yet:
//! `forget` is planned on [`SecretStore`]. When it lands it is `is_key_id`, then
//! `entry`, then `delete_credential`, with `NoEntry` treated as success.
//!
//! ## References
//! * Apple, Keychain Services: <https://developer.apple.com/documentation/security/keychain-services>
//! * keyring 4.2.0 (`Entry::new`, `set_password`, `get_password`, `delete_credential`):
//!   <https://docs.rs/keyring/4.2.0/keyring/>
//! * apple-native-keyring-store 1.0.2, `src/keychain.rs`: the Keychain store the `v1`
//!   feature selects on macOS. `set_generic_password` replaces an existing item with the
//!   same service and account, and `errSecItemNotFound` maps to `NoEntry`.

use platform_api::SecretStore;

/// Keychain service name for every item this app writes (plan 01 §6).
const SERVICE: &str = "dev.netameta.sharectrl";

/// A key id is 128 random bits as 32 lowercase hex digits.
const KEY_ID_LEN: usize = 32;

pub struct KeychainSecretStore;

impl SecretStore for KeychainSecretStore {
    fn seal(&self, plain: &str) -> Option<String> {
        let id = new_key_id();
        let entry = entry(&id)?;
        match entry.set_password(plain) {
            Ok(()) => Some(id),
            Err(e) => {
                tracing::warn!("keychain: could not store the password: {e}");
                None
            }
        }
    }

    fn open(&self, sealed: &str) -> Option<String> {
        // Reject anything that is not one of our ids before any Keychain call, so
        // foreign or corrupt config values never reach the Keychain at all.
        if !is_key_id(sealed) {
            return None;
        }
        let entry = entry(sealed)?;
        match entry.get_password() {
            Ok(password) => Some(password),
            Err(e) => {
                tracing::warn!("keychain: could not read the saved password: {e}");
                None
            }
        }
    }
}

/// A fresh random id. v4 UUIDs draw from the OS CSPRNG; `simple()` renders the
/// 32 lowercase hex digits without dashes.
fn new_key_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn is_key_id(s: &str) -> bool {
    s.len() == KEY_ID_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The Keychain entry for `id`. Fails only when the store is unavailable, which is
/// logged here so callers only see `None`.
fn entry(id: &str) -> Option<keyring::Entry> {
    match keyring::Entry::new(SERVICE, id) {
        Ok(entry) => Some(entry),
        Err(e) => {
            tracing::warn!("keychain: store unavailable: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ids_are_32_lowercase_hex_and_unique() {
        let a = new_key_id();
        let b = new_key_id();
        assert!(is_key_id(&a));
        assert!(is_key_id(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_garbage_without_touching_the_keychain() {
        let upper = "A".repeat(KEY_ID_LEN);
        let not_hex = "g".repeat(KEY_ID_LEN);
        let long = "0".repeat(KEY_ID_LEN + 1);
        for bad in [
            "",
            "not-hex",
            "00112233",
            upper.as_str(),
            not_hex.as_str(),
            long.as_str(),
        ] {
            assert!(!is_key_id(bad), "accepted {bad:?}");
            assert!(KeychainSecretStore.open(bad).is_none());
        }
    }

    /// Removes the Keychain item for `id` so device tests leave nothing behind.
    fn forget(id: &str) {
        if let Ok(entry) = keyring::Entry::new(SERVICE, id) {
            // Best effort: a missing item is the state we want anyway.
            let _ = entry.delete_credential();
        }
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn roundtrips_through_keychain() {
        let secret = "correct horse battery staple";
        let sealed = KeychainSecretStore
            .seal(secret)
            .expect("Keychain available on macOS");
        assert!(is_key_id(&sealed));
        assert!(!sealed.contains("correct"));
        assert_eq!(KeychainSecretStore.open(&sealed).as_deref(), Some(secret));
        // Read the item straight from the Keychain, so an in-memory stand-in for
        // the store could not pass this test.
        let stored = keyring::Entry::new(SERVICE, &sealed)
            .expect("Keychain entry")
            .get_password()
            .expect("Keychain item");
        assert_eq!(stored, secret);
        forget(&sealed);
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn unknown_id_is_none() {
        assert!(KeychainSecretStore.open(&new_key_id()).is_none());
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn deleted_item_is_none() {
        let sealed = KeychainSecretStore
            .seal("hunter2")
            .expect("Keychain available on macOS");
        forget(&sealed);
        assert!(KeychainSecretStore.open(&sealed).is_none());
    }
}
