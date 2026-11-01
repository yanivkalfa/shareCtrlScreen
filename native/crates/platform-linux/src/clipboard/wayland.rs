//! Owns the Wayland clipboard selection to serve a file list, through the
//! data-control protocols (`ext-data-control-v1` / `wlr-data-control`) via
//! `wl-clipboard-rs`.
//!
//! Wayland, like X11, keeps no clipboard storage: the client that set the
//! selection answers each paste, so [`WaylandOwner`] runs the serving loop on a
//! thread for as long as the selection is ours. Compositors without a
//! data-control protocol (GNOME's Mutter, Weston) refuse the setup; the caller
//! then falls back to the X11 path through XWayland, which they bridge.

use std::sync::mpsc;
use std::thread::JoinHandle;

use wl_clipboard_rs::copy::{clear, ClipboardType, MimeSource, MimeType, Options, Seat, Source};

use super::offer::FileOffer;

pub(super) struct WaylandOwner {
    /// Returns when the compositor tells us another client took the selection,
    /// so "finished" is exactly "no longer owned".
    thread: Option<JoinHandle<()>>,
}

impl WaylandOwner {
    /// Take the selection and start serving `offer`. The error says why the
    /// compositor cannot do it (typically a missing data-control protocol).
    pub(super) fn acquire(offer: &FileOffer) -> Result<Self, String> {
        let sources: Vec<MimeSource> = offer
            .targets()
            .into_iter()
            .map(|(mime, data)| MimeSource {
                source: Source::Bytes(data.into()),
                mime_type: MimeType::Specific(mime.to_owned()),
            })
            .collect();

        // The setup (bind protocols, set the selection) runs on the serving
        // thread, because the prepared copy is bound to the thread that serves
        // it; its outcome comes back over a channel so the caller still gets a
        // synchronous answer.
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("clipboard-wayland".into())
            .spawn(move || {
                let mut options = Options::new();
                options
                    .foreground(true)
                    // `text/uri-list` is a text/* type, which the library would
                    // otherwise also publish as plain text. A pasted file list
                    // must not turn into text in a terminal or editor.
                    .omit_additional_text_mime_types(true);
                match options.prepare_copy_multi(sources) {
                    Ok(prepared) => {
                        let _ = ready_tx.send(Ok(()));
                        if let Err(e) = prepared.serve() {
                            tracing::debug!("clipboard Wayland serving ended: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                    }
                }
            })
            .map_err(|e| format!("cannot start the clipboard thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(reason)
            }
            Err(_) => Err("the clipboard thread died during setup".into()),
        }
    }

    /// Whether our file list is still what a paste would get.
    pub(super) fn is_owned(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for WaylandOwner {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // A finished thread means another client already replaced the selection;
        // clearing now would erase *their* copy.
        if thread.is_finished() {
            let _ = thread.join();
            return;
        }
        // The serving loop only ends when the compositor cancels our source, and
        // clearing the selection is what makes it do that. If the clear fails the
        // thread would never return, so it is left to end with the process
        // rather than joined.
        match clear(ClipboardType::Regular, Seat::All) {
            Ok(()) => {
                let _ = thread.join();
            }
            Err(e) => tracing::warn!("could not release the Wayland clipboard: {e}"),
        }
    }
}
