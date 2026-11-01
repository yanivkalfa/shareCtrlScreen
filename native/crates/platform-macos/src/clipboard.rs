//! macOS clipboard: text and file references on `NSPasteboard.general`.
//!
//! Text only in the text direction, file URLs only in the file direction — the
//! same deliberately narrow surface as the Windows backend, so a copied image or
//! rich document is never mirrored to the peer.
//!
//! Change detection uses `NSPasteboard.changeCount`, which the OS bumps on every
//! write by anyone. Polling one integer is cheap and needs no observer; the same
//! counter is the echo guard: after we write text that came from the peer the
//! caller remembers the resulting count and never sends it straight back.
//!
//! ## References
//!
//! * `NSPasteboard.changeCount` — increments each time the contents of the
//!   pasteboard change: <https://developer.apple.com/documentation/appkit/nspasteboard/changecount>
//! * `clearContents()` — returns the new change count; the pasteboard is owned by
//!   the caller afterwards, so further `setString`/`writeObjects` calls add to the
//!   same generation: <https://developer.apple.com/documentation/appkit/nspasteboard/clearcontents()>
//! * `writeObjects(_:)` with `NSURL` — publishes `public.file-url`, the type
//!   Finder, Mail and Slack read on paste:
//!   <https://developer.apple.com/documentation/appkit/nspasteboard/writeobjects(_:)>
//! * `NSPasteboard.PasteboardType.fileURL` (`public.file-url`):
//!   <https://developer.apple.com/documentation/appkit/nspasteboard/pasteboardtype/fileurl>

use std::path::PathBuf;

use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSString, NSURL};
use platform_api::{Clipboard, MAX_TEXT_BYTES};

/// The general (system-wide) pasteboard.
///
/// Holds nothing: `generalPasteboard` is a process-wide singleton, so fetching it
/// per call is free and the type stays usable from whichever thread the engine
/// creates it on.
pub struct MacClipboard;

impl MacClipboard {
    /// The counter after our own write. Read it **after** the write has landed:
    /// the echo guard compares this value with later polls, so a stale value
    /// would make our own write look like a local edit and bounce it back.
    fn count_now(pasteboard: &NSPasteboard) -> u64 {
        u64::try_from(pasteboard.changeCount()).unwrap_or(0)
    }
}

impl Clipboard for MacClipboard {
    fn change_count(&self) -> u64 {
        Self::count_now(&NSPasteboard::generalPasteboard())
    }

    fn get_text(&self) -> Option<String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        // A file copied in Finder carries its name as a string too, and AppKit may
        // add one when it publishes file URLs. A file list is not text — treating
        // its name as text would overwrite the peer's clipboard whenever a file is
        // dropped.
        // SAFETY: reads an immutable constant exported by AppKit.
        let file_url = unsafe { NSPasteboardTypeFileURL };
        if pasteboard
            .availableTypeFromArray(&NSArray::from_slice(&[file_url]))
            .is_some()
        {
            return None;
        }
        // SAFETY: as above.
        let string_type = unsafe { NSPasteboardTypeString };
        let text = pasteboard.stringForType(string_type)?;
        // `length` counts UTF-16 units, each at least one UTF-8 byte, so a count
        // over the cap proves the UTF-8 form is over it too — refuse before paying
        // for the copy. Oversized text is refused, never truncated: a cut-off paste
        // on the other machine is worse than none.
        if text.length() > MAX_TEXT_BYTES {
            return None;
        }
        let text = text.to_string();
        (!text.is_empty() && text.len() <= MAX_TEXT_BYTES).then_some(text)
    }

    fn set_text(&self, text: &str) -> Option<u64> {
        if text.len() > MAX_TEXT_BYTES {
            // `get_text` could never read it back, so writing it would only
            // destroy whatever the user had on the clipboard.
            return None;
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        // `clearContents` is what bumps the counter and makes us the owner;
        // `setString` then adds to that same generation.
        pasteboard.clearContents();
        // SAFETY: reads an immutable constant exported by AppKit.
        let string_type = unsafe { NSPasteboardTypeString };
        if !pasteboard.setString_forType(&NSString::from_str(text), string_type) {
            tracing::warn!("NSPasteboard refused the text");
            return None;
        }
        Some(Self::count_now(&pasteboard))
    }

    fn set_files(&self, paths: &[PathBuf]) -> Option<u64> {
        if paths.is_empty() {
            return None;
        }
        let mut urls = Vec::with_capacity(paths.len());
        for path in paths {
            // `fileURLWithPath` resolves a relative path against the process cwd,
            // which would paste a different file than the one received.
            if !path.is_absolute() {
                tracing::warn!("clipboard file path is not absolute; not copied");
                return None;
            }
            // A lossy conversion would paste a different file than the one meant.
            let Some(path) = path.to_str() else {
                tracing::warn!("clipboard file path is not valid UTF-8; not copied");
                return None;
            };
            urls.push(ProtocolObject::<dyn NSPasteboardWriting>::from_retained(
                NSURL::fileURLWithPath(&NSString::from_str(path)),
            ));
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        // Real `public.file-url` items, so Finder, Mail and Slack accept the paste
        // as files. Nothing else is written, so no text is left behind.
        if !pasteboard.writeObjects(&NSArray::from_retained_slice(&urls)) {
            tracing::warn!("NSPasteboard refused the file list");
            return None;
        }
        Some(Self::count_now(&pasteboard))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pasteboard is ONE global OS resource but Rust runs tests in parallel
    /// threads; without this the tests clobber each other's contents mid-assertion.
    static PASTEBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        PASTEBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn roundtrips_text_and_reports_the_post_write_count() {
        let _lock = exclusive();
        let clip = MacClipboard;
        let marker = "sharectrl clipboard test \u{5d0}\u{5d1}"; // non-ASCII on purpose
        let before = clip.change_count();
        let count = clip.set_text(marker);
        assert_eq!(clip.get_text().as_deref(), Some(marker));
        // The echo guard depends on the returned value being the post-write one,
        // and on each write advancing the counter exactly once.
        assert_eq!(count, Some(clip.change_count()));
        assert_eq!(count, Some(before + 1));
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn every_write_advances_the_count() {
        let _lock = exclusive();
        let clip = MacClipboard;
        let first = clip.set_text("same");
        let second = clip.set_text("same");
        // Identical content still counts as a write by someone.
        assert!(second > first);
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn file_list_leaves_no_text_behind() {
        let _lock = exclusive();
        let clip = MacClipboard;
        let p = std::env::temp_dir().join("sharectrl-pasteboard-test.txt");
        std::fs::write(&p, b"x").expect("temp file is writable");
        clip.set_text("stale text");
        let count = clip.set_files(std::slice::from_ref(&p));
        assert_eq!(count, Some(clip.change_count()));
        // A file list is NOT text, so text sync must ignore it entirely.
        assert_eq!(clip.get_text(), None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn empty_file_list_is_rejected_and_writes_nothing() {
        let _lock = exclusive();
        let clip = MacClipboard;
        let before = clip.change_count();
        assert_eq!(clip.set_files(&[]), None);
        assert_eq!(clip.change_count(), before);
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn empty_text_is_not_reported() {
        let _lock = exclusive();
        let clip = MacClipboard;
        if clip.set_text("").is_some() {
            assert_eq!(clip.get_text(), None);
        }
    }

    #[test]
    #[ignore = "needs a Mac"]
    fn oversized_text_is_refused_not_truncated() {
        let _lock = exclusive();
        let clip = MacClipboard;
        let big = "a".repeat(MAX_TEXT_BYTES + 1);
        // Refused on write: nothing is written and the counter does not move.
        let before = clip.change_count();
        assert_eq!(clip.set_text(&big), None);
        assert_eq!(clip.change_count(), before);
        // Refused on read: another app put too much text on the clipboard.
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        // SAFETY: reads an immutable constant exported by AppKit.
        let string_type = unsafe { NSPasteboardTypeString };
        assert!(pasteboard.setString_forType(&NSString::from_str(&big), string_type));
        assert_eq!(clip.get_text(), None);
        // The cap itself is still accepted.
        let at_cap = "a".repeat(MAX_TEXT_BYTES);
        assert!(clip.set_text(&at_cap).is_some());
        assert_eq!(clip.get_text().map(|t| t.len()), Some(MAX_TEXT_BYTES));
    }
}
