//! The capture's event thread and the hand-off it feeds.
//!
//! x11rb has no "wait for an event with a timeout", and `acquire` must return
//! `Ok(None)` after its timeout (trait invariant 2). So a dedicated thread
//! blocks in `wait_for_event` on the shared connection and raises flags in a
//! [`Pump`]; `acquire` waits on the pump's condition variable with a timeout.
//! `RustConnection` is designed for exactly this split: one thread reads
//! events while another sends requests and reads their replies (x11rb's
//! `rust_connection` module docs).
//!
//! The flags are levels, not a queue: what `acquire` needs is "something was
//! drawn since I last looked", not each event, so a burst of notifications
//! costs one wake-up — the same newest-wins idea as the macOS frame slot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::damage::Damage;
use x11rb::protocol::xproto::Window;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// What happened since the capture last looked.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Signals {
    /// Our Damage object went from empty to non-empty.
    pub damaged: bool,
    /// The displayed cursor image changed (XFixes `CursorNotify`).
    pub cursor: bool,
    /// The screen configuration may have changed (RandR notify or a root
    /// `ConfigureNotify`); the capture re-reads the geometry to find out.
    pub screen: bool,
    /// The connection failed (X server gone); the reason, for the log.
    pub closed: Option<String>,
}

impl Signals {
    fn any(&self) -> bool {
        self.damaged || self.cursor || self.screen || self.closed.is_some()
    }
}

/// Level-triggered flags between the event thread and `acquire`.
#[derive(Default)]
pub(super) struct Pump {
    state: Mutex<Signals>,
    ready: Condvar,
}

impl Pump {
    // Poisoning only means a thread panicked while holding the lock; every
    // update leaves the flags consistent, so keep going with them.
    fn lock(&self) -> MutexGuard<'_, Signals> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn raise(&self, set: impl FnOnce(&mut Signals)) {
        set(&mut self.lock());
        self.ready.notify_all();
    }

    /// Wait up to `timeout` for any flag, then take (and clear) all of them.
    /// Returns the empty set on timeout.
    pub(super) fn wait(&self, timeout: Duration) -> Signals {
        let guard = self.lock();
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |s| !s.any())
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }
}

/// Everything the thread needs to classify events.
pub(super) struct Watch {
    pub conn: Arc<RustConnection>,
    pub root: Window,
    pub damage: Damage,
    pub pump: Arc<Pump>,
    pub shutdown: Arc<AtomicBool>,
}

/// The event thread's body: runs until the connection fails or `shutdown` is
/// set (the owner then wakes it with a message to its own window).
pub(super) fn run(w: Watch) {
    loop {
        let event = w.conn.wait_for_event();
        if w.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match event {
            Ok(Event::DamageNotify(e)) if e.damage == w.damage => {
                w.pump.raise(|s| s.damaged = true)
            }
            Ok(Event::XfixesCursorNotify(_)) => w.pump.raise(|s| s.cursor = true),
            Ok(Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_)) => {
                w.pump.raise(|s| s.screen = true)
            }
            Ok(Event::ConfigureNotify(e)) if e.window == w.root => {
                w.pump.raise(|s| s.screen = true)
            }
            // An unchecked request failed. Every request whose failure matters
            // is checked or has a reply (its error is delivered there), so this
            // is diagnostic only.
            Ok(Event::Error(e)) => tracing::debug!("x11 capture: asynchronous X error {e:?}"),
            Ok(_) => {}
            Err(e) => {
                let reason = e.to_string();
                w.pump.raise(|s| s.closed = Some(reason));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_times_out_empty() {
        let pump = Pump::default();
        let t = Instant::now();
        assert_eq!(pump.wait(Duration::from_millis(30)), Signals::default());
        assert!(t.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn flags_coalesce_and_are_cleared_by_wait() {
        let pump = Pump::default();
        pump.raise(|s| s.damaged = true);
        pump.raise(|s| s.damaged = true);
        pump.raise(|s| s.cursor = true);
        let got = pump.wait(Duration::from_secs(5));
        assert!(got.damaged && got.cursor && !got.screen && got.closed.is_none());
        assert_eq!(pump.wait(Duration::ZERO), Signals::default());
    }

    #[test]
    fn a_raise_from_another_thread_wakes_the_waiter() {
        let pump = Arc::new(Pump::default());
        let p = pump.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            p.raise(|s| s.closed = Some("gone".into()));
        });
        let got = pump.wait(Duration::from_secs(10));
        assert_eq!(got.closed.as_deref(), Some("gone"));
        t.join().expect("raiser thread");
    }
}
