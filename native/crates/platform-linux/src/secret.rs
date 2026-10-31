//! Saved passwords (auto-login) in the desktop keyring, through the freedesktop
//! Secret Service (GNOME Keyring, KWallet, KeePassXC, ...).
//!
//! `seal` stores the password as an item in the user's keyring and returns only
//! a random id, which is what `config.json` keeps. The config file alone is
//! therefore useless: the credential lives in the keyring, which the desktop
//! already locks with the user's login. Nothing is ever written to a file of our
//! own: if no keyring is reachable, `seal` returns `None` and the engine does not
//! save the password.
//!
//! ## Connection lifecycle
//! The Secret Service connection is opened per call, not once at startup, and no
//! result is cached. Each `seal` and `open` builds a fresh store from
//! `zbus-secret-service-keyring-store`. So a first attempt made before the
//! desktop keyring is running fails, and the next call tries again: a process
//! started early in the session recovers without a restart. The keyring crate's
//! v1 layer is not used because it caches the first initialisation result for the
//! life of the process (a `LazyLock`), which would make that early failure
//! permanent.
//!
//! The item attributes are fixed by the store to `service` and `username`, so the
//! opaque id is stored as `username`.
//!
//! ## References
//! * freedesktop Secret Service API draft: <https://specifications.freedesktop.org/secret-service/latest/>
//!   (item attributes, collections, locking and prompts, error names).
//! * `keyring-core` 1.0.1 (`api::CredentialStoreApi::build`, `Entry`, `Error`):
//!   <https://docs.rs/keyring-core/1.0.1>. `NoEntry` is a miss, `NoStorageAccess`
//!   means the keyring is locked or its unlock was refused.
//! * `zbus-secret-service-keyring-store` 1.0.1: `Store::new` connects to the
//!   daemon (failure is `PlatformFailure`); `cred.rs` sets the `service` and
//!   `username` attributes and the item label.
//! * `secret-service` 5.2.0 (`item.rs`, `collection.rs`): a locked item makes the
//!   daemon run its unlock prompt, and the call waits for the answer.

use keyring_core::api::CredentialStoreApi;
use keyring_core::{Entry, Error};
use platform_api::SecretStore;
use uuid::Uuid;
use zbus_secret_service_keyring_store::Store;

/// Secret Service `service` attribute of every item we create. Reverse-DNS, so it
/// cannot collide with another application's items.
pub const SERVICE: &str = "dev.netameta.sharectrl";

/// Length of a sealed id: a random v4 UUID in simple (hyphen-free) lowercase hex.
const ID_LEN: usize = 32;

/// Saved passwords in the user's Secret Service keyring.
pub struct SecretServiceStore;

impl SecretStore for SecretServiceStore {
    fn seal(&self, plain: &str) -> Option<String> {
        // 122 random bits: unguessable, and it is only ever a lookup key.
        let id = Uuid::new_v4().simple().to_string();
        let entry = entry(&id)?;
        match entry.set_password(plain) {
            Ok(()) => Some(id),
            Err(e) => {
                report(&e, "save");
                None
            }
        }
    }

    fn open(&self, sealed: &str) -> Option<String> {
        // Checked before any D-Bus call: config.json can hold anything, and a
        // malformed value must never reach the keyring.
        if !is_id(sealed) {
            return None;
        }
        let entry = entry(sealed)?;
        match entry.get_password() {
            Ok(plain) => Some(plain),
            Err(e) => {
                report(&e, "read");
                None
            }
        }
    }
}

/// The keyring entry for `id`, from a fresh connection to the Secret Service.
/// Failing to connect is the usual "no keyring running" case, so it gets the
/// message that tells the user what to start.
fn entry(id: &str) -> Option<Entry> {
    let store = match Store::new() {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "no Secret Service (gnome-keyring/kwallet) is running ({e}); \
                 saved logins are unavailable until it starts"
            );
            return None;
        }
    };
    match store.build(SERVICE, id, None) {
        Ok(entry) => Some(entry),
        Err(e) => {
            report(&e, "use");
            None
        }
    }
}

/// Logs a keyring failure with the step that failed and what the user can do.
/// Keyring messages never include the password.
fn report(e: &Error, action: &str) {
    match e {
        Error::NoStorageAccess(_) => tracing::warn!(
            "could not {action} the saved password: the keyring is locked or its \
             unlock was refused; unlock it (gnome-keyring/kwallet) to use saved logins"
        ),
        Error::NoEntry => {
            tracing::warn!("could not {action} the saved password: it is no longer in the keyring")
        }
        _ => tracing::warn!("could not {action} the saved password: {e}"),
    }
}

/// True for exactly the shape `seal` produces: 32 lowercase hex digits.
fn is_id(s: &str) -> bool {
    s.len() == ID_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_have_the_sealed_shape() {
        for _ in 0..16 {
            let id = Uuid::new_v4().simple().to_string();
            assert!(is_id(&id), "{id} should be a valid id");
        }
    }

    #[test]
    fn rejects_garbage_without_touching_the_keyring() {
        // Every case must fail the shape check, so none reaches D-Bus.
        let store = SecretServiceStore;
        assert_eq!(store.open(""), None);
        assert_eq!(store.open("not-an-id"), None);
        assert_eq!(store.open(&"A".repeat(ID_LEN)), None, "uppercase hex");
        assert_eq!(store.open(&"a".repeat(ID_LEN + 1)), None, "too long");
        assert_eq!(store.open(&"a".repeat(ID_LEN - 1)), None, "too short");
        assert_eq!(
            store.open("0011223344556677889900aabbccddeg"),
            None,
            "32 chars but one non-hex byte"
        );
        assert_eq!(
            store.open(&"é".repeat(16)),
            None,
            "32 bytes of multi-byte UTF-8"
        );
        assert!(!is_id("00000000-0000-4000-8000-000000000000"), "hyphenated");
    }
}
