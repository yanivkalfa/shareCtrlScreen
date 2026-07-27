//! File-transfer framing for the reliable `bulk` data channel.
//!
//! SCTP preserves message boundaries, so each channel write is one frame and no
//! length prefix is needed — a single leading byte identifies the kind:
//!
//! | byte | frame | payload                                   |
//! |------|-------|-------------------------------------------|
//! | 0    | Begin | JSON [`FileMeta`] — name + total size     |
//! | 1    | Chunk | raw file bytes, in order                  |
//! | 2    | End   | (empty) — all chunks sent                 |
//! | 3    | Abort | UTF-8 reason                              |
//!
//! Transfers are one-at-a-time per session, so frames need no transfer id: the
//! channel is ordered and reliable, and `Begin` implicitly ends any previous
//! transfer. Keeping the format this small is deliberate — the receiver treats
//! everything in it as hostile input (see `transfer` in the engine).

use serde::{Deserialize, Serialize};

/// Chunk payload size. Small enough to stay well under the SCTP message limit
/// and to interleave with video without visible stalls.
pub const CHUNK_BYTES: usize = 16 * 1024;

/// Largest file we will accept, so a peer cannot fill the receiver's disk.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Announced at the start of a transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileMeta {
    /// The sender's file name. **Untrusted** — the receiver sanitizes it before
    /// touching the filesystem ([`sanitize_file_name`]).
    pub name: String,
    pub size: u64,
    /// Where on the remote screen the file was dropped, normalized `[0,1]`. The
    /// receiver focuses the window under this point before pasting, so the file
    /// lands in whatever the user aimed at. `None` ⇒ save only, don't paste.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_at: Option<(f64, f64)>,
}

/// One decoded frame from the bulk channel.
#[derive(Debug, Clone, PartialEq)]
pub enum BulkFrame {
    Begin(FileMeta),
    Chunk(Vec<u8>),
    End,
    Abort(String),
}

impl BulkFrame {
    /// Encode for the wire (one SCTP message).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            BulkFrame::Begin(meta) => {
                out.push(0);
                out.extend_from_slice(&serde_json::to_vec(meta).unwrap_or_default());
            }
            BulkFrame::Chunk(bytes) => {
                out.push(1);
                out.extend_from_slice(bytes);
            }
            BulkFrame::End => out.push(2),
            BulkFrame::Abort(reason) => {
                out.push(3);
                out.extend_from_slice(reason.as_bytes());
            }
        }
        out
    }

    /// Decode a wire frame. `None` for an empty or unknown-kind message, so a
    /// malformed peer can't panic the receiver.
    pub fn decode(bytes: &[u8]) -> Option<BulkFrame> {
        let (kind, rest) = bytes.split_first()?;
        match kind {
            0 => serde_json::from_slice::<FileMeta>(rest)
                .ok()
                .map(BulkFrame::Begin),
            1 => Some(BulkFrame::Chunk(rest.to_vec())),
            2 => Some(BulkFrame::End),
            3 => Some(BulkFrame::Abort(String::from_utf8_lossy(rest).into_owned())),
            _ => None,
        }
    }
}

/// Reduce a peer-supplied file name to something safe to create inside the
/// receive directory: the final path component only, with separators, drive
/// letters, `..`, control characters and Windows-reserved characters removed.
///
/// A remote peer chooses this string, so it is treated as hostile: without this
/// a name like `..\..\Windows\System32\evil.dll` or `C:\autoexec.bat` would
/// escape the download folder entirely. Returns `None` if nothing usable is
/// left, in which case the caller must reject the transfer rather than invent a
/// name.
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    // Take only the last component, treating BOTH separators as such regardless
    // of host platform (the sender may be any OS).
    let base = raw.rsplit(['/', '\\', ':']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() || cleaned == ".." || cleaned == "." {
        return None;
    }
    // Windows reserves these device names with or without an extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Some(format!("_{cleaned}"));
    }
    // Bound the length so we can always append a de-duplicating suffix.
    Some(cleaned.chars().take(180).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let meta = FileMeta {
            name: "notes.txt".into(),
            size: 42,
            drop_at: Some((0.5, 0.25)),
        };
        for f in [
            BulkFrame::Begin(meta),
            BulkFrame::Chunk(vec![1, 2, 3]),
            BulkFrame::End,
            BulkFrame::Abort("nope".into()),
        ] {
            assert_eq!(BulkFrame::decode(&f.encode()), Some(f));
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(BulkFrame::decode(&[]), None); // empty message
        assert_eq!(BulkFrame::decode(&[9, 1, 2]), None); // unknown kind
        assert_eq!(BulkFrame::decode(&[0, b'{']), None); // truncated JSON
    }

    #[test]
    fn sanitize_strips_path_traversal() {
        // The whole point: none of these may escape the receive directory.
        assert_eq!(
            sanitize_file_name(r"..\..\Windows\evil.dll").as_deref(),
            Some("evil.dll")
        );
        assert_eq!(sanitize_file_name("/etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            sanitize_file_name(r"C:\autoexec.bat").as_deref(),
            Some("autoexec.bat")
        );
        assert_eq!(
            sanitize_file_name("plain.txt").as_deref(),
            Some("plain.txt")
        );
    }

    #[test]
    fn sanitize_rejects_empty_and_dots() {
        assert_eq!(sanitize_file_name(""), None);
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name("   "), None);
        assert_eq!(sanitize_file_name(r"a\b\.."), None);
    }

    #[test]
    fn sanitize_defuses_reserved_device_names() {
        assert_eq!(sanitize_file_name("CON").as_deref(), Some("_CON"));
        assert_eq!(sanitize_file_name("nul.txt").as_deref(), Some("_nul.txt"));
    }

    #[test]
    fn sanitize_drops_control_and_illegal_chars() {
        assert_eq!(
            sanitize_file_name("a\u{0}b<>|?*.txt").as_deref(),
            Some("ab.txt")
        );
    }
}
