//! A depth-1, newest-wins hand-off between the ScreenCaptureKit delivery queue
//! (producer) and the host media thread (consumer, `ScreenCapture::acquire`).
//!
//! Why depth 1 and newest-wins: the host encodes the *current* screen, never a
//! backlog. A frame that arrives while the previous one is still waiting is
//! dropped at once — which also hands its surface straight back to
//! ScreenCaptureKit's pool, so a slow encoder can never hold enough surfaces to
//! stall the stream (plans/04-TARGET-ARCHITECTURE.md §7).
//!
//! OS-independent so its semantics are unit-tested on every CI job.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Why the stream behind a slot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// Display slept / was reconfigured / the system stopped the stream:
    /// recreating the stream recovers.
    Lost,
    /// Screen Recording was refused or revoked: no amount of retrying helps.
    PermissionDenied,
}

/// Result of [`FrameSlot::wait`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wait<T> {
    Frame(T),
    Timeout,
    Stopped(StopReason),
}

/// Delivery counters, logged when a stream ends (the "superseded" figure is how
/// many frames the encoder was too slow for).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SlotStats {
    pub delivered: u64,
    pub superseded: u64,
}

struct State<T> {
    pending: Option<T>,
    stopped: Option<StopReason>,
    stats: SlotStats,
}

pub(crate) struct FrameSlot<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

impl<T> FrameSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                pending: None,
                stopped: None,
                stats: SlotStats::default(),
            }),
            ready: Condvar::new(),
        }
    }

    // Poisoning only means a thread panicked while holding the lock; every
    // update below leaves the state consistent, so the data is still valid and
    // the capture keeps working instead of cascading the panic.
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Offer a new frame, replacing (and dropping) any frame still waiting.
    /// Ignored once the slot is stopped: the stream is dead and the consumer is
    /// about to recreate it.
    pub(crate) fn put(&self, frame: T) {
        let superseded = {
            let mut s = self.lock();
            if s.stopped.is_some() {
                return;
            }
            s.stats.delivered += 1;
            let old = s.pending.replace(frame);
            if old.is_some() {
                s.stats.superseded += 1;
            }
            old
        };
        // Drop the superseded frame outside the lock: releasing a pixel buffer
        // calls into CoreVideo, which must not extend the critical section.
        drop(superseded);
        self.ready.notify_one();
    }

    /// Mark the stream as ended. The first reason wins (a permission failure
    /// is often followed by a generic stop for the same stream).
    pub(crate) fn stop(&self, reason: StopReason) {
        let pending = {
            let mut s = self.lock();
            if s.stopped.is_none() {
                s.stopped = Some(reason);
            }
            s.pending.take()
        };
        drop(pending);
        self.ready.notify_all();
    }

    /// Whether a frame is waiting to be taken.
    pub(crate) fn has_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    pub(crate) fn stats(&self) -> SlotStats {
        self.lock().stats
    }

    /// Take the waiting frame, waiting up to `timeout` for one. A stopped slot
    /// reports `Stopped` even if a frame was left behind: the session is gone
    /// and its last frame is not worth encoding.
    pub(crate) fn wait(&self, timeout: Duration) -> Wait<T> {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = self.lock();
        loop {
            if let Some(reason) = s.stopped {
                return Wait::Stopped(reason);
            }
            if let Some(frame) = s.pending.take() {
                return Wait::Frame(frame);
            }
            // `None` deadline = a timeout too large to represent: wait forever.
            let left = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => Some(left),
                    _ => return Wait::Timeout,
                },
                None => None,
            };
            s = match left {
                Some(left) => {
                    self.ready
                        .wait_timeout(s, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self.ready.wait(s).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn empty_slot_times_out() {
        let slot = FrameSlot::<u32>::new();
        let t = Instant::now();
        assert_eq!(slot.wait(Duration::from_millis(20)), Wait::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(15));
        assert_eq!(slot.wait(Duration::ZERO), Wait::Timeout);
    }

    #[test]
    fn newest_frame_wins_and_older_ones_are_dropped_at_once() {
        let slot = FrameSlot::new();
        let first = Arc::new(1);
        slot.put(Arc::clone(&first));
        slot.put(Arc::new(2));
        // The superseded frame was released by `put`, not kept until `wait`:
        // that is what returns its surface to ScreenCaptureKit promptly.
        assert_eq!(Arc::strong_count(&first), 1);
        assert_eq!(slot.wait(Duration::ZERO), Wait::Frame(Arc::new(2)));
        assert_eq!(slot.wait(Duration::ZERO), Wait::Timeout);
        assert_eq!(
            slot.stats(),
            SlotStats {
                delivered: 2,
                superseded: 1
            }
        );
    }

    #[test]
    fn a_frame_from_another_thread_wakes_the_waiter() {
        let slot = Arc::new(FrameSlot::new());
        let producer = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            producer.put(7u32);
        });
        assert_eq!(slot.wait(Duration::from_secs(5)), Wait::Frame(7));
        t.join().unwrap();
    }

    #[test]
    fn stop_wakes_the_waiter_drops_the_pending_frame_and_keeps_the_first_reason() {
        let slot = Arc::new(FrameSlot::new());
        let pending = Arc::new(0u8);
        slot.put(Arc::clone(&pending));
        slot.stop(StopReason::PermissionDenied);
        slot.stop(StopReason::Lost);
        assert_eq!(Arc::strong_count(&pending), 1);
        assert_eq!(
            slot.wait(Duration::ZERO),
            Wait::Stopped(StopReason::PermissionDenied)
        );
        // Late frames from a dying stream are refused.
        slot.put(Arc::new(1));
        assert!(!slot.has_pending());

        let slot = Arc::new(FrameSlot::<u8>::new());
        let stopper = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            stopper.stop(StopReason::Lost);
        });
        assert_eq!(
            slot.wait(Duration::from_secs(5)),
            Wait::Stopped(StopReason::Lost)
        );
        t.join().unwrap();
    }

    #[test]
    fn has_pending_reflects_the_slot() {
        let slot = FrameSlot::new();
        assert!(!slot.has_pending());
        slot.put(1u8);
        assert!(slot.has_pending());
        let _ = slot.wait(Duration::ZERO);
        assert!(!slot.has_pending());
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let slot = FrameSlot::new();
        slot.put(3u8);
        assert_eq!(slot.wait(Duration::MAX), Wait::Frame(3));
    }
}
