//! Host-side receipt of files pushed by the controller.
//!
//! Everything arriving here is chosen by the remote peer, so it is treated as
//! hostile input. Four rules, all enforced below rather than assumed:
//!
//! 1. **The file name cannot escape the receive directory.** It is reduced to a
//!    bare component by [`protocol::transfer::sanitize_file_name`], and the
//!    final path is verified to still be a direct child of the destination.
//! 2. **Nothing is ever overwritten.** A colliding name gets a ` (2)`, ` (3)` …
//!    suffix, so a transfer can't clobber the user's existing files.
//! 3. **The declared size is a hard cap**, and the bytes actually written are
//!    counted against it — a peer cannot under-declare and then stream forever.
//! 4. **Files land in one predictable place**, never a path the peer picks.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::transfer::{sanitize_file_name, FileMeta, MAX_FILE_BYTES};

/// Where received files are written: `%USERPROFILE%\Downloads\ShareCtrlScreen`.
/// A fixed, obvious location — the sender never influences it.
pub fn receive_dir() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("Downloads").join("ShareCtrlScreen")
}

/// An in-progress inbound file.
pub struct Incoming {
    file: File,
    path: PathBuf,
    written: u64,
    expected: u64,
}

impl Incoming {
    /// Validate `meta` and open the destination file. `Err` means the transfer
    /// must be refused — the caller should not create anything.
    pub fn begin(meta: &FileMeta, dir: &Path) -> Result<Self, String> {
        if meta.size > MAX_FILE_BYTES {
            return Err(format!(
                "file is too large ({} MB, limit {} MB)",
                meta.size / 1_048_576,
                MAX_FILE_BYTES / 1_048_576
            ));
        }
        let name = sanitize_file_name(&meta.name).ok_or("unusable file name")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        let path = unique_path(dir, &name);

        // Belt and braces: after sanitizing AND de-duplicating, the result must
        // still sit directly inside `dir`. If it doesn't, something got through
        // the name filter and we refuse rather than write outside.
        if path.parent() != Some(dir) {
            return Err("refusing a path outside the receive folder".into());
        }

        let file = File::create(&path).map_err(|e| format!("cannot write {path:?}: {e}"))?;
        Ok(Incoming {
            file,
            path,
            written: 0,
            expected: meta.size,
        })
    }

    /// Append one chunk. `Err` aborts the transfer (the partial file is removed
    /// by [`Self::abort`]).
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        // Count what actually arrives, not what was promised: a peer that
        // declared 1 KB must not be able to stream a gigabyte.
        if self.written + bytes.len() as u64 > self.expected {
            return Err("sender exceeded the size it declared".into());
        }
        self.file
            .write_all(bytes)
            .map_err(|e| format!("write failed: {e}"))?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Finish and return the path written. Errors if the sender delivered fewer
    /// bytes than promised (a truncated file is worse than a rejected one).
    pub fn finish(mut self) -> Result<PathBuf, String> {
        let _ = self.file.flush();
        if self.written != self.expected {
            let path = self.path.clone();
            self.abort();
            return Err(format!(
                "incomplete transfer of {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        Ok(self.path)
    }

    /// Drop the partial file — nothing half-written is left behind.
    pub fn abort(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn progress(&self) -> (u64, u64) {
        (self.written, self.expected)
    }
}

/// `dir/name`, or `dir/name (2)` … when taken. Never returns an existing path,
/// so a transfer cannot overwrite the user's files.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        // A leading dot means a dotfile, not an extension.
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (dup){ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(sub: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sharectrl-xfer-{sub}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A transfer announcement as a peer would send it. `drop_at` is irrelevant
    /// to receipt — it only steers the paste afterwards.
    fn meta(name: &str, size: u64) -> FileMeta {
        FileMeta {
            name: name.into(),
            size,
            drop_at: None,
        }
    }

    #[test]
    fn writes_a_file_and_reports_path() {
        let dir = tmp("write");
        let meta = meta("hello.txt", 5);
        let mut inc = Incoming::begin(&meta, &dir).unwrap();
        inc.write(b"hello").unwrap();
        let path = inc.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(path.parent(), Some(dir.as_path()));
    }

    #[test]
    fn traversal_name_stays_inside_the_directory() {
        let dir = tmp("traversal");
        let meta = meta(r"..\..\..\evil.txt", 2);
        let mut inc = Incoming::begin(&meta, &dir).unwrap();
        inc.write(b"xx").unwrap();
        let path = inc.finish().unwrap();
        // Must be dir/evil.txt — NOT three levels up.
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(path.file_name().unwrap(), "evil.txt");
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let dir = tmp("nodup");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"original").unwrap();
        let meta = meta("a.txt", 3);
        let mut inc = Incoming::begin(&meta, &dir).unwrap();
        inc.write(b"new").unwrap();
        let path = inc.finish().unwrap();
        // Suffix goes before the extension, so the file stays openable.
        assert_eq!(path.file_name().unwrap(), "a (2).txt");
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"original");
    }

    #[test]
    fn rejects_more_bytes_than_declared() {
        let dir = tmp("oversend");
        let meta = meta("small.bin", 4);
        let mut inc = Incoming::begin(&meta, &dir).unwrap();
        assert!(inc.write(b"12345").is_err());
    }

    #[test]
    fn rejects_absurd_declared_size() {
        let dir = tmp("huge");
        let meta = meta("huge.bin", MAX_FILE_BYTES + 1);
        assert!(Incoming::begin(&meta, &dir).is_err());
    }

    #[test]
    fn rejects_unusable_name() {
        let dir = tmp("badname");
        let meta = meta("..", 1);
        assert!(Incoming::begin(&meta, &dir).is_err());
    }

    #[test]
    fn truncated_transfer_is_discarded() {
        let dir = tmp("truncated");
        let meta = meta("part.bin", 10);
        let mut inc = Incoming::begin(&meta, &dir).unwrap();
        inc.write(b"123").unwrap();
        let path = dir.join("part.bin");
        assert!(inc.finish().is_err());
        assert!(!path.exists(), "partial file must not be left behind");
    }
}
