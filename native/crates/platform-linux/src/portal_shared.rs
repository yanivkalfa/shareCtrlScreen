//! The process's one live combined RemoteDesktop + ScreenCast portal session,
//! shared by remote control (the libei injector), the Wayland capture when it
//! runs on a combined restore token (or remote control would use the portal),
//! and the permissions "Grant" flow.
//!
//! Why shared: the restore token is single-use (every `Start` returns its
//! replacement), so two sessions started from the same stored token race and
//! the loser shows the consent dialog again; and one combined session is one
//! dialog for capture and input (plans/02 §3). [`acquire`] hands every user
//! the same [`SharedSession`] while any holds it.
//!
//! Who holds the session: every holder keeps an `Arc<SharedSession>`; the
//! handshake (L1's [`portal_session::start`] with `Scope::ScreenAndInput`,
//! keeping its cursor-mode and single-monitor ScreenCast options) runs in a
//! task of its own on the portal runtime, so a holder that stops waiting
//! never strands it. When the last holder drops, that task either abandons a
//! handshake still in its dialog (L1's `abandon`, which closes the session and
//! the dialog) or closes the granted session. The next `acquire` starts afresh,
//! so a refusal is not sticky beyond the holders that saw it.
//!
//! The lifecycle ([`Shared`], [`Slot`]) is generic over what the handshake
//! produces, so it is tested with a fake handshake and no D-Bus.
//!
//! ## References
//! * RemoteDesktop portal (combined sessions, `ConnectToEIS`, restore tokens):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
//! * Session `Close` ends related dialogs:
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Session.html>
//! * tokio `watch` (a dropped sender ends `changed()`):
//!   <https://docs.rs/tokio/1/tokio/sync/watch/index.html>

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use ashpd::desktop::remote_desktop::DeviceType;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::permissions::previous_token;
use crate::portal_session::{self, runtime, Failure, Granted, Scope};
use crate::portal_token;

/// Why the shared session could not start, worded for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedError {
    /// The user dismissed the consent dialog (as opposed to a portal failure).
    pub(crate) dismissed: bool,
    pub(crate) message: String,
}

impl std::fmt::Display for SharedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Resolves when the last holder has let go.
pub(crate) type Abandon = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What a handshake ends with: `None` when it was abandoned (nobody waits).
type Handshake<T> = Pin<Box<dyn Future<Output = Option<Result<T, SharedError>>> + Send>>;

/// What the handshake task publishes once.
struct Outcome<T> {
    result: OnceLock<Result<T, SharedError>>,
}

/// One shared handshake and its result.
///
/// Fields drop in declaration order, which matters: the outcome reference
/// goes before the release signal, so when the handshake task wakes on the
/// last release it holds the only reference and can take the result out.
pub(crate) struct Shared<T: Send + Sync + 'static> {
    outcome: Arc<Outcome<T>>,
    /// Turns `true` once `outcome` is set.
    ready: watch::Receiver<bool>,
    /// Never sent on; dropping it (with the last holder) tells the handshake
    /// task to abandon the dialog or close what it produced.
    _release: watch::Sender<()>,
}

/// The shared portal session.
pub(crate) type SharedSession = Shared<Granted>;

impl<T: Send + Sync + 'static> Shared<T> {
    /// Run `start` on `rt` (it gets a future that resolves once no holder is
    /// left) and, if it produced a value, hand that value to `close` after
    /// the last holder lets go. Without a runtime the result is `unavailable`.
    fn spawn<S, C, CF>(rt: Option<&Runtime>, start: S, close: C, unavailable: SharedError) -> Self
    where
        S: FnOnce(Abandon) -> Handshake<T>,
        C: FnOnce(T) -> CF + Send + 'static,
        CF: Future<Output = ()> + Send + 'static,
    {
        let outcome = Arc::new(Outcome {
            result: OnceLock::new(),
        });
        let (ready_tx, ready) = watch::channel(false);
        let (release, released) = watch::channel(());
        match rt {
            Some(rt) => {
                let mut abandon_on = released.clone();
                let handshake = start(Box::pin(async move {
                    // Err = every holder dropped its handle.
                    let _ = abandon_on.changed().await;
                }));
                rt.spawn(run(
                    handshake,
                    close,
                    Arc::clone(&outcome),
                    ready_tx,
                    released,
                ));
            }
            None => {
                let _ = outcome.result.set(Err(unavailable));
                ready_tx.send_replace(true);
            }
        }
        Self {
            outcome,
            ready,
            _release: release,
        }
    }

    /// The handshake's result, waiting for it (the consent dialog, unless the
    /// stored token restores it). Every holder gets the same answer. Safe to
    /// stop waiting at any point: the handshake keeps running for the other
    /// holders, and is abandoned once none is left.
    pub(crate) async fn granted(&self) -> Result<&T, &SharedError> {
        let mut ready = self.ready.clone();
        if ready.wait_for(|r| *r).await.is_err() {
            // The task ended without publishing: it only does that when its
            // runtime shut down (process exit).
            return Err(&*SHUTDOWN);
        }
        match self.outcome.result.get() {
            Some(result) => result.as_ref(),
            None => Err(&*SHUTDOWN),
        }
    }
}

static SHUTDOWN: LazyLock<SharedError> = LazyLock::new(|| SharedError {
    dismissed: false,
    message: "the portal runtime stopped (the app is shutting down)".into(),
});

/// The handshake task: run it, publish, then hold the value until the last
/// holder lets go and close it.
async fn run<T, C, CF>(
    handshake: Handshake<T>,
    close: C,
    outcome: Arc<Outcome<T>>,
    ready: watch::Sender<bool>,
    mut released: watch::Receiver<()>,
) where
    T: Send + Sync + 'static,
    C: FnOnce(T) -> CF,
    CF: Future<Output = ()>,
{
    let Some(result) = handshake.await else {
        tracing::debug!("portal: shared session abandoned before it was granted");
        return;
    };
    let granted = result.is_ok();
    let _ = outcome.result.set(result);
    ready.send_replace(true);
    drop(ready);
    if !granted {
        return;
    }
    // Hold until no handle is left (`changed` errs once the sender is gone).
    while released.changed().await.is_ok() {}
    // The holders borrowed the value through their handles, which are all
    // gone, so this task holds the last reference to the outcome.
    if let Some(Ok(value)) = Arc::into_inner(outcome).and_then(|o| o.result.into_inner()) {
        close(value).await;
    }
}

/// The process-wide holder of the live shared value: a weak reference, so it
/// never keeps a session open by itself.
pub(crate) struct Slot<T: Send + Sync + 'static>(Mutex<Weak<Shared<T>>>);

impl<T: Send + Sync + 'static> Slot<T> {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(Weak::new()))
    }

    fn lock(&self) -> MutexGuard<'_, Weak<Shared<T>>> {
        // A poisoned lock only means another thread panicked while swapping a
        // Weak; the Weak itself is always valid.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live value, or a new one from `make`.
    fn acquire(&self, make: impl FnOnce() -> Shared<T>) -> Arc<Shared<T>> {
        let mut current = self.lock();
        if let Some(live) = current.upgrade() {
            return live;
        }
        let fresh = Arc::new(make());
        *current = Arc::downgrade(&fresh);
        fresh
    }

    fn live(&self) -> Option<Arc<Shared<T>>> {
        self.lock().upgrade()
    }
}

static CURRENT: Slot<Granted> = Slot::new();

/// The live shared session, or a new one whose handshake starts now.
/// `state_dir` holds the restore token (`Paths::log_dir`).
pub(crate) fn acquire(state_dir: &Path) -> Arc<SharedSession> {
    acquire_with_token(&portal_token::path_in(state_dir))
}

/// [`acquire`] for a caller that already has the token file's path.
pub(crate) fn acquire_with_token(token_path: &Path) -> Arc<SharedSession> {
    CURRENT.acquire(|| {
        let token_path = token_path.to_owned();
        Shared::spawn(
            runtime(),
            move |abandon| -> Handshake<Granted> {
                Box::pin(portal_handshake(token_path, abandon))
            },
            |granted: Granted| granted.session.close(),
            SharedError {
                dismissed: false,
                message: "the desktop portal cannot be reached: the portal runtime could not \
                          start (see app.log)"
                    .into(),
            },
        )
    })
}

/// The shared session if one is held right now (an injector is running),
/// without starting one.
pub(crate) fn live() -> Option<Arc<SharedSession>> {
    CURRENT.live()
}

/// L1's combined handshake, with the token bookkeeping of a single-use token.
async fn portal_handshake(
    token_path: PathBuf,
    abandon: Abandon,
) -> Option<Result<Granted, SharedError>> {
    let stored = portal_token::read(&token_path);
    // Only a token issued with remote control can restore devices.
    let previous = previous_token(true, stored.as_ref());
    match portal_session::start(Scope::ScreenAndInput, previous, None, abandon).await {
        Ok(granted) => {
            // A token handed to `Start` is spent whatever happens next.
            portal_token::replace(&token_path, previous.is_some(), granted.token.as_ref());
            if !granted.covers_input {
                tracing::warn!(
                    "portal: remote control was granted without both {:?} and {:?}; input \
                     will not reach the desktop. Allow both keyboard and pointer when asked",
                    DeviceType::Keyboard,
                    DeviceType::Pointer
                );
            }
            Some(Ok(granted))
        }
        // Nobody is waiting; L1's start closed the session.
        Err(Failure::Abandoned) => None,
        Err(f) => {
            if previous.is_some() && token_spent_by(&f) {
                portal_token::replace(&token_path, true, None);
            }
            Some(Err(SharedError {
                dismissed: f.is_cancel(),
                message: describe(&f),
            }))
        }
    }
}

/// Whether a failed handshake spent the token it was handed (L12's rule:
/// spent = handed to that `Start`). Only a dismissed dialog proves it: the
/// dialog appeared, so `Start` saw the token and it did not restore. Every
/// other failure may have happened before `Start` (the portal not up yet at
/// an autostart login, a portal restart, `CreateSession` or `Select*`
/// refused), and deleting a still-valid grant would make the next
/// connection to an unattended host need someone at it.
fn token_spent_by(f: &Failure) -> bool {
    f.is_cancel()
}

fn describe(f: &Failure) -> String {
    if f.is_cancel() {
        return "the remote-control request was declined on this computer; accept \"Allow \
                remote control\" when it appears, or grant it ahead of time in the permissions \
                screen"
            .into();
    }
    match f {
        Failure::Unreachable(e) => format!(
            "the desktop portal cannot be reached ({e}); remote control on GNOME/KDE needs \
             xdg-desktop-portal with this desktop's backend running"
        ),
        f => format!("the RemoteDesktop portal could not start a session: {f}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::oneshot;

    /// Counts handshakes started, abandoned and values closed.
    #[derive(Default)]
    struct Counters {
        started: AtomicUsize,
        abandoned: AtomicUsize,
        closed: AtomicUsize,
    }

    fn rt() -> &'static Runtime {
        runtime().expect("portal runtime")
    }

    fn unavailable() -> SharedError {
        SharedError {
            dismissed: false,
            message: "no runtime".into(),
        }
    }

    /// A handshake that answers with whatever `answer` sends, and records an
    /// abandon when every holder goes before it answered.
    fn fake(
        counters: &Arc<Counters>,
        answer: oneshot::Receiver<Result<u32, SharedError>>,
    ) -> Shared<u32> {
        let (c_start, c_close) = (Arc::clone(counters), Arc::clone(counters));
        Shared::spawn(
            Some(rt()),
            move |abandon| -> Handshake<u32> {
                c_start.started.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    tokio::select! {
                        answer = answer => answer.ok(),
                        () = abandon => {
                            c_start.abandoned.fetch_add(1, Ordering::SeqCst);
                            None
                        }
                    }
                })
            },
            move |_value| async move {
                c_close.closed.fetch_add(1, Ordering::SeqCst);
            },
            unavailable(),
        )
    }

    fn eventually(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn the_last_holder_closes_a_granted_session_once() {
        let counters = Arc::new(Counters::default());
        let (tx, rx) = oneshot::channel();
        let first = Arc::new(fake(&counters, rx));
        let second = Arc::clone(&first);
        tx.send(Ok(7)).unwrap();
        assert_eq!(rt().block_on(first.granted()), Ok(&7));
        assert_eq!(
            rt().block_on(second.granted()),
            Ok(&7),
            "every holder, one answer"
        );
        drop(first);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(counters.closed.load(Ordering::SeqCst), 0, "still held");
        drop(second);
        eventually("the close", || counters.closed.load(Ordering::SeqCst) == 1);
        assert_eq!(counters.abandoned.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dropping_every_holder_mid_dialog_abandons_it() {
        let counters = Arc::new(Counters::default());
        let (tx, rx) = oneshot::channel();
        let held = fake(&counters, rx);
        drop(held);
        eventually("the abandon", || {
            counters.abandoned.load(Ordering::SeqCst) == 1
        });
        assert_eq!(counters.closed.load(Ordering::SeqCst), 0);
        // The answer arriving later has nobody to reach.
        assert!(tx.send(Ok(1)).is_err());
    }

    #[test]
    fn a_refusal_is_shared_but_not_sticky() {
        let slot: Slot<u32> = Slot::new();
        let counters = Arc::new(Counters::default());
        let (tx, rx) = oneshot::channel();
        let mut answer = Some(rx);
        let first = slot.acquire(|| fake(&counters, answer.take().unwrap()));
        let again = slot.acquire(|| unreachable!("a live session is reused"));
        assert!(Arc::ptr_eq(&first, &again));
        assert!(slot.live().is_some());
        let refused = SharedError {
            dismissed: true,
            message: "declined".into(),
        };
        tx.send(Err(refused.clone())).unwrap();
        assert_eq!(rt().block_on(again.granted()), Err(&refused));
        drop((first, again));
        assert!(
            slot.live().is_none(),
            "the slot never keeps a session itself"
        );
        let (tx2, rx2) = oneshot::channel();
        let fresh = slot.acquire(|| fake(&counters, rx2));
        tx2.send(Ok(3)).unwrap();
        assert_eq!(
            rt().block_on(fresh.granted()),
            Ok(&3),
            "a new handshake after"
        );
        assert_eq!(counters.started.load(Ordering::SeqCst), 2);
        assert_eq!(
            counters.closed.load(Ordering::SeqCst),
            0,
            "a refusal closes nothing"
        );
    }

    #[test]
    fn only_a_dismissed_dialog_spends_the_stored_token() {
        use ashpd::desktop::ResponseError;
        assert!(token_spent_by(&Failure::Refused(ashpd::Error::Response(
            ResponseError::Cancelled
        ))));
        // Possibly before `Start`: a transient failure keeps a valid grant.
        assert!(!token_spent_by(&Failure::Unreachable(
            ashpd::Error::NoResponse
        )));
        assert!(!token_spent_by(&Failure::Refused(ashpd::Error::Response(
            ResponseError::Other
        ))));
        assert!(!token_spent_by(&Failure::Refused(ashpd::Error::NoResponse)));
        assert!(!token_spent_by(&Failure::TimedOut(Duration::from_secs(1))));
    }

    #[test]
    fn without_a_runtime_the_answer_is_immediate() {
        let shared: Shared<u32> = Shared::spawn(
            None,
            |_| unreachable!("nothing to run on"),
            |_value| async {},
            unavailable(),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(shared.granted()), Err(&unavailable()));
    }
}
