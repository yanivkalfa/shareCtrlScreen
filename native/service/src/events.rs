//! What the service control manager tells the service, as plain values, and
//! the hand-off from the SCM's control handler to the session loop (plan 10a
//! §3.7). Free of Win32 calls so it is unit-tested on every OS: `main.rs` only
//! copies the raw numbers out of `HandlerEx` into [`Control::from_code`],
//! [`SessionEvent::decode`] and the [`Inbox`].
//!
//! The handler runs on the SCM's dispatcher thread, the loop on its own. The
//! handler must not block or take a lock, so the [`Inbox`] is a fixed ring of
//! atomics plus `Thread::unpark`: posting never waits, never allocates and
//! never logs. All work (deciding, launching, logging) stays on the loop's
//! thread, which [`Inbox::wait`]s with a timeout, so the 2 s poll stays as the
//! fallback for anything the SCM does not tell the service.
//!
//! ## References
//! - HandlerEx: <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nc-winsvc-lphandler_function_ex>
//!   — `SERVICE_CONTROL_STOP` 0x1, `SERVICE_CONTROL_INTERROGATE` 0x4,
//!   `SERVICE_CONTROL_SHUTDOWN` 0x5, `SERVICE_CONTROL_SESSIONCHANGE` 0xE;
//!   for a session change "this parameter can be one of the values specified
//!   in the wParam parameter of the WM_WTSSESSION_CHANGE message" and
//!   lpEventData "is a pointer to a WTSSESSION_NOTIFICATION structure";
//!   "return NO_ERROR for SERVICE_CONTROL_INTERROGATE even if your service
//!   does not handle it", ERROR_CALL_NOT_IMPLEMENTED for an unhandled control;
//!   "a control handler should avoid operations that might block, such as
//!   taking a lock"; "a service will only be notified of a user logon if it is
//!   fully loaded before the logon attempt is made" (why the poll stays).
//! - Writing a control handler: <https://learn.microsoft.com/en-us/windows/win32/services/writing-a-control-handler-function>
//!   — "When a Handler function is called by the dispatcher thread": one
//!   producer thread, which is what lets [`Inbox`] be single-producer.
//! - WM_WTSSESSION_CHANGE: <https://learn.microsoft.com/en-us/windows/win32/termserv/wm-wtssession-change>
//!   — the event values 0x1–0xB and 0xF ([`SessionChange`]).
//! - WTSSESSION_NOTIFICATION: <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-wtssession_notification>
//!   — `{ DWORD cbSize; DWORD dwSessionId; }`, cbSize "Size, in bytes, of
//!   this structure".
//! - System error codes: <https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499->
//!   — `ERROR_CALL_NOT_IMPLEMENTED` is 120 (0x78); `ERROR_SUCCESS`
//!   (`NO_ERROR`) is 0.
//! - `std::thread::park_timeout` / `Thread::unpark`: <https://doc.rust-lang.org/std/thread/fn.park.html>
//!   — an `unpark` before the `park` makes the token available, so that
//!   `park` returns at once (no lost wake-up); calls to `unpark`
//!   synchronize-with calls to `park`; `park` may also return spuriously.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::thread::Thread;
use std::time::Duration;

/// `NO_ERROR`: what HandlerEx returns for a control it handled.
pub const NO_ERROR: u32 = 0;
/// `ERROR_CALL_NOT_IMPLEMENTED`: what HandlerEx returns for a control it does
/// not handle.
pub const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

pub const CONTROL_STOP: u32 = 0x1;
pub const CONTROL_INTERROGATE: u32 = 0x4;
pub const CONTROL_SHUTDOWN: u32 = 0x5;
pub const CONTROL_SESSIONCHANGE: u32 = 0xE;

/// The size of a `WTSSESSION_NOTIFICATION` (two DWORDs).
pub const NOTIFICATION_SIZE: u32 = 8;

/// A control code the SCM sent to HandlerEx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Stop,
    Shutdown,
    Interrogate,
    SessionChange,
    /// Any control the service does not accept (it is not sent one unless a
    /// future accept mask asks for it).
    Other(u32),
}

impl Control {
    pub fn from_code(code: u32) -> Self {
        match code {
            CONTROL_STOP => Self::Stop,
            CONTROL_SHUTDOWN => Self::Shutdown,
            CONTROL_INTERROGATE => Self::Interrogate,
            CONTROL_SESSIONCHANGE => Self::SessionChange,
            other => Self::Other(other),
        }
    }

    /// What HandlerEx returns for this control. INTERROGATE gets `NO_ERROR`
    /// although the service does nothing for it, as the docs require.
    pub fn reply(self) -> u32 {
        match self {
            Self::Stop | Self::Shutdown | Self::Interrogate | Self::SessionChange => NO_ERROR,
            Self::Other(_) => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }
}

/// A `WM_WTSSESSION_CHANGE` event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    ConsoleConnect,
    ConsoleDisconnect,
    RemoteConnect,
    RemoteDisconnect,
    Logon,
    Logoff,
    Lock,
    Unlock,
    RemoteControl,
    /// Documented as reserved.
    Create,
    /// Documented as reserved.
    Terminate,
    DesktopReady,
    /// A value this build does not know; kept for the log.
    Other(u32),
}

impl SessionChange {
    pub fn from_code(code: u32) -> Self {
        match code {
            0x1 => Self::ConsoleConnect,
            0x2 => Self::ConsoleDisconnect,
            0x3 => Self::RemoteConnect,
            0x4 => Self::RemoteDisconnect,
            0x5 => Self::Logon,
            0x6 => Self::Logoff,
            0x7 => Self::Lock,
            0x8 => Self::Unlock,
            0x9 => Self::RemoteControl,
            0xA => Self::Create,
            0xB => Self::Terminate,
            0xF => Self::DesktopReady,
            other => Self::Other(other),
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Self::ConsoleConnect => 0x1,
            Self::ConsoleDisconnect => 0x2,
            Self::RemoteConnect => 0x3,
            Self::RemoteDisconnect => 0x4,
            Self::Logon => 0x5,
            Self::Logoff => 0x6,
            Self::Lock => 0x7,
            Self::Unlock => 0x8,
            Self::RemoteControl => 0x9,
            Self::Create => 0xA,
            Self::Terminate => 0xB,
            Self::DesktopReady => 0xF,
            Self::Other(code) => code,
        }
    }

    /// The name used in `sharectrl-service.log`.
    pub fn name(self) -> &'static str {
        match self {
            Self::ConsoleConnect => "console connect",
            Self::ConsoleDisconnect => "console disconnect",
            Self::RemoteConnect => "remote connect",
            Self::RemoteDisconnect => "remote disconnect",
            Self::Logon => "logon",
            Self::Logoff => "logoff",
            Self::Lock => "lock",
            Self::Unlock => "unlock",
            Self::RemoteControl => "remote control",
            Self::Create => "create",
            Self::Terminate => "terminate",
            Self::DesktopReady => "desktop ready",
            Self::Other(_) => "unknown",
        }
    }
}

/// One session-change notification: what happened, to which session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionEvent {
    pub change: SessionChange,
    pub session: u32,
}

impl SessionEvent {
    /// From HandlerEx's `dwEventType` and the notification's `dwSessionId`.
    pub fn decode(event_type: u32, session: u32) -> Self {
        Self {
            change: SessionChange::from_code(event_type),
            session,
        }
    }

    /// Whether a notification whose `cbSize` is `cb_size` holds the session
    /// id. The handler reads `dwSessionId` only when this is true, so a short
    /// or garbled structure is never read past its stated end.
    pub fn notification_complete(cb_size: u32) -> bool {
        cb_size >= NOTIFICATION_SIZE
    }

    fn pack(self) -> u64 {
        (u64::from(self.change.code()) << 32) | u64::from(self.session)
    }

    fn unpack(v: u64) -> Self {
        // Truncation is the point: the two halves are the two u32s `pack` put in.
        Self::decode((v >> 32) as u32, v as u32)
    }
}

/// How many events the inbox holds before it counts the rest as lost. The
/// loop drains it as soon as it is woken, so a burst this long between two
/// wake-ups does not happen in practice; a loss is logged and the poll still
/// covers it.
pub const INBOX_CAPACITY: usize = 64;

/// What [`Inbox::drain`] returns.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// The recorded events, oldest first.
    pub events: Vec<SessionEvent>,
    /// Events that could not be recorded since the last drain (the ring was
    /// full, or the notification was unreadable).
    pub lost: usize,
}

/// The hand-off from the control handler to the loop: a single-producer,
/// single-consumer ring of session events, a stop flag, and the loop's thread
/// to wake.
///
/// INVARIANTS:
/// * Only one thread posts (the SCM's dispatcher thread calls HandlerEx, see
///   the references) and only the loop's thread drains. The ring relies on
///   it: `posted` is written by the producer alone, `taken` by the consumer
///   alone.
/// * The loop's thread is registered with [`set_waiter`](Self::set_waiter)
///   BEFORE the handler is registered with the SCM, so every post after that
///   unparks it. A post between the loop's last drain and its park leaves the
///   park token set, so that park returns at once: no wake-up is lost.
pub struct Inbox {
    slots: [AtomicU64; INBOX_CAPACITY],
    /// Events posted so far (wrapping). Written only by the producer.
    posted: AtomicUsize,
    /// Events drained so far (wrapping). Written only by the consumer.
    taken: AtomicUsize,
    lost: AtomicUsize,
    stop: AtomicBool,
    waiter: OnceLock<Thread>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; INBOX_CAPACITY],
            posted: AtomicUsize::new(0),
            taken: AtomicUsize::new(0),
            lost: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            waiter: OnceLock::new(),
        }
    }

    /// Make the calling thread the one [`wait`](Self::wait) parks and posts
    /// wake. Returns `false` if another thread already is (the first one
    /// stays; a `wait` on any other thread then only sleeps out its timeout).
    pub fn set_waiter(&self) -> bool {
        let me = std::thread::current();
        let id = me.id();
        self.waiter.set(me).is_ok() || self.waiter.get().map(Thread::id) == Some(id)
    }

    /// Record `event` and wake the loop. Never blocks, allocates or logs:
    /// safe to call from the control handler.
    pub fn post(&self, event: SessionEvent) {
        // Relaxed: only this (producer) thread writes `posted`.
        let posted = self.posted.load(Ordering::Relaxed);
        // Acquire: the consumer has finished reading every slot below `taken`.
        let taken = self.taken.load(Ordering::Acquire);
        if posted.wrapping_sub(taken) >= INBOX_CAPACITY {
            self.lost.fetch_add(1, Ordering::Relaxed);
        } else {
            self.slots[posted % INBOX_CAPACITY].store(event.pack(), Ordering::Relaxed);
            // Release: the slot write above is visible to a consumer that
            // sees the new count.
            self.posted.store(posted.wrapping_add(1), Ordering::Release);
        }
        self.wake();
    }

    /// Count a notification that could not be read (no or short event data)
    /// and wake the loop, which logs it.
    pub fn note_lost(&self) {
        self.lost.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }

    /// Ask the loop to stop and wake it at once.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Take every recorded event, oldest first. Loop thread only.
    pub fn drain(&self) -> Drained {
        // Relaxed: only this (consumer) thread writes `taken`.
        let taken = self.taken.load(Ordering::Relaxed);
        // Acquire: pairs with the producer's Release, so the slots are written.
        let posted = self.posted.load(Ordering::Acquire);
        let count = posted.wrapping_sub(taken);
        let events = (0..count)
            .map(|k| {
                let slot = taken.wrapping_add(k) % INBOX_CAPACITY;
                SessionEvent::unpack(self.slots[slot].load(Ordering::Relaxed))
            })
            .collect();
        // Release: the producer may reuse these slots only after the reads.
        self.taken.store(posted, Ordering::Release);
        Drained {
            events,
            lost: self.lost.swap(0, Ordering::Relaxed),
        }
    }

    /// Wait until something is posted, a stop is requested, or `timeout`
    /// passes, whichever is first; may also return early (a spurious wake-up
    /// only costs one extra poll, which is idempotent). Returns at once when a
    /// stop is already requested. On a thread other than the waiter it only
    /// sleeps: correct, but without the early wake-up.
    pub fn wait(&self, timeout: Duration) {
        if self.stop_requested() {
            return;
        }
        let me = std::thread::current().id();
        if self.waiter.get().map(Thread::id) == Some(me) {
            std::thread::park_timeout(timeout);
        } else {
            std::thread::sleep(timeout);
        }
    }

    fn wake(&self) {
        if let Some(thread) = self.waiter.get() {
            thread.unpark();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn ev(change: SessionChange, session: u32) -> SessionEvent {
        SessionEvent { change, session }
    }

    #[test]
    fn control_codes_and_replies() {
        assert_eq!(Control::from_code(0x1), Control::Stop);
        assert_eq!(Control::from_code(0x5), Control::Shutdown);
        assert_eq!(Control::from_code(0x4), Control::Interrogate);
        assert_eq!(Control::from_code(0xE), Control::SessionChange);
        assert_eq!(Control::from_code(0x2), Control::Other(0x2)); // PAUSE
        for handled in [0x1, 0x4, 0x5, 0xE] {
            assert_eq!(Control::from_code(handled).reply(), NO_ERROR);
        }
        assert_eq!(Control::Other(0x2).reply(), ERROR_CALL_NOT_IMPLEMENTED);
        assert_eq!(Control::Other(0x0F).reply(), ERROR_CALL_NOT_IMPLEMENTED); // PRESHUTDOWN
    }

    #[test]
    fn session_change_codes_round_trip() {
        let documented = [0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xA, 0xB, 0xF];
        for code in documented {
            let change = SessionChange::from_code(code);
            assert!(!matches!(change, SessionChange::Other(_)), "{code:#x}");
            assert_eq!(change.code(), code);
        }
        assert_eq!(SessionChange::from_code(0x5), SessionChange::Logon);
        assert_eq!(SessionChange::from_code(0x6), SessionChange::Logoff);
        assert_eq!(SessionChange::from_code(0x7), SessionChange::Lock);
        assert_eq!(SessionChange::from_code(0x8), SessionChange::Unlock);
        assert_eq!(SessionChange::from_code(0xC), SessionChange::Other(0xC));
        assert_eq!(SessionChange::Other(0xC).code(), 0xC);
    }

    #[test]
    fn a_short_notification_is_not_read() {
        assert!(SessionEvent::notification_complete(8));
        assert!(SessionEvent::notification_complete(12));
        assert!(!SessionEvent::notification_complete(4));
        assert!(!SessionEvent::notification_complete(0));
    }

    #[test]
    fn events_drain_in_order_with_their_session() {
        let inbox = Inbox::new();
        let sent = [
            ev(SessionChange::Lock, 1),
            ev(SessionChange::Unlock, 1),
            ev(SessionChange::Logon, u32::MAX),
            ev(SessionChange::Other(0x77), 3),
        ];
        for e in sent {
            inbox.post(e);
        }
        let drained = inbox.drain();
        assert_eq!(drained.events, sent);
        assert_eq!(drained.lost, 0);
        assert_eq!(inbox.drain(), Drained::default());
    }

    #[test]
    fn a_full_inbox_counts_the_rest_as_lost_and_recovers() {
        let inbox = Inbox::new();
        for i in 0..(INBOX_CAPACITY as u32 + 5) {
            inbox.post(ev(SessionChange::Lock, i));
        }
        inbox.note_lost();
        let drained = inbox.drain();
        assert_eq!(drained.events.len(), INBOX_CAPACITY);
        assert_eq!(drained.events[0].session, 0);
        assert_eq!(drained.lost, 6);
        // The ring wraps around cleanly after a drain.
        for round in 0..3u32 {
            for i in 0..40 {
                inbox.post(ev(SessionChange::Unlock, round * 100 + i));
            }
            let d = inbox.drain();
            assert_eq!(d.lost, 0);
            assert_eq!(d.events.len(), 40);
            assert_eq!(d.events[39].session, round * 100 + 39);
        }
    }

    #[test]
    fn a_post_before_the_wait_is_not_lost() {
        let inbox = Inbox::new();
        assert!(inbox.set_waiter());
        assert!(inbox.set_waiter(), "the same thread again is fine");
        inbox.post(ev(SessionChange::Logon, 1));
        let t0 = Instant::now();
        inbox.wait(Duration::from_secs(30));
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert_eq!(inbox.drain().events, [ev(SessionChange::Logon, 1)]);
    }

    #[test]
    fn a_post_from_another_thread_wakes_the_waiter() {
        let inbox = Arc::new(Inbox::new());
        assert!(inbox.set_waiter());
        let producer = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                inbox.post(ev(SessionChange::Logoff, 2));
            })
        };
        let t0 = Instant::now();
        let mut got = Vec::new();
        // park may return spuriously: wait until the event is there.
        while got.is_empty() && t0.elapsed() < Duration::from_secs(10) {
            inbox.wait(Duration::from_secs(30));
            got = inbox.drain().events;
        }
        assert!(producer.join().is_ok());
        assert_eq!(got, [ev(SessionChange::Logoff, 2)]);
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "woken, not timed out"
        );
    }

    #[test]
    fn a_busy_producer_never_reorders_or_miscounts() {
        // The handler's thread posts while the loop's thread drains. Whatever
        // the interleaving, the events that arrive are in posting order and
        // every post is either delivered or counted as lost.
        const POSTS: u32 = 200_000;
        let inbox = Arc::new(Inbox::new());
        assert!(inbox.set_waiter());
        let producer = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                for n in 0..POSTS {
                    inbox.post(ev(SessionChange::Lock, n));
                }
            })
        };
        let mut delivered = 0usize;
        let mut lost = 0usize;
        let mut next = 0u32;
        let t0 = Instant::now();
        while delivered + lost < POSTS as usize && t0.elapsed() < Duration::from_secs(60) {
            inbox.wait(Duration::from_millis(1));
            let drained = inbox.drain();
            for e in &drained.events {
                assert_eq!(e.change, SessionChange::Lock);
                assert!(e.session >= next, "{} arrived after {next}", e.session);
                next = e.session + 1;
            }
            delivered += drained.events.len();
            lost += drained.lost;
        }
        assert!(producer.join().is_ok());
        assert_eq!(delivered + lost, POSTS as usize);
        assert!(delivered > 0);
    }

    #[test]
    fn a_stop_wakes_the_waiter_at_once_and_keeps_it_awake() {
        let inbox = Arc::new(Inbox::new());
        assert!(inbox.set_waiter());
        let stopper = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                inbox.request_stop();
            })
        };
        let t0 = Instant::now();
        while !inbox.stop_requested() && t0.elapsed() < Duration::from_secs(10) {
            inbox.wait(Duration::from_secs(30));
        }
        assert!(stopper.join().is_ok());
        assert!(inbox.stop_requested());
        assert!(t0.elapsed() < Duration::from_secs(10));
        // Once stopped, a wait never sleeps.
        let t1 = Instant::now();
        inbox.wait(Duration::from_secs(30));
        assert!(t1.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn only_the_first_thread_is_the_waiter() {
        let inbox = Arc::new(Inbox::new());
        assert!(inbox.set_waiter());
        let other = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                let registered = inbox.set_waiter();
                // Not the waiter: the wait only sleeps its (short) timeout.
                let t0 = Instant::now();
                inbox.wait(Duration::from_millis(50));
                (registered, t0.elapsed())
            })
        };
        let Ok((registered, slept)) = other.join() else {
            panic!("the other thread panicked");
        };
        assert!(!registered);
        assert!(slept >= Duration::from_millis(50));
    }
}
