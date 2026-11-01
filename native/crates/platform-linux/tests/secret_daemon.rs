//! Round trip through a real Secret Service daemon.
//!
//! Needs an unlocked keyring on the session bus. Run it as:
//! `dbus-run-session -- sh -c 'echo | gnome-keyring-daemon --unlock --components=secrets && cargo test -p platform-linux --test secret_daemon -- --ignored'`
//! (see `plans/checklists/L9.md`). The newline matters: with an empty stdin
//! (`echo -n`) the daemon never creates the `login` collection that the `default`
//! alias resolves to, and every save fails. Ignored by default because a developer
//! machine or CI runner without a keyring would fail it for environmental reasons.
//!
//! One test function on purpose: it changes `DBUS_SESSION_BUS_ADDRESS` to force a
//! failed first attempt, and parallel tests in this binary would race on it.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use platform_api::SecretStore;
use platform_linux::secret::{SecretServiceStore, SERVICE};
use secret_service::blocking::SecretService;
use secret_service::EncryptionType;

const PASSWORD: &str = "correct horse battery staple";

#[test]
#[ignore = "needs an unlocked Secret Service daemon on the session bus (dbus-run-session + gnome-keyring)"]
fn password_round_trips_through_the_keyring() {
    let real_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .expect("run under dbus-run-session so a session bus exists");
    let store = SecretServiceStore;

    // A first attempt with no bus fails. It must not poison later attempts: the
    // connection is not cached, so the same process recovers once the bus is up.
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        "unix:path=/nonexistent/sharectrl-test-bus",
    );
    assert_eq!(store.seal(PASSWORD), None, "no bus: nothing is saved");
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &real_bus);

    let sealed = store
        .seal(PASSWORD)
        .expect("a reachable, unlocked keyring stores the password");
    // config.json gets only the opaque id, never the password.
    assert!(!sealed.contains("correct"));
    assert_eq!(store.open(&sealed).as_deref(), Some(PASSWORD));

    // Read the item back from the daemon directly, bypassing our store, so the
    // test fails for any implementation that does not put the secret in the keyring
    // under the documented attributes.
    let service = SecretService::connect(EncryptionType::Plain).expect("daemon reachable");
    let collection = service
        .get_default_collection()
        .expect("default collection");
    let found = collection
        .search_items(HashMap::from([
            ("service", SERVICE),
            ("username", sealed.as_str()),
        ]))
        .expect("search by attributes");
    assert_eq!(found.len(), 1, "exactly one item for the id");
    let item = &found[0];
    assert_eq!(
        item.get_secret().expect("secret readable"),
        PASSWORD.as_bytes()
    );

    // Forget deletes the item from the daemon, and the id then reads as nothing.
    store.forget(&sealed);
    let remaining = collection
        .search_items(HashMap::from([
            ("service", SERVICE),
            ("username", sealed.as_str()),
        ]))
        .expect("search by attributes");
    assert!(
        remaining.is_empty(),
        "forget removed the item from the daemon"
    );
    assert_eq!(store.open(&sealed), None, "a forgotten id reads as nothing");

    // Forgetting again, or an id the keyring never had, is not an error.
    store.forget(&sealed);
    store.forget("ffffffffffffffffffffffffffffffff");

    // An id the keyring does not know is a plain miss.
    assert_eq!(store.open("ffffffffffffffffffffffffffffffff"), None);
}
