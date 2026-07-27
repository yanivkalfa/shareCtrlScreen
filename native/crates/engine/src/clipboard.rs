//! Windows clipboard text access for session clipboard sync.
//!
//! Text only, in both directions. Deliberately **not** files or arbitrary
//! formats: a clipboard can carry rendered HTML, images and file drop-lists, and
//! silently mirroring all of that across machines is a much larger surface than
//! the feature needs.
//!
//! Change detection uses `GetClipboardSequenceNumber`, which the OS bumps on
//! every clipboard write. Polling one integer is far cheaper than a format
//! listener and needs no window of our own — and the same counter gives us the
//! echo guard: after we write a value that arrived from the peer we remember the
//! resulting sequence number, so we never bounce it straight back.

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};

/// Cap on synced text. Clipboards can hold megabytes (a whole document); this
/// keeps a stray Ctrl+C from stalling the control channel.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// RAII guard so every early return still closes the clipboard. Leaving it open
/// blocks every other application on the machine from using it.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Option<Self> {
        // SAFETY: null HWND associates the clipboard with the current task.
        // Contended by other apps, so a failure here is normal — retry later.
        for _ in 0..5 {
            if unsafe { OpenClipboard(Some(HWND(std::ptr::null_mut()))) }.is_ok() {
                return Some(ClipboardGuard);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful OpenClipboard.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// The OS clipboard change counter — cheap to poll, bumped on every write.
pub fn sequence() -> u32 {
    // SAFETY: no arguments, no state.
    unsafe { GetClipboardSequenceNumber() }
}

/// Current clipboard text, or `None` when it holds no text (an image, files, …)
/// or is larger than [`MAX_TEXT_BYTES`].
pub fn get_text() -> Option<String> {
    let _guard = ClipboardGuard::open()?;
    // SAFETY: clipboard is open; the handle is owned by the clipboard, not us,
    // and is only valid until CloseClipboard — we copy out before the guard drops.
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        if handle.0.is_null() {
            return None;
        }
        let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
        if ptr.is_null() {
            return None;
        }
        // The buffer is NUL-terminated UTF-16; bound the scan so a malformed
        // clipboard entry can't run away.
        let max = MAX_TEXT_BYTES / 2;
        let mut len = 0usize;
        while len < max && *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(HGLOBAL(handle.0));
        (!text.is_empty()).then_some(text)
    }
}

/// Replace the clipboard with `text`. Returns the sequence number afterwards so
/// the caller can recognise (and ignore) its own write.
pub fn set_text(text: &str) -> Option<u32> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0); // NUL terminator
    let bytes = std::mem::size_of_val(&utf16[..]);

    let guard = ClipboardGuard::open()?;
    // SAFETY: clipboard is open. The moveable global is handed to the clipboard
    // on success, which then owns it — we must NOT free it in that case.
    unsafe {
        EmptyClipboard().ok()?;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
        let dst = GlobalLock(h) as *mut u16;
        if dst.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        let _ = GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).is_err() {
            // Ownership was NOT transferred — release it ourselves.
            let _ = GlobalFree(Some(h));
            return None;
        }
    }
    // The sequence number advances when the clipboard is CLOSED, not when the
    // data is set, so it must be read after the guard is released. Reading it
    // early returns a stale value, and the echo guard built on it would then
    // mistake our own write for a local edit and bounce the text back to the
    // peer — an endless clipboard ping-pong between the two machines.
    drop(guard);
    Some(sequence())
}

/// Put `paths` on the clipboard as a **file-drop list** (`CF_HDROP`) — byte for
/// byte the same format Explorer writes when you copy a file.
///
/// This is what makes a transferred file behave like a local one: Windows offers
/// no way to synthesize a real OLE drop into another process (posting
/// `WM_DROPFILES` cross-process fails, and `DoDragDrop` can't be driven to
/// completion on a third party's window), but every app that accepts a *pasted*
/// file — chat clients, browsers, Explorer, Office — reads exactly this format.
///
/// Layout: a [`DROPFILES`] header followed immediately by the paths as
/// NUL-separated UTF-16, terminated by one extra NUL.
pub fn set_files(paths: &[std::path::PathBuf]) -> Option<u32> {
    use windows::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return None;
    }
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().to_string_lossy().encode_utf16());
        list.push(0); // separator
    }
    list.push(0); // double-NUL terminates the list

    let header = std::mem::size_of::<DROPFILES>();
    let total = header + list.len() * 2;

    let guard = ClipboardGuard::open()?;
    // SAFETY: the global is sized for header+list and fully written before it is
    // handed over; on success the clipboard owns it and we must not free it.
    unsafe {
        EmptyClipboard().ok()?;
        let h = GlobalAlloc(GMEM_MOVEABLE, total).ok()?;
        let base = GlobalLock(h) as *mut u8;
        if base.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        let df = base as *mut DROPFILES;
        (*df).pFiles = header as u32; // paths start right after the header
        (*df).pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        (*df).fNC = false.into();
        (*df).fWide = true.into(); // UTF-16 paths
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, base.add(header), list.len() * 2);
        let _ = GlobalUnlock(h);
        if SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
            return None;
        }
    }
    // As in `set_text`: the sequence number only advances once the clipboard is
    // closed, so it must be read after the guard is released.
    drop(guard);
    Some(sequence())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard is ONE global OS resource, but Rust runs tests in parallel
    /// threads — without this the file-list and text tests clobber each other's
    /// clipboard mid-assertion and fail for reasons that have nothing to do with
    /// the code under test.
    static CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn file_list_roundtrips_as_a_drop_list() {
        let _lock = exclusive();
        let p = std::env::temp_dir().join("sharectrl-cf-hdrop-test.txt");
        let _ = std::fs::write(&p, b"x");
        let Some(seq) = set_files(std::slice::from_ref(&p)) else {
            return; // clipboard held by another app — not a failure
        };
        assert_eq!(seq, sequence());
        // A file list is NOT text, so text sync must ignore it entirely —
        // otherwise dropping a file would also overwrite the peer's clipboard.
        assert_eq!(get_text(), None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_file_list_is_rejected() {
        assert_eq!(set_files(&[]), None);
    }

    #[test]
    fn sequence_is_readable() {
        // Just proves the API is wired; the value itself is machine state.
        let _ = sequence();
    }

    #[test]
    fn roundtrips_text_and_reports_new_sequence() {
        let _lock = exclusive();
        let marker = "sharectrl clipboard test \u{5d0}\u{5d1}"; // non-ASCII on purpose
        let Some(seq) = set_text(marker) else {
            // A locked clipboard (another app holding it) is not a test failure.
            return;
        };
        assert_eq!(get_text().as_deref(), Some(marker));
        // The echo guard depends on the write bumping the counter.
        assert_eq!(seq, sequence());
    }

    #[test]
    fn empty_text_is_not_reported() {
        let _lock = exclusive();
        if set_text("").is_some() {
            assert_eq!(get_text(), None);
        }
    }
}
