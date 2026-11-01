//! The wire form of a copied file list: the three clipboard targets file
//! managers read and write, built from absolute paths.
//!
//! Nothing here touches a display server, so the byte layouts are unit-tested
//! directly; `x11` and `wayland` only serve the bytes produced here.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// RFC 2483 URI list. What GTK, Qt and every drag-and-drop target accept.
pub(super) const TARGET_URI_LIST: &str = "text/uri-list";
/// GNOME Files, Nemo, Caja, Thunar and PCManFM paste files from this target
/// (preferring it over the URI list, to learn copy versus cut).
pub(super) const TARGET_GNOME: &str = "x-special/gnome-copied-files";
/// Dolphin and the other KDE apps read the copy/cut verdict from here.
pub(super) const TARGET_KDE_CUT: &str = "application/x-kde-cutselection";

/// Bytes GLib's `g_filename_to_uri` leaves unescaped in a path, so the URIs we
/// publish are byte-identical to the ones GTK file managers publish for the same
/// file: RFC 3986 unreserved, the path-safe sub-delimiters, `:`, `@` and `/`.
fn is_path_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b)
}

/// `file://` URI for an absolute path, percent-encoding raw bytes: Linux paths
/// are not required to be UTF-8, and the URI must round-trip them regardless.
fn file_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        // A relative path means nothing in another process's working directory.
        return None;
    }
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if is_path_safe(b) {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    Some(uri)
}

/// One file-list selection, ready to be served.
pub(super) struct FileOffer {
    uri_list: Vec<u8>,
    gnome: Vec<u8>,
    kde_cut: Vec<u8>,
}

impl FileOffer {
    /// `None` for an empty list or any non-absolute path; a partial list would
    /// paste fewer files than were sent, silently.
    pub(super) fn new(paths: &[PathBuf]) -> Option<Self> {
        if paths.is_empty() {
            return None;
        }
        let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect::<Option<_>>()?;

        // RFC 2483: every line, the last included, ends in CRLF.
        let mut uri_list = String::new();
        for uri in &uris {
            uri_list.push_str(uri);
            uri_list.push_str("\r\n");
        }
        // `copy` then one URI per line, LF-separated, no trailing newline: the
        // exact layout GNOME Files writes.
        let mut gnome = String::from("copy");
        for uri in &uris {
            gnome.push('\n');
            gnome.push_str(uri);
        }
        Some(Self {
            uri_list: uri_list.into_bytes(),
            gnome: gnome.into_bytes(),
            // "0" is copy, "1" is cut. We never move the sender's files.
            kde_cut: b"0".to_vec(),
        })
    }

    /// Every target with its payload, in the order they are advertised.
    pub(super) fn targets(&self) -> [(&'static str, &[u8]); 3] {
        [
            (TARGET_URI_LIST, &self.uri_list),
            (TARGET_GNOME, &self.gnome),
            (TARGET_KDE_CUT, &self.kde_cut),
        ]
    }

    /// Content-derived change count for this selection; see [`text_count`].
    pub(super) fn count(&self) -> u64 {
        let mut h = DefaultHasher::new();
        "files".hash(&mut h);
        self.uri_list.hash(&mut h);
        h.finish()
    }
}

/// Content-derived change count for the text clipboard (`None` = no usable
/// text). Linux has no OS counter, so "the clipboard changed" means "its content
/// differs". The domain tag keeps a file list and a text from ever colliding.
pub(super) fn text_count(text: Option<&str>) -> u64 {
    let mut h = DefaultHasher::new();
    "text".hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(paths: &[&str]) -> FileOffer {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        FileOffer::new(&paths).expect("absolute paths")
    }

    #[test]
    fn uri_list_is_crlf_terminated_per_rfc_2483() {
        let o = offer(&["/home/u/a.txt", "/home/u/b.txt"]);
        assert_eq!(
            o.targets()[0].1,
            b"file:///home/u/a.txt\r\nfile:///home/u/b.txt\r\n"
        );
    }

    #[test]
    fn gnome_target_is_copy_then_lf_separated_uris_without_trailing_newline() {
        let o = offer(&["/a", "/b"]);
        assert_eq!(o.targets()[1].1, b"copy\nfile:///a\nfile:///b");
    }

    #[test]
    fn kde_target_says_copy_not_cut() {
        assert_eq!(offer(&["/a"]).targets()[2].1, b"0");
    }

    #[test]
    fn special_characters_are_percent_encoded_like_glib() {
        let o = offer(&["/tmp/a b/100%/x#y?z/\u{e9}.txt"]);
        assert_eq!(
            o.targets()[0].1,
            b"file:///tmp/a%20b/100%25/x%23y%3Fz/%C3%A9.txt\r\n"
        );
        // GLib leaves path sub-delimiters alone.
        let o = offer(&["/t/a(1)+b,c;d=e@f:g'h!i$j&k*l"]);
        assert_eq!(
            o.targets()[0].1,
            b"file:///t/a(1)+b,c;d=e@f:g'h!i$j&k*l\r\n"
        );
    }

    #[test]
    fn non_utf8_paths_survive_as_raw_bytes() {
        use std::ffi::OsStr;
        let p = PathBuf::from(OsStr::from_bytes(b"/t/\xff\xfe"));
        let o = FileOffer::new(&[p]).expect("absolute");
        assert_eq!(o.targets()[0].1, b"file:///t/%FF%FE\r\n");
    }

    #[test]
    fn empty_and_relative_lists_are_refused() {
        assert!(FileOffer::new(&[]).is_none());
        assert!(FileOffer::new(&[PathBuf::from("/ok"), PathBuf::from("rel/x")]).is_none());
    }

    #[test]
    fn counts_follow_content_and_never_collide_across_kinds() {
        assert_eq!(text_count(Some("a")), text_count(Some("a")));
        assert_ne!(text_count(Some("a")), text_count(Some("b")));
        assert_ne!(text_count(Some("a")), text_count(None));
        assert_ne!(offer(&["/a"]).count(), offer(&["/b"]).count());
        assert_ne!(offer(&["/a"]).count(), text_count(None));
    }
}
