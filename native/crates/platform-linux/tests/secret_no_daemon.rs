//! With no Secret Service reachable, saving must fail (never fall back to a
//! file) and reading must yield nothing.
//!
//! Its own test binary: the `keyring` store is a process-wide default, set on
//! first use, so this must not share a process with tests that reach a daemon.

#![cfg(target_os = "linux")]

use platform_api::SecretStore;
use platform_linux::secret::SecretServiceStore;

#[test]
fn no_session_bus_means_no_save_and_no_read() {
    // A bus address with no socket behind it: the store cannot connect.
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        "unix:path=/nonexistent/sharectrl-test-bus",
    );
    let store = SecretServiceStore;
    assert_eq!(store.seal("hunter2"), None);
    assert_eq!(store.open("0011223344556677889900aabbccddee"), None);
}
