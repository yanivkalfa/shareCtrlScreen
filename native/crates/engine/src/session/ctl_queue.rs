//! The outbound side of the reliable `ctl` data channel: one ordered queue in
//! front of the SCTP send buffer.
//!
//! str0m's `Channel::write` refuses (`Ok(false)`) a message that does not fit
//! in what is left of its 128 KiB send buffer, shared by every channel and
//! freed only as the peer acknowledges. Before this queue the driver ignored
//! that answer, so anything sent while the buffer was full vanished from a
//! channel the rest of the engine treats as reliable — the cursor image parts
//! of task D9 (about 33 KiB each, four for a 128 px image) hit it on an idle
//! link. Now nothing is dropped: a refused message stays at the head of the
//! queue, in order, and is retried on every driver pass until the peer has
//! acknowledged enough. The queue also holds everything sent before the
//! channel opens (the initial `Perm` among them).
//!
//! Memory is bounded by liveness, not by a cap: while the peer acknowledges
//! nothing, pongs stop too and the driver ends the session within its grace
//! period. A cap would bring back silent loss.
//!
//! ## References
//! * str0m 0.21 `Channel::write` ("Returns true or false whether the buffer was
//!   accepted"), `sctp::MAX_BUFFERED_ACROSS_STREAMS` (128 KiB) and
//!   `RtcSctp::available`.

use std::collections::VecDeque;
use std::fmt::Display;

/// str0m's send buffer across all channels. A message larger than this can
/// never be accepted, so it is dropped (with a warning) instead of blocking
/// every message behind it for the rest of the session.
pub(super) const SCTP_SEND_BUFFER: usize = 128 * 1024;

#[derive(Debug, Default)]
pub(super) struct CtlQueue {
    open: bool,
    pending: VecDeque<Vec<u8>>,
}

impl CtlQueue {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// The channel opened: from now on [`flush`](Self::flush) writes.
    pub(super) fn open(&mut self) {
        self.open = true;
    }

    /// Messages waiting (not yet accepted by the transport).
    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Queue `bytes` behind everything still waiting, then send what fits.
    /// `write` is the transport's send: `Ok(true)` accepted, `Ok(false)` no
    /// room yet, `Err` refused for good.
    pub(super) fn send<E: Display>(
        &mut self,
        bytes: Vec<u8>,
        write: impl FnMut(&[u8]) -> Result<bool, E>,
    ) {
        if bytes.len() > SCTP_SEND_BUFFER {
            tracing::warn!(
                "ctl: dropping a {}-byte message, larger than the {SCTP_SEND_BUFFER}-byte send buffer",
                bytes.len()
            );
            return;
        }
        self.pending.push_back(bytes);
        self.flush(write);
    }

    /// Write waiting messages in order until the transport has no room. A
    /// no-op before the channel opens.
    pub(super) fn flush<E: Display>(&mut self, mut write: impl FnMut(&[u8]) -> Result<bool, E>) {
        if !self.open {
            return;
        }
        while let Some(front) = self.pending.front() {
            match write(front) {
                Ok(true) => {
                    self.pending.pop_front();
                }
                // No room until the peer acknowledges more: keep it at the
                // head (order matters: image parts, perm changes, input).
                Ok(false) => break,
                // Refused for good (channel closed, message over the peer's
                // size limit). Retrying would wedge the queue forever.
                Err(e) => {
                    tracing::warn!("ctl: message dropped by the transport: {e}");
                    self.pending.pop_front();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport with `room` bytes of send buffer.
    struct Fake {
        room: usize,
        sent: Vec<Vec<u8>>,
    }

    impl Fake {
        fn write(&mut self, b: &[u8]) -> Result<bool, String> {
            if b.first() == Some(&0xff) {
                return Err("refused".into());
            }
            if b.len() > self.room {
                return Ok(false);
            }
            self.room -= b.len();
            self.sent.push(b.to_vec());
            Ok(true)
        }
    }

    #[test]
    fn nothing_leaves_before_the_channel_opens_and_order_is_kept() {
        let mut q = CtlQueue::default();
        let mut t = Fake {
            room: 1000,
            sent: Vec::new(),
        };
        q.send(vec![1], |b| t.write(b));
        q.send(vec![2], |b| t.write(b));
        assert!(t.sent.is_empty());
        q.open();
        q.flush(|b| t.write(b));
        assert_eq!(t.sent, vec![vec![1], vec![2]]);
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn a_full_buffer_delays_instead_of_dropping() {
        // Four 33 KiB image parts into 128 KiB: the fourth does not fit until
        // the peer acknowledges; it and everything after it wait, in order.
        let part = |n: u8| vec![n; 33 * 1024];
        let mut q = CtlQueue::default();
        q.open();
        let mut t = Fake {
            room: SCTP_SEND_BUFFER,
            sent: Vec::new(),
        };
        for n in 1..=4 {
            q.send(part(n), |b| t.write(b));
        }
        q.send(vec![9], |b| t.write(b)); // small, but must not overtake
        assert_eq!(t.sent.len(), 3);
        assert_eq!(q.pending(), 2);
        // The peer acknowledged: room again.
        t.room = SCTP_SEND_BUFFER;
        q.flush(|b| t.write(b));
        assert_eq!(q.pending(), 0);
        let firsts: Vec<u8> = t.sent.iter().map(|m| m[0]).collect();
        assert_eq!(firsts, vec![1, 2, 3, 4, 9]);
    }

    #[test]
    fn hopeless_messages_are_dropped_not_wedged() {
        let mut q = CtlQueue::default();
        q.open();
        let mut t = Fake {
            room: SCTP_SEND_BUFFER * 2,
            sent: Vec::new(),
        };
        q.send(vec![0; SCTP_SEND_BUFFER + 1], |b| t.write(b));
        q.send(vec![0xff], |b| t.write(b));
        q.send(vec![7], |b| t.write(b));
        assert_eq!(t.sent, vec![vec![7]]);
        assert_eq!(q.pending(), 0);
    }
}
