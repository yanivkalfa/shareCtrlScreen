//! When the service (re)launches the host: the decision half of the session
//! loop, kept free of Win32 calls so it is unit-tested on every OS.
//!
//! The service keeps one host running in the active console session (§8b). It
//! launches one when a session becomes active, and launches a new one when the
//! host it started dies: a crash, or being killed. It does NOT relaunch a host
//! that exited cleanly (exit code 0). Code 0 is what every deliberate exit
//! returns:
//!
//! * the user picked Quit (the tray menu),
//! * the updater handed over to the installer (`tauri-plugin-updater` exits the
//!   process with 0 on Windows; the installer then restarts the service, which
//!   launches the new host — relaunching the OLD exe here would race it),
//! * a duplicate launch stepped aside for a host already running in the session
//!   (`platform_windows::instance`).
//!
//! Closing the window does not exit the host at all: it hides to the tray.
//! A cleanly-exited host stays down until the session changes (sign-out and
//! back in, a user switch), the service restarts, or the user starts the app.
//!
//! A host that keeps dying is relaunched with a doubling delay, so a host that
//! crashes at startup cannot spin the CPU. The delay resets once a host has
//! stayed up for [`STABLE_AFTER`].
//!
//! ## Whether a user is signed in (plan 10 §2 fact 7, plan 10a §3.7, §4.3)
//!
//! The console session can keep its id while nobody is signed in to it (the
//! sign-in screen) and after someone signs in. So a sign-in is not always a
//! session change, and before U2 a sign-in after a run of failed launches
//! waited out the remaining delay, up to [`MAX_RETRY`]. The supervisor now
//! also knows whether a user is signed in at the active console session:
//!
//! * The loop finds out by asking for the user's token whenever no host is
//!   running ([`wants_user_check`](Supervisor::wants_user_check)): that is the
//!   first step of every launch, so it answers exactly "could a launch start
//!   now". A failure is any error: which error Windows returns when nobody is
//!   signed in is not documented, so no error code is singled out.
//! * A logoff event for the active session means the user left
//!   ([`session_event`](Supervisor::session_event)). A logon event changes
//!   nothing by itself; it only wakes the loop, whose token check then
//!   reports the sign-in. So the event and the 2 s poll are the same input
//!   whichever comes first, and a logon event that comes before the token is
//!   available cannot count as a sign-in that then fails.
//!
//! While nobody is signed in, no launch is attempted (it could only fail at
//! the token), so no delay builds up. A sign-in — the token appears after the
//! service saw it missing — is [`user_arrived`](Supervisor::user_arrived): a
//! pending relaunch delay is dropped, the failure count is cleared, and the
//! host is launched at once. A launch that still fails after that starts the
//! backoff again from [`FIRST_RETRY`]. A host that exited cleanly is launched
//! again after a sign-out and sign-in in the same session, as after a session
//! change; while the same user stays signed in it stays down.
//!
//! A brief `None` from the console (the docs say `WTSGetActiveConsoleSessionId`
//! returns 0xFFFFFFFF while a session is being attached or detached) is a
//! session change, as before U2.

use std::time::{Duration, Instant};

/// The first relaunch delay after a failure; doubles with each failure in a row.
pub const FIRST_RETRY: Duration = Duration::from_secs(2);
/// The longest delay between relaunch attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(300);
/// A host that ran this long before dying counts as a fresh failure, not one
/// more in a row.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    /// No host yet in this session: launch one.
    Wanted,
    /// The host this service started is running.
    Running { since: Instant },
    /// The host exited cleanly: leave it down until the session changes.
    Stopped,
    /// The host died, or launching it failed: try again at `at`.
    Retry { at: Instant },
}

/// Whether a user is signed in at the active console session, as far as the
/// service knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum User {
    /// Not checked yet in this session.
    Unknown,
    /// The user's token could not be had, or the user logged off.
    Absent,
    /// The user's token could be had.
    Present,
}

/// What [`Supervisor::user_arrived`] did, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrived {
    /// The relaunch delay that was still pending and is now skipped, if any.
    pub skipped: Option<Duration>,
}

/// What [`Supervisor::session_event`] did with an event, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventEffect {
    /// The event is about another session (RDP, another user): ignored.
    OtherSession,
    /// The user of the active session logged off: no launch until a sign-in.
    UserLeft,
    /// Recorded; it changes nothing (a logon is confirmed by the token check,
    /// lock and unlock are for later phases, the rest by the poll).
    Noted,
}

/// How a host the service watched ended, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exit code 0: a deliberate exit. The host stays down.
    Clean,
    /// Any other code: the host is relaunched after `retry_in`.
    Abnormal { code: u32, retry_in: Duration },
}

/// The relaunch policy for the active console session.
#[derive(Debug)]
pub struct Supervisor {
    session: Option<u32>,
    host: Host,
    user: User,
    /// Failures in a row (host deaths and failed launches) since the last
    /// stable run; sets the next delay.
    failures: u32,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            session: None,
            host: Host::Wanted,
            user: User::Unknown,
            failures: 0,
        }
    }

    /// Follow the active console session (`None`: no session at the console).
    /// A session change starts over: the new session wants a host, its user
    /// is not known yet, and the old session's host is no longer this
    /// service's concern. [`poll`](Self::poll) does this too; the loop calls it
    /// first so that events and the token check apply to the current session.
    pub fn follow(&mut self, active: Option<u32>) {
        if active != self.session {
            self.session = active;
            self.host = Host::Wanted;
            self.user = User::Unknown;
            self.failures = 0;
        }
    }

    /// The active console session the supervisor follows.
    pub fn session(&self) -> Option<u32> {
        self.session
    }

    /// Whether the loop should check now that a user is signed in (by asking
    /// for the token): whenever there is a session and no host is running in
    /// it.
    pub fn wants_user_check(&self) -> bool {
        self.session.is_some() && !self.watching()
    }

    /// The user's token for the active session could be had. Turns a sign-in
    /// — the user was known to be absent — into an immediate launch: a pending
    /// `Retry` (or a clean `Stopped`, as the user signed out and back in)
    /// becomes `Wanted` and the failure count is cleared. Returns what it did
    /// for a sign-in, `None` when nothing changed (no session, the user was
    /// already known to be present, or this is the first check of the
    /// session, which wants a host anyway). Idempotent.
    pub fn user_arrived(&mut self, now: Instant) -> Option<Arrived> {
        self.session?;
        let was = std::mem::replace(&mut self.user, User::Present);
        if was != User::Absent {
            return None;
        }
        // WHY this cannot hide a crash loop: the count is cleared only on a
        // change from Absent to Present, and Absent is entered only by a
        // failed token query or a logoff event. A host that keeps crashing
        // while its user stays signed in never passes through Absent, so its
        // delay keeps doubling.
        self.failures = 0;
        let skipped = match self.host {
            Host::Retry { at } => {
                self.host = Host::Wanted;
                Some(at.saturating_duration_since(now)).filter(|d| !d.is_zero())
            }
            Host::Stopped => {
                self.host = Host::Wanted;
                None
            }
            Host::Wanted | Host::Running { .. } => None,
        };
        Some(Arrived { skipped })
    }

    /// The user's token for the active session could not be had, or its user
    /// logged off: no launch is attempted until
    /// [`user_arrived`](Self::user_arrived). Returns whether this changed what
    /// the service knew (for a single log line per sign-out). Idempotent.
    pub fn user_left(&mut self) -> bool {
        if self.session.is_none() {
            return false;
        }
        std::mem::replace(&mut self.user, User::Absent) != User::Absent
    }

    /// A session-change event from the SCM. Only events for the active
    /// console session count. A logoff there is [`user_left`](Self::user_left);
    /// everything else is only recorded (see the module doc).
    pub fn session_event(&mut self, event: crate::events::SessionEvent) -> EventEffect {
        use crate::events::SessionChange;
        if self.session != Some(event.session) {
            return EventEffect::OtherSession;
        }
        match event.change {
            SessionChange::Logoff => {
                self.user_left();
                EventEffect::UserLeft
            }
            _ => EventEffect::Noted,
        }
    }

    /// Called every poll with the active console session (`None`: no
    /// session at the console). Returns the session to launch a host into
    /// now, if any: never while its user is known to be absent. A session
    /// change starts over ([`follow`](Self::follow)).
    pub fn poll(&mut self, active: Option<u32>, now: Instant) -> Option<u32> {
        self.follow(active);
        let session = self.session?;
        if self.user == User::Absent {
            return None;
        }
        match self.host {
            Host::Wanted => Some(session),
            Host::Retry { at } if now >= at => Some(session),
            Host::Running { .. } | Host::Stopped | Host::Retry { .. } => None,
        }
    }

    /// Whether the service is watching a running host (and so should check it
    /// for exit).
    pub fn watching(&self) -> bool {
        matches!(self.host, Host::Running { .. })
    }

    /// The launch [`poll`](Self::poll) asked for succeeded.
    pub fn launched(&mut self, now: Instant) {
        self.host = Host::Running { since: now };
    }

    /// The launch [`poll`](Self::poll) asked for failed. Returns the delay
    /// before the next attempt.
    pub fn launch_failed(&mut self, now: Instant) -> Duration {
        self.schedule_retry(now)
    }

    /// The watched host exited with `code`.
    pub fn exited(&mut self, code: u32, now: Instant) -> Exit {
        if code == 0 {
            self.host = Host::Stopped;
            self.failures = 0;
            return Exit::Clean;
        }
        if let Host::Running { since } = self.host {
            if now.saturating_duration_since(since) >= STABLE_AFTER {
                self.failures = 0;
            }
        }
        Exit::Abnormal {
            code,
            retry_in: self.schedule_retry(now),
        }
    }

    fn schedule_retry(&mut self, now: Instant) -> Duration {
        let delay = retry_delay(self.failures);
        self.failures = self.failures.saturating_add(1);
        self.host = Host::Retry { at: now + delay };
        delay
    }
}

/// [`FIRST_RETRY`] doubled once per earlier failure in a row, capped at
/// [`MAX_RETRY`].
fn retry_delay(earlier_failures: u32) -> Duration {
    FIRST_RETRY
        .checked_mul(1u32.checked_shl(earlier_failures).unwrap_or(u32::MAX))
        .map_or(MAX_RETRY, |d| d.min(MAX_RETRY))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: Option<u32> = Some(1);
    const S2: Option<u32> = Some(2);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A supervisor whose host is running in session 1, started at `t0`.
    fn running(t0: Instant) -> Supervisor {
        let mut s = Supervisor::new();
        assert_eq!(s.poll(S1, t0), S1);
        s.launched(t0);
        s
    }

    #[test]
    fn nobody_signed_in_launches_nothing() {
        let mut s = Supervisor::new();
        assert_eq!(s.poll(None, Instant::now()), None);
    }

    #[test]
    fn a_new_session_gets_one_host() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert!(s.watching());
        // A running host is never launched again.
        assert_eq!(s.poll(S1, t0 + secs(2)), None);
        assert_eq!(s.poll(S1, t0 + secs(3600)), None);
    }

    #[test]
    fn a_clean_exit_stays_down_until_the_session_changes() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert_eq!(s.exited(0, t0 + secs(10)), Exit::Clean);
        assert!(!s.watching());
        assert_eq!(s.poll(S1, t0 + secs(12)), None);
        assert_eq!(s.poll(S1, t0 + secs(100_000)), None);
        // Signing out and back in brings it back.
        assert_eq!(s.poll(None, t0 + secs(100_001)), None);
        assert_eq!(s.poll(S1, t0 + secs(100_002)), S1);
    }

    #[test]
    fn a_crash_is_relaunched_after_the_first_delay() {
        let t0 = Instant::now();
        let mut s = running(t0);
        let died = t0 + secs(600);
        assert_eq!(
            s.exited(0xC000_0409, died),
            Exit::Abnormal {
                code: 0xC000_0409,
                retry_in: FIRST_RETRY
            }
        );
        assert_eq!(s.poll(S1, died + FIRST_RETRY - secs(1)), None);
        assert_eq!(s.poll(S1, died + FIRST_RETRY), S1);
    }

    #[test]
    fn repeated_quick_crashes_back_off_up_to_the_cap() {
        let mut now = Instant::now();
        let mut s = running(now);
        let mut delays = Vec::new();
        for _ in 0..12 {
            now += secs(1); // dies one second after each start
            let Exit::Abnormal { retry_in, .. } = s.exited(1, now) else {
                panic!("a non-zero exit is abnormal");
            };
            delays.push(retry_in.as_secs());
            now += retry_in;
            assert_eq!(s.poll(S1, now), S1);
            s.launched(now);
        }
        assert_eq!(
            delays,
            [2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300, 300],
            "doubling from FIRST_RETRY, capped at MAX_RETRY"
        );
    }

    #[test]
    fn a_host_that_ran_a_while_resets_the_backoff() {
        let mut now = Instant::now();
        let mut s = running(now);
        for _ in 0..5 {
            now += secs(1);
            let Exit::Abnormal { retry_in, .. } = s.exited(1, now) else {
                panic!("abnormal");
            };
            now += retry_in;
            s.poll(S1, now);
            s.launched(now);
        }
        now += STABLE_AFTER;
        assert_eq!(
            s.exited(1, now),
            Exit::Abnormal {
                code: 1,
                retry_in: FIRST_RETRY
            }
        );
    }

    #[test]
    fn failed_launches_back_off_too() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        assert_eq!(s.poll(S1, t0), S1);
        assert_eq!(s.launch_failed(t0), secs(2));
        assert_eq!(s.poll(S1, t0 + secs(1)), None);
        assert_eq!(s.poll(S1, t0 + secs(2)), S1);
        assert_eq!(s.launch_failed(t0 + secs(2)), secs(4));
    }

    #[test]
    fn a_session_change_launches_at_once_and_forgets_the_backoff() {
        let mut now = Instant::now();
        let mut s = running(now);
        for _ in 0..4 {
            now += secs(1);
            s.exited(1, now);
        }
        // Mid-backoff, the user switches: the new session gets a host now.
        assert_eq!(s.poll(S2, now), S2);
        s.launched(now);
        assert_eq!(
            s.exited(1, now + secs(1)),
            Exit::Abnormal {
                code: 1,
                retry_in: FIRST_RETRY
            }
        );
    }

    // ---- Whether a user is signed in (U2) ----

    use crate::events::{SessionChange, SessionEvent};

    fn event(change: SessionChange, session: Option<u32>) -> SessionEvent {
        SessionEvent {
            change,
            session: session.unwrap_or(u32::MAX),
        }
    }

    /// A supervisor in session 1 whose launches have failed `n` times in a
    /// row (the token was not there while the service could not yet tell),
    /// at `t0`. Returns it and the time the last failure happened.
    fn failing(t0: Instant, n: u32) -> (Supervisor, Instant) {
        let mut s = Supervisor::new();
        let mut now = t0;
        for _ in 0..n {
            assert_eq!(s.poll(S1, now), S1);
            now += s.launch_failed(now);
        }
        (s, now)
    }

    #[test]
    fn a_sign_in_after_a_long_backoff_launches_at_once_and_resets_the_delay() {
        let t0 = Instant::now();
        let (mut s, due) = failing(t0, 9);
        // The next relaunch is due MAX_RETRY after the last failure.
        let last_failure = due - MAX_RETRY;
        let signed_in = last_failure + secs(10);
        assert_eq!(s.poll(S1, signed_in), None, "mid-backoff");
        // The poll's token check: nobody yet, then the user is there.
        assert!(s.user_left());
        assert_eq!(
            s.user_arrived(signed_in),
            Some(Arrived {
                skipped: Some(MAX_RETRY - secs(10))
            })
        );
        assert_eq!(s.poll(S1, signed_in), S1, "launched at once");
        // A launch that fails right after the sign-in waits FIRST_RETRY, not
        // minutes.
        assert_eq!(s.launch_failed(signed_in), FIRST_RETRY);
        assert_eq!(s.poll(S1, signed_in + FIRST_RETRY), S1);
        assert_eq!(s.launch_failed(signed_in + FIRST_RETRY), secs(4));
    }

    #[test]
    fn nobody_signed_in_attempts_no_launch_and_builds_no_delay() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.follow(S1);
        assert!(s.wants_user_check());
        assert!(s.user_left(), "first check: nobody");
        assert!(!s.user_left(), "logged once, not every poll");
        for k in 0..1000 {
            assert_eq!(s.poll(S1, t0 + secs(2 * k)), None);
            assert!(s.wants_user_check(), "keeps checking");
        }
        let signed_in = t0 + secs(2000);
        assert_eq!(s.user_arrived(signed_in), Some(Arrived { skipped: None }));
        assert_eq!(s.poll(S1, signed_in), S1);
        s.launched(signed_in);
        assert!(!s.wants_user_check(), "no token checks while the host runs");
    }

    #[test]
    fn the_first_check_of_a_session_is_not_a_sign_in() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.follow(S1);
        // The service started with the user already signed in.
        assert_eq!(s.user_arrived(t0), None);
        assert_eq!(s.poll(S1, t0), S1);
        assert_eq!(s.user_arrived(t0), None, "idempotent");
    }

    #[test]
    fn a_sign_in_for_another_session_does_nothing() {
        let t0 = Instant::now();
        let (mut s, due) = failing(t0, 6);
        let before = format!("{s:?}");
        for change in [
            SessionChange::Logon,
            SessionChange::Logoff,
            SessionChange::Lock,
        ] {
            assert_eq!(
                s.session_event(event(change, S2)),
                EventEffect::OtherSession
            );
        }
        assert_eq!(format!("{s:?}"), before, "an RDP or other user's session");
        assert_eq!(s.poll(S1, due - secs(1)), None);
        assert_eq!(s.poll(S1, due), S1);
    }

    #[test]
    fn a_logon_event_alone_is_not_a_sign_in() {
        // The logon notice can come before the token is available: only the
        // token check that follows counts.
        let t0 = Instant::now();
        let (mut s, due) = failing(t0, 4);
        assert!(s.user_left());
        assert_eq!(
            s.session_event(event(SessionChange::Logon, S1)),
            EventEffect::Noted
        );
        assert_eq!(
            s.poll(S1, due),
            None,
            "still nobody until the token is there"
        );
        assert!(s.user_arrived(due).is_some());
        assert_eq!(s.poll(S1, due), S1);
    }

    /// The observable state after each step, for comparing two orders.
    fn trace(s: &mut Supervisor, at: Instant) -> (Option<u32>, bool, String) {
        let launch = s.poll(S1, at);
        (launch, s.wants_user_check(), format!("{s:?}"))
    }

    #[test]
    fn event_then_poll_and_poll_then_event_agree() {
        let t0 = Instant::now();
        let t1 = t0 + secs(30);

        // Sign-out: the logoff event and a failing token check, both orders.
        let (mut a, _) = failing(t0, 5);
        a.session_event(event(SessionChange::Logoff, S1));
        a.user_left();
        let (mut b, _) = failing(t0, 5);
        b.user_left();
        b.session_event(event(SessionChange::Logoff, S1));
        assert_eq!(trace(&mut a, t1), trace(&mut b, t1));

        // Sign-in: the logon event and a successful token check, both orders.
        let (mut a, _) = failing(t0, 5);
        a.user_left();
        a.session_event(event(SessionChange::Logon, S1));
        let arrived_a = a.user_arrived(t1);
        let (mut b, _) = failing(t0, 5);
        b.user_left();
        let arrived_b = b.user_arrived(t1);
        b.session_event(event(SessionChange::Logon, S1));
        assert_eq!(arrived_a, arrived_b);
        assert_eq!(trace(&mut a, t1), trace(&mut b, t1));
        assert_eq!(trace(&mut a, t1).0, S1, "the sign-in launches");
    }

    #[test]
    fn repeated_events_and_checks_are_idempotent() {
        let t0 = Instant::now();
        let (mut s, _) = failing(t0, 3);
        assert!(s.user_left());
        for _ in 0..5 {
            assert_eq!(
                s.session_event(event(SessionChange::Logoff, S1)),
                EventEffect::UserLeft
            );
            assert!(!s.user_left());
        }
        let t1 = t0 + secs(100);
        assert!(s.user_arrived(t1).is_some());
        for _ in 0..5 {
            assert_eq!(s.user_arrived(t1), None);
            assert_eq!(
                s.session_event(event(SessionChange::Logon, S1)),
                EventEffect::Noted
            );
        }
        assert_eq!(s.poll(S1, t1), S1);
        s.launched(t1);
        assert_eq!(s.user_arrived(t1 + secs(2)), None);
        assert_eq!(
            s.poll(S1, t1 + secs(2)),
            None,
            "one host, not one per event"
        );
    }

    #[test]
    fn lock_and_unlock_change_nothing() {
        let t0 = Instant::now();
        let mut s = running(t0);
        s.user_arrived(t0);
        let before = format!("{s:?}");
        for change in [
            SessionChange::Lock,
            SessionChange::Unlock,
            SessionChange::ConsoleConnect,
            SessionChange::RemoteControl,
            SessionChange::DesktopReady,
            SessionChange::Other(0x42),
        ] {
            assert_eq!(s.session_event(event(change, S1)), EventEffect::Noted);
        }
        assert_eq!(format!("{s:?}"), before);
        assert_eq!(s.poll(S1, t0 + secs(2)), None);
    }

    #[test]
    fn a_sign_in_while_the_host_runs_changes_nothing() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert!(!s.wants_user_check());
        // A logoff notice while the host still runs, then a sign-in.
        s.session_event(event(SessionChange::Logoff, S1));
        assert_eq!(
            s.user_arrived(t0 + secs(5)),
            Some(Arrived { skipped: None })
        );
        assert!(s.watching(), "still the same running host");
        assert_eq!(s.poll(S1, t0 + secs(6)), None, "no second host");
    }

    #[test]
    fn a_clean_exit_stays_down_while_the_same_user_stays_signed_in() {
        let t0 = Instant::now();
        let mut s = running(t0);
        assert_eq!(s.exited(0, t0 + secs(10)), Exit::Clean);
        assert!(s.wants_user_check());
        // The token check every poll while stopped: the user is still there.
        for k in 0..100 {
            assert_eq!(s.user_arrived(t0 + secs(12 + 2 * k)), None);
            assert_eq!(s.poll(S1, t0 + secs(12 + 2 * k)), None);
        }
    }

    #[test]
    fn a_clean_exit_comes_back_after_signing_out_and_in_in_the_same_session() {
        // The module doc: a cleanly-exited host stays down until the user
        // signs out and back in. The console can keep its session id across
        // that, so the sign-in alone brings the host back.
        let t0 = Instant::now();
        let mut s = running(t0);
        s.user_arrived(t0);
        assert_eq!(s.exited(0, t0 + secs(10)), Exit::Clean);
        assert_eq!(
            s.session_event(event(SessionChange::Logoff, S1)),
            EventEffect::UserLeft
        );
        assert_eq!(s.poll(S1, t0 + secs(20)), None, "nobody signed in");
        assert_eq!(
            s.user_arrived(t0 + secs(30)),
            Some(Arrived { skipped: None })
        );
        assert_eq!(s.poll(S1, t0 + secs(30)), S1);
    }

    #[test]
    fn a_crash_keeps_its_backoff_while_the_user_stays() {
        // Real failures keep the doubling delay: the token check succeeding
        // every poll is not a sign-in.
        let mut now = Instant::now();
        let mut s = running(now);
        s.user_arrived(now);
        let mut delays = Vec::new();
        for _ in 0..5 {
            now += secs(1);
            let Exit::Abnormal { retry_in, .. } = s.exited(1, now) else {
                panic!("abnormal");
            };
            delays.push(retry_in.as_secs());
            assert_eq!(s.user_arrived(now), None);
            assert_eq!(s.poll(S1, now), None);
            now += retry_in;
            assert_eq!(s.poll(S1, now), S1);
            s.launched(now);
        }
        assert_eq!(delays, [2, 4, 8, 16, 32]);
    }

    #[test]
    fn a_session_change_forgets_who_was_signed_in() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.follow(S1);
        assert!(s.user_left());
        assert_eq!(s.poll(S1, t0), None);
        // Fast user switching: the console moves to session 2.
        assert_eq!(s.poll(S2, t0 + secs(1)), S2);
        // No session at all (0xFFFFFFFF): nothing to check or launch.
        s.follow(None);
        assert!(!s.wants_user_check());
        assert!(!s.user_left());
        assert_eq!(s.user_arrived(t0 + secs(2)), None);
        assert_eq!(s.poll(None, t0 + secs(2)), None);
    }

    #[test]
    fn the_delay_never_overflows() {
        assert_eq!(retry_delay(0), FIRST_RETRY);
        assert_eq!(retry_delay(31), MAX_RETRY);
        assert_eq!(retry_delay(32), MAX_RETRY);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY);
    }
}
