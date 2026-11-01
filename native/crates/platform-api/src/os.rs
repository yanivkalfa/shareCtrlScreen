//! Clipboard, saved-secret storage, start-at-login and per-user paths.
//! Contracts derived from `engine::{clipboard, secret, autostart, transfer}`.

use std::path::PathBuf;

/// The system clipboard (text + file references).
///
/// Echo-guard contract (the clipboard loop depends on it to avoid two peers
/// bouncing text forever): [`change_count`](Self::change_count) advances on
/// every write by anyone, and `set_*` returns the value **after** our write is
/// committed. Backends without an OS counter (X11/Wayland) derive it from the
/// content and must still satisfy this.
pub trait Clipboard {
    fn change_count(&self) -> u64;
    /// Text only; `None` for no text / empty / non-text. At most
    /// [`MAX_TEXT_BYTES`] bytes of UTF-8 — larger content
    /// returns `None` (never a silent truncation).
    fn get_text(&self) -> Option<String>;
    /// Replace the clipboard with `text`; returns the new change count.
    fn set_text(&self, text: &str) -> Option<u64>;
    /// Put real file references on the clipboard so a paste in any app pastes
    /// the files (CF_HDROP / file URLs / `text/uri-list`). Must not leave text
    /// behind (`get_text` → `None` afterwards). Empty slice → `None`.
    fn set_files(&self, paths: &[PathBuf]) -> Option<u64>;
}

/// Clipboard text size cap (bytes of UTF-8), shared with the wire protocol.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Seals saved auto-login passwords so the config file alone is useless.
/// Failure is `None`, and the caller then does **not** save — never fall back to
/// plaintext. Protects against file exposure, not against code running as the
/// same user.
///
/// `seal` and `open` may block on an OS prompt with no timeout of their own (a
/// locked Keychain or Secret Service keyring asks the user to unlock it), so
/// callers on an async runtime run them in `spawn_blocking` and bound the wait.
pub trait SecretStore: Send + Sync {
    fn seal(&self, plain: &str) -> Option<String>;
    /// `None` for foreign, corrupt or garbage input; never panics.
    fn open(&self, sealed: &str) -> Option<String>;
    /// Drop whatever the OS keeps for `sealed`; called once its config entry is
    /// gone. A store whose sealed string is self-contained (DPAPI) has nothing
    /// to do, which is the default. A store that keeps the secret in an OS
    /// keyring (Keychain, Secret Service) deletes that item. Foreign, corrupt
    /// or already-deleted input is not an error; a failure is logged by the
    /// store, never returned.
    fn forget(&self, sealed: &str) {
        let _ = sealed;
    }
}

/// Start the app at user sign-in (not boot). The registered command is the
/// absolute executable path plus [`AUTOSTART_FLAG`].
pub trait Autostart: Send + Sync {
    fn is_enabled(&self) -> bool;
    /// Disabling an absent entry is success. Errors are user-readable.
    fn set_enabled(&self, on: bool) -> Result<(), String>;
    /// At startup: re-point a stale entry at the current executable, add or
    /// remove it to match `want`. Logs, never fails.
    fn reconcile(&self, want: bool);
}

/// Command-line flag the autostart entry passes; the app starts minimized.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// Per-user locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `config.json` lives here.
    pub config_dir: PathBuf,
    /// `app.log` lives here.
    pub log_dir: PathBuf,
    /// Received files go here (created on demand), e.g. `~/Downloads/ShareCtrlScreen`.
    pub downloads_dir: PathBuf,
}
