//! The xdg-desktop-portal restore token, persisted between runs.
//!
//! A restore token lets the host re-open a ScreenCast (and RemoteDesktop) session
//! without showing the consent dialog again. It is single-use: every `Start` hands
//! back a replacement, so the file is always overwritten, never appended.
//!
//! File format, one value per line: the token, then optionally the word `input`
//! when the token was issued by a combined RemoteDesktop + ScreenCast session (so
//! restoring it also restores remote control). A screen-only token must not be
//! mistaken for one that grants input, hence the marker.
//!
//! ## References
//! * ScreenCast `persist_mode` / `restore_token`:
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
//! * Same convention (`$XDG_STATE_HOME/<app>/portal.token`) as `wdotool`;
//!   plans/02-LINUX-FINDINGS.md §1 "Persistence".

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "portal.token";
const INPUT_MARKER: &str = "input";

/// A token read from, or about to be written to, disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub token: String,
    /// The token came from a session that also held RemoteDesktop devices.
    pub covers_input: bool,
}

/// `<state_dir>/portal.token`. `state_dir` is `$XDG_STATE_HOME/sharectrl`
/// (`Paths::log_dir`).
pub fn path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// The stored token, or `None` when there is none or it is unusable. An
/// unreadable or malformed file is treated as "never asked": the worst outcome is
/// one extra consent dialog.
pub fn read(path: &Path) -> Option<StoredToken> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), "cannot read portal token: {e}");
            }
            return None;
        }
    };
    let mut lines = text.lines().map(str::trim);
    let token = lines.next().filter(|t| is_valid(t))?;
    Some(StoredToken {
        token: token.to_owned(),
        covers_input: lines.next() == Some(INPUT_MARKER),
    })
}

/// Replace the stored token. Written 0600 through a temporary file and renamed, so
/// a crash never leaves a truncated token and the secret is never world-readable,
/// not even briefly.
pub fn write(path: &Path, stored: &StoredToken) -> io::Result<()> {
    if !is_valid(&stored.token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "portal restore token is empty or contains whitespace",
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("token.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // `mode` only applies to a freshly created file; a stale temp file from a
    // crashed run keeps its old bits.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(stored.token.as_bytes())?;
    file.write_all(b"\n")?;
    if stored.covers_input {
        file.write_all(INPUT_MARKER.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

/// Record the outcome of a portal `Start`. `spent` = the stored token was handed to
/// that `Start`. Tokens are single-use, so a spent one is gone either way: store the
/// replacement, or remove the file when the portal returned none (version < 4, or the
/// user unticked "remember"), so the preflight does not report a grant that no longer
/// exists. A token that was *not* handed over (another scope) is still valid and is
/// kept unless a replacement arrives. Failures are logged: the worst outcome is one
/// extra consent dialog next time.
pub fn replace(path: &Path, spent: bool, token: Option<&StoredToken>) {
    let result = match token {
        Some(stored) => write(path, stored),
        None if !spent => Ok(()),
        None => {
            tracing::info!("the portal returned no restore token; consent will not persist");
            match fs::remove_file(path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    };
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), "cannot update portal token: {e}");
    }
}

fn is_valid(token: &str) -> bool {
    !token.is_empty() && !token.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(token: &str, covers_input: bool) -> StoredToken {
        StoredToken {
            token: token.into(),
            covers_input,
        }
    }

    #[test]
    fn round_trips_both_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        for scope in [false, true] {
            let t = stored("0b5d-uuid", scope);
            write(&path, &t).unwrap();
            assert_eq!(read(&path), Some(t));
        }
    }

    #[test]
    fn creates_missing_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir.path().join("a/b"));
        write(&path, &stored("t", false)).unwrap();
        assert!(read(&path).is_some());
    }

    #[test]
    fn overwrites_because_tokens_are_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        write(&path, &stored("old", true)).unwrap();
        write(&path, &stored("new", false)).unwrap();
        assert_eq!(read(&path), Some(stored("new", false)));
    }

    #[test]
    fn file_is_owner_only_even_over_a_loose_stale_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let tmp = path.with_extension("token.tmp");
        fs::write(&tmp, "stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, &stored("t", false)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!tmp.exists());
    }

    #[test]
    fn missing_empty_or_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        assert_eq!(read(&path), None);
        for junk in ["", "\n", "two words\n", "   \n"] {
            fs::write(&path, junk).unwrap();
            assert_eq!(read(&path), None, "{junk:?}");
        }
    }

    #[test]
    fn unknown_second_line_means_screen_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        fs::write(&path, "tok\nsomething-else\n").unwrap();
        assert_eq!(read(&path), Some(stored("tok", false)));
    }

    #[test]
    fn replace_stores_the_new_token_or_drops_the_spent_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        replace(&path, false, Some(&stored("first", true)));
        assert_eq!(read(&path), Some(stored("first", true)));
        replace(&path, true, None);
        assert_eq!(read(&path), None);
        assert!(!path.exists());
        // Nothing stored and nothing returned is not an error.
        replace(&path, true, None);
        assert!(!path.exists());
    }

    #[test]
    fn a_token_that_was_not_handed_over_survives_an_empty_answer() {
        // E.g. a combined consent with only a screen-only token stored: that token was
        // not passed to `Start`, so it is still valid when the portal returns none.
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        write(&path, &stored("screen", false)).unwrap();
        replace(&path, false, None);
        assert_eq!(read(&path), Some(stored("screen", false)));
        // A replacement still overwrites it.
        replace(&path, false, Some(&stored("both", true)));
        assert_eq!(read(&path), Some(stored("both", true)));
    }

    #[test]
    fn rejects_tokens_that_would_corrupt_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        for bad in ["", "a b", "a\nb"] {
            let err = write(&path, &stored(bad, false)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(!path.exists());
    }
}
