//! Files dropped on the video: the `text/uri-list` both X11 (XDND) and
//! Wayland (`wl_data_offer`) drag sources hand over, decoded to local paths.
//!
//! ## References
//! * RFC 2483 §5 (`text/uri-list`: CRLF-separated, `#` comment lines).
//! * RFC 8089 (`file:` URIs; `file:///p` and `file://localhost/p` name a
//!   local file, another host does not).
//! * RFC 3986 §2.1 (percent-encoding of raw path bytes; Linux paths need not
//!   be UTF-8, which GLib's `g_filename_to_uri` and Qt both encode this way).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// The MIME type of a dropped file list.
pub(crate) const URI_LIST: &str = "text/uri-list";

/// Local paths in a URI list. Non-`file:` URIs and files on other hosts are
/// skipped (they are not files the host can be sent).
pub(crate) fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(file_path)
        .collect()
}

fn file_path(uri: &[u8]) -> Option<PathBuf> {
    let rest = strip_prefix_ignore_case(uri, b"file:")?;
    let path = if let Some(authority) = rest.strip_prefix(b"//") {
        let slash = authority.iter().position(|&b| b == b'/')?;
        let (host, path) = authority.split_at(slash);
        if !(host.is_empty() || host.eq_ignore_ascii_case(b"localhost")) {
            return None;
        }
        path
    } else {
        // `file:/p` (no authority), as some KDE versions write it.
        rest
    };
    if !path.starts_with(b"/") {
        return None;
    }
    let decoded = percent_decode(path)?;
    // A NUL cannot be in a Linux path; such a URI is malformed.
    if decoded.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

fn strip_prefix_ignore_case<'a>(s: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &s[prefix.len()..])
}

fn percent_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn crlf_list_with_comments_and_encoded_names() {
        let list =
            b"# from nautilus\r\nfile:///home/u/My%20File.txt\r\nfile://localhost/tmp/a%25b\r\n";
        assert_eq!(
            parse_uri_list(list),
            vec![
                PathBuf::from("/home/u/My File.txt"),
                PathBuf::from("/tmp/a%b")
            ]
        );
    }

    #[test]
    fn lf_only_and_authority_less_uris_are_accepted() {
        assert_eq!(
            parse_uri_list(b"file:/tmp/x\nFILE:///tmp/y"),
            vec![PathBuf::from("/tmp/x"), PathBuf::from("/tmp/y")]
        );
    }

    #[test]
    fn non_utf8_bytes_survive() {
        let paths = parse_uri_list(b"file:///tmp/caf%E9\r\n");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/caf\xe9");
    }

    #[test]
    fn remote_hosts_other_schemes_and_malformed_uris_are_skipped() {
        let list = b"https://example.com/a\r\nfile://otherhost/tmp/a\r\nfile:///tmp/bad%G1\r\n\
                     file:///tmp/nul%00\r\nfile:relative\r\nfile:///tmp/ok\r\n";
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/tmp/ok")]);
        assert!(parse_uri_list(b"").is_empty());
    }
}
