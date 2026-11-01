//! Clipboard text sync, both directions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use platform_api::{Clipboard, Platform, MAX_TEXT_BYTES};
use protocol::ControlMsg;

use super::serialize;

/// How often the local clipboard is polled for changes.
const POLL: Duration = Duration::from_millis(300);

/// Mirror clipboard TEXT between the two machines. The clipboard is created on
/// this thread (some backends are thread-bound) and polled via its change
/// counter. Applying a value received from the peer bumps that same counter, so
/// the resulting value is remembered and skipped — otherwise each side would
/// echo the other forever.
pub(super) fn clipboard_loop(
    platform: Arc<dyn Platform>,
    ctl_tx: Sender<Vec<u8>>,
    inbound: Receiver<String>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let clipboard = platform.create_clipboard();
    let mut sync = ClipboardSync::new(clipboard.as_ref());
    while !stop.load(Ordering::SeqCst) {
        sync.step(
            clipboard.as_ref(),
            &ctl_tx,
            &inbound,
            enabled.load(Ordering::SeqCst),
        );
        std::thread::sleep(POLL);
    }
}

/// The echo-guarded sync state machine, separate from the thread so it can be
/// tested deterministically.
struct ClipboardSync {
    last_count: u64,
    last_text: Option<String>,
}

impl ClipboardSync {
    fn new(clipboard: &dyn Clipboard) -> Self {
        // Whatever is on the clipboard when the session starts is not sent: only
        // copies made during the session are.
        Self {
            last_count: clipboard.change_count(),
            last_text: None,
        }
    }

    fn step(
        &mut self,
        clipboard: &dyn Clipboard,
        ctl_tx: &Sender<Vec<u8>>,
        inbound: &Receiver<String>,
        enabled: bool,
    ) {
        // Apply anything the peer sent first, so the poll below sees the
        // resulting change count and treats it as ours. Text received while sync
        // is disabled is discarded, not queued.
        while let Ok(text) = inbound.try_recv() {
            if !enabled || self.last_text.as_deref() == Some(text.as_str()) {
                continue;
            }
            if let Some(count) = clipboard.set_text(&text) {
                self.last_count = count;
                self.last_text = Some(text);
            }
        }

        if !enabled {
            return;
        }
        let count = clipboard.change_count();
        if count == self.last_count {
            return;
        }
        self.last_count = count;
        if let Some(text) = clipboard.get_text() {
            if self.last_text.as_deref() != Some(text.as_str()) && text.len() <= MAX_TEXT_BYTES {
                self.last_text = Some(text.clone());
                let _ = ctl_tx.send(serialize(&ControlMsg::Clipboard { text }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockClipboard;
    use std::sync::mpsc;

    fn sent(rx: &Receiver<Vec<u8>>) -> Vec<String> {
        rx.try_iter()
            .map(
                |b| match serde_json::from_slice::<ControlMsg>(&b).unwrap() {
                    ControlMsg::Clipboard { text } => text,
                    other => panic!("unexpected {other:?}"),
                },
            )
            .collect()
    }

    #[test]
    fn local_copies_are_sent_once_and_peer_text_is_never_echoed() {
        let clip = MockClipboard::default();
        clip.user_copies("before the session");
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (peer_tx, peer_rx) = mpsc::channel();
        let mut s = ClipboardSync::new(&clip);

        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert!(
            sent(&ctl_rx).is_empty(),
            "pre-session clipboard is not sent"
        );

        clip.user_copies("hello");
        s.step(&clip, &ctl_tx, &peer_rx, true);
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(sent(&ctl_rx), ["hello"]);

        peer_tx.send("from peer".into()).unwrap();
        s.step(&clip, &ctl_tx, &peer_rx, true);
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(clip.get_text().as_deref(), Some("from peer"));
        assert!(
            sent(&ctl_rx).is_empty(),
            "applied peer text must not bounce back"
        );
    }

    #[test]
    fn disabled_sync_neither_sends_nor_applies() {
        let clip = MockClipboard::default();
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (peer_tx, peer_rx) = mpsc::channel();
        let mut s = ClipboardSync::new(&clip);
        clip.user_copies("secret");
        peer_tx.send("injected".into()).unwrap();
        s.step(&clip, &ctl_tx, &peer_rx, false);
        assert!(sent(&ctl_rx).is_empty());
        assert_eq!(clip.get_text().as_deref(), Some("secret"));
        // Re-enabling does not replay text that arrived while disabled.
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(clip.get_text().as_deref(), Some("secret"));
    }
}
