//! Linux clipboard: text through `arboard`, file lists through our own selection
//! owner.
//!
//! * **Text** goes through `arboard`, which talks X11 (`CLIPBOARD`, `UTF8_STRING`)
//!   or the Wayland data-control protocols and falls back from the latter to the
//!   former when the compositor lacks them.
//! * **File lists** are published as the three targets file managers use to
//!   paste files (`offer`), by a selection owner of our own: `x11` for X11
//!   sessions and as the fallback on compositors without data-control
//!   (GNOME/Mutter bridges XWayland selections), `wayland` otherwise.
//! * **Change count.** Neither X11 nor Wayland has an OS clipboard counter, so
//!   the count is derived from content: a hash of the text, or of the file list
//!   when we own one. `set_*` return exactly the value `change_count` reports
//!   right after, which is all the echo guard in the engine's `ClipboardSync`
//!   needs: text applied from the peer is recognised as ours and not sent back.
//!   The one consequence of deriving it from content: copying text identical to
//!   what is already on the clipboard is not a change, and is not synced again.
//!
//! ## Lifetime
//!
//! A Linux selection exists only while its owner process serves it. The engine
//! creates one `LinuxClipboard` on its clipboard thread and keeps it for the
//! whole session, so a file list set by `set_files` stays pasteable until the
//! session ends or the user copies something else. Dropping the clipboard
//! releases a file-list selection we still own, cleanly; `arboard` hands text to
//! the desktop's clipboard manager when there is one.
//!
//! ## References
//!
//! * freedesktop.org Clipboard specification (selection ownership, `TARGETS`):
//!   <https://www.freedesktop.org/wiki/ClipboardSpec/>
//! * ICCCM section 2: selections (timestamps, `SelectionRequest`/`Notify`,
//!   `INCR`, `MULTIPLE`): <https://tronche.com/gui/x/icccm/sec-2.html>
//! * RFC 2483 section 5: the `text/uri-list` format (CRLF-terminated lines).
//! * RFC 8089: the `file` URI scheme; GLib `g_filename_to_uri` for the escaping
//!   GTK file managers emit.
//! * Wayland data-control: `ext-data-control-v1` and `wlr-data-control-unstable-v1`
//!   (wayland-protocols / wlr-protocols); `wl-clipboard-rs` documentation.
//! * `x-special/gnome-copied-files` (`copy`/`cut` then URIs) as written by GNOME
//!   Files; `application/x-kde-cutselection` (`0`/`1`) as written by Dolphin.
//! * `arboard` documentation, Linux section.

mod offer;
mod wayland;
mod x11;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use platform_api::{Clipboard, MAX_TEXT_BYTES};

use offer::{text_count, FileOffer};
use wayland::WaylandOwner;
use x11::X11Owner;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whoever currently serves our file-list selection.
enum FileOwner {
    X11(X11Owner),
    Wayland(WaylandOwner),
}

impl FileOwner {
    fn acquire(offer: &FileOffer, prefer_wayland: bool) -> Option<Self> {
        if prefer_wayland {
            match WaylandOwner::acquire(offer) {
                Ok(owner) => return Some(Self::Wayland(owner)),
                Err(reason) => {
                    tracing::debug!("Wayland file clipboard unavailable, using X11: {reason}")
                }
            }
        }
        match X11Owner::acquire(offer) {
            Ok(owner) => Some(Self::X11(owner)),
            Err(reason) => {
                tracing::warn!("cannot put files on the clipboard: {reason}");
                None
            }
        }
    }

    fn is_owned(&self) -> bool {
        match self {
            Self::X11(o) => o.is_owned(),
            Self::Wayland(o) => o.is_owned(),
        }
    }
}

struct OwnedFiles {
    owner: FileOwner,
    count: u64,
}

pub(crate) struct LinuxClipboard {
    prefer_wayland: bool,
    /// Opened on first use and reopened after a connection error: the display
    /// may not exist yet when the clipboard is created, and can go away.
    text: Mutex<Option<arboard::Clipboard>>,
    files: Mutex<Option<OwnedFiles>>,
}

impl LinuxClipboard {
    /// `prefer_wayland`: try the Wayland data-control path for file lists before
    /// X11 (a Wayland session). Text always goes through `arboard`, which picks
    /// its own backend.
    pub(crate) fn new(prefer_wayland: bool) -> Self {
        Self {
            prefer_wayland,
            text: Mutex::new(None),
            files: Mutex::new(None),
        }
    }

    /// Run `f` on the text clipboard, opening it if needed. A failure that means
    /// the connection is gone drops the handle so the next call reconnects.
    fn with_text<R>(
        &self,
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>,
    ) -> Option<R> {
        let mut slot = lock(&self.text);
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => *slot = Some(c),
                Err(e) => {
                    tracing::debug!("text clipboard unavailable: {e}");
                    return None;
                }
            }
        }
        let clipboard = slot.as_mut()?;
        match f(clipboard) {
            Ok(r) => Some(r),
            Err(
                arboard::Error::ContentNotAvailable
                | arboard::Error::ConversionFailure
                | arboard::Error::ClipboardOccupied,
            ) => None,
            Err(e) => {
                tracing::debug!("text clipboard error, reconnecting: {e}");
                *slot = None;
                None
            }
        }
    }

    fn read_text(&self) -> Option<String> {
        self.with_text(|c| c.get_text())
            .filter(|t| !t.is_empty() && t.len() <= MAX_TEXT_BYTES)
    }

    /// Count of our file list while it is still the clipboard.
    fn owned_files_count(&self) -> Option<u64> {
        lock(&self.files)
            .as_ref()
            .filter(|f| f.owner.is_owned())
            .map(|f| f.count)
    }
}

impl Clipboard for LinuxClipboard {
    fn change_count(&self) -> u64 {
        // While our own file list is the clipboard it has no text to hash, and
        // reading text from ourselves would only be a round trip to nothing.
        if let Some(count) = self.owned_files_count() {
            return count;
        }
        text_count(self.read_text().as_deref())
    }

    fn get_text(&self) -> Option<String> {
        if self.owned_files_count().is_some() {
            return None;
        }
        self.read_text()
    }

    fn set_text(&self, text: &str) -> Option<u64> {
        // Refused, never truncated: the same cap `get_text` applies, so what we
        // report as written is what a read would return.
        if text.len() > MAX_TEXT_BYTES {
            return None;
        }
        // Release a file list first so it does not linger as a second owner.
        *lock(&self.files) = None;
        // Empty text is written as an empty string rather than a "clear": on X11
        // a clear only disowns the selection, and a clipboard bridge or manager
        // may immediately restore the previous contents; an owned empty value
        // is deterministic, and reads back as "no text".
        self.with_text(|c| c.set_text(text))?;
        Some(text_count((!text.is_empty()).then_some(text)))
    }

    fn set_files(&self, paths: &[PathBuf]) -> Option<u64> {
        let offer = FileOffer::new(paths)?;
        let mut files = lock(&self.files);
        // Drop the previous selection first: two owners of one selection would
        // fight, and releasing is what ends the old serving thread.
        *files = None;
        let owner = FileOwner::acquire(&offer, self.prefer_wayland)?;
        let count = offer.count();
        *files = Some(OwnedFiles { owner, count });
        Some(count)
    }
}

#[cfg(test)]
mod tests;
