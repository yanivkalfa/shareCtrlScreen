//! The process's one live combined RemoteDesktop + ScreenCast portal session,
//! shared by remote control (the libei injector), the Wayland capture when it
//! runs on a combined restore token, and the permissions "Grant" flow.
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
//! the dialog) or closes the granted session. The next `acquire` starts afresh.
//!
//! ## References
//! * RemoteDesktop portal (combined sessions, `ConnectToEIS`, restore tokens):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
//! * Session `Close` ends related dialogs:
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Session.html>
//! * tokio `watch` (a dropped sender ends `changed()`):
//!   <https://docs.rs/tokio/1/tokio/sync/watch/index.html>

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError, Weak};

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

/// What the handshake task publishes once.
struct Outcome {
    result: OnceLock<Result<Granted, SharedError>>,
}

/// Fields drop in declaration order, which matters: the outcome reference
/// goes before the release signal, so when the handshake task wakes on the
/// last release it holds the only reference and can take the session out.
pub(crate) struct SharedSession {
    outcome: Arc<Outcome>,
    /// Turns `true` once `outcome` is set.
    ready: watch::Receiver<bool>,
    /// Never sent on; dropping it (with the last holder) tells the handshake
    /// task to abandon the dialog or close the session.
    _release: watch::Sender<()>,
}

static CURRENT: Mutex<Weak<SharedSession>> = Mutex::new(Weak::new());

/// The live shared session, or a new one whose handshake starts now.
/// `state_dir` holds the restore token (`Paths::log_dir`).
pub(crate) fn acquire(state_dir: &Path) -> Arc<SharedSession> {
    acquire_with_token(&portal_token::path_in(state_dir))
}

/// [`acquire`] for a caller that already has the token file's path.
pub(crate) fn acquire_with_token(token_path: &Path) -> Arc<SharedSession> {
    let mut current = lock_current();
    if let Some(live) = current.upgrade() {
        return live;
    }
    let session = Arc::new(SharedSession::start(token_path.to_owned()));
    *current = Arc::downgrade(&session);
    session
}

/// The shared session if one is held right now (an injector is running),
/// without starting one.
pub(crate) fn live() -> Option<Arc<SharedSession>> {
    lock_current().upgrade()
}

fn lock_current() -> std::sync::MutexGuard<'static, Weak<SharedSession>> {
    // A poisoned lock only means another thread panicked while swapping a
    // Weak; the Weak itself is always valid.
    CURRENT.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SharedSession {
    fn start(token_path: PathBuf) -> Self {
        let outcome = Arc::new(Outcome {
            result: OnceLock::new(),
        });
        let (ready_tx, ready) = watch::channel(false);
        let (release, released) = watch::channel(());
        match runtime() {
            Some(rt) => {
                rt.spawn(handshake(
                    token_path,
                    Arc::clone(&outcome),
                    ready_tx,
                    released,
                ));
            }
            None => {
                let _ = outcome.result.set(Err(SharedError {
                    dismissed: false,
                    message: "the desktop portal cannot be reached: the portal runtime \
                              could not start (see app.log)"
                        .into(),
                }));
                ready_tx.send_replace(true);
            }
        }
        Self {
            outcome,
            ready,
            _release: release,
        }
    }

    /// The granted session, waiting for the handshake (the consent dialog,
    /// unless the stored token restores it). Every holder gets the same
    /// answer. Safe to stop waiting at any point: the handshake keeps running
    /// for the other holders, and is abandoned once none is left.
    pub(crate) async fn granted(&self) -> Result<&Granted, &SharedError> {
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

/// The handshake task: run it, store the replacement token, publish, then
/// hold the session until the last holder lets go and close it.
async fn handshake(
    token_path: PathBuf,
    outcome: Arc<Outcome>,
    ready: watch::Sender<bool>,
    mut released: watch::Receiver<()>,
) {
    let stored = portal_token::read(&token_path);
    // Only a token issued with remote control can restore devices.
    let previous = previous_token(true, stored.as_ref());
    let mut abandon_on = released.clone();
    let abandon = async move {
        // Err = every holder dropped its handle.
        let _ = abandon_on.changed().await;
    };
    let result = portal_session::start(Scope::ScreenAndInput, previous, None, abandon).await;
    let published = match result {
        Ok(granted) => {
            // A token handed to `Start` is spent whatever happens next.
            portal_token::replace(&token_path, previous.is_some(), granted.token.as_ref());
            Ok(granted)
        }
        Err(Failure::Abandoned) => {
            tracing::debug!("portal: shared session abandoned before it was granted");
            return;
        }
        Err(f) => Err(SharedError {
            dismissed: f.is_cancel(),
            message: describe(&f),
        }),
    };
    let granted_ok = published.is_ok();
    let _ = outcome.result.set(published);
    ready.send_replace(true);
    drop(ready);
    if !granted_ok {
        return;
    }
    // Hold until no handle is left (`changed` errs once the sender is gone).
    while released.changed().await.is_ok() {}
    // The holders borrowed the session through their handles, which are all
    // gone, so this task holds the last reference to the outcome.
    match Arc::into_inner(outcome).and_then(|o| o.result.into_inner()) {
        Some(Ok(granted)) => granted.session.close().await,
        Some(Err(_)) | None => {}
    }
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
