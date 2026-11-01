//! The xdg-desktop-portal ScreenCast handshake, shared by the permission consent flow
//! (L12, which closes the session as soon as it holds a restore token) and the capture
//! session (L1, which keeps it open and reads the PipeWire stream it grants).
//!
//! `CreateSession` → `SelectSources` (+ `SelectDevices` for a combined RemoteDesktop
//! session) → `Start`. A combined session shows one dialog and returns one restore token
//! for capture and input (plans/02 §3); persistence is then owned by the RemoteDesktop
//! side, so its ScreenCast selection carries no persist mode or token of its own.
//!
//! ## References
//! * ScreenCast portal (`SelectSources` options `types`, `multiple`, `cursor_mode`,
//!   `persist_mode`, `restore_token`; `Start` results; `OpenPipeWireRemote`;
//!   `AvailableCursorModes`, interface version >= 2 for cursor modes):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
//! * RemoteDesktop portal (`SelectDevices`, combined sessions):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
//! * Session (`Close` "ends all related user interaction (dialogs, etc)", `Closed`):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Session.html>
//! * `ashpd` wrappers: <https://docs.rs/ashpd/0.13.13/ashpd/desktop/screencast/>
//! * plans/02-LINUX-FINDINGS.md §1 (restore tokens), §3 (combined sessions).

use std::fmt;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use tokio::runtime::{Builder, Runtime};

use crate::portal_token::StoredToken;

/// What the session asks the user for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// ScreenCast only.
    Screen,
    /// RemoteDesktop (keyboard + pointer) carrying a ScreenCast selection.
    ScreenAndInput,
}

/// Why a handshake produced no session.
#[derive(Debug)]
pub(crate) enum Failure {
    /// No session bus, no `xdg-desktop-portal`, or a portal without the interface.
    Unreachable(ashpd::Error),
    /// A reachable portal failed or refused a request (the user's cancel included).
    Refused(ashpd::Error),
    /// Nobody answered the dialog within the caller's bound. The session was closed,
    /// which dismisses the dialog.
    TimedOut(Duration),
    /// The caller gave up waiting (its `abandon` future resolved). The session was
    /// closed, which dismisses the dialog.
    Abandoned,
}

impl Failure {
    /// The user dismissed the consent dialog.
    pub(crate) fn is_cancel(&self) -> bool {
        matches!(
            self,
            Failure::Refused(ashpd::Error::Response(
                ashpd::desktop::ResponseError::Cancelled
            ))
        )
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Unreachable(e) => write!(f, "desktop portal unreachable: {e}"),
            Failure::Refused(e) => write!(f, "desktop portal request failed: {e}"),
            Failure::TimedOut(d) => write!(f, "no answer within {} s", d.as_secs()),
            Failure::Abandoned => write!(f, "the request was abandoned"),
        }
    }
}

/// An open portal session of either kind.
pub(crate) enum PortalSession {
    Screen(Session<Screencast>),
    Combined(Session<RemoteDesktop>),
}

impl PortalSession {
    /// Resolves when the portal closes the session (the user pressed "Stop sharing"
    /// in the desktop's indicator, the compositor ended it, or the portal restarted).
    /// An error means the signal could not be subscribed to.
    pub(crate) async fn closed(&self) -> Result<(), ashpd::Error> {
        match self {
            PortalSession::Screen(s) => {
                let mut signal = std::pin::pin!(s.receive_closed().await?);
                signal.next().await;
            }
            PortalSession::Combined(s) => {
                let mut signal = std::pin::pin!(s.receive_closed().await?);
                signal.next().await;
            }
        }
        Ok(())
    }

    /// End the session. A failed close only leaves the portal to time the session
    /// out, so it is logged, not propagated.
    pub(crate) async fn close(self) {
        let closed = match &self {
            PortalSession::Screen(s) => s.close().await,
            PortalSession::Combined(s) => s.close().await,
        };
        if let Err(e) = closed {
            tracing::debug!("closing the portal session failed: {e}");
        }
    }
}

/// A started session: the streams the user granted and the replacement restore token.
pub(crate) struct Granted {
    pub session: PortalSession,
    /// The ScreenCast proxy `OpenPipeWireRemote` goes through (for both kinds).
    pub screencast: Screencast,
    /// The RemoteDesktop proxy `ConnectToEIS` goes through; `Some` exactly for a
    /// combined session.
    pub remote_desktop: Option<RemoteDesktop>,
    pub streams: Vec<Stream>,
    /// `None` when the portal does not persist (interface version < 4, or the user
    /// unticked "remember" in the dialog).
    pub token: Option<StoredToken>,
}

impl Granted {
    /// A file descriptor of the PipeWire remote on which only the granted streams are
    /// visible.
    pub(crate) async fn open_pipewire_remote(&self) -> Result<OwnedFd, ashpd::Error> {
        match &self.session {
            PortalSession::Screen(s) => {
                self.screencast
                    .open_pipe_wire_remote(s, Default::default())
                    .await
            }
            PortalSession::Combined(s) => {
                self.screencast
                    .open_pipe_wire_remote(s, Default::default())
                    .await
            }
        }
    }
}

impl Granted {
    /// The socket to the compositor's EIS server (libei) for a combined session.
    /// Call it once per session: the connection is the session's one input
    /// channel (the shared slot's single injector makes the call).
    pub(crate) async fn connect_to_eis(&self) -> Result<OwnedFd, String> {
        match (&self.session, &self.remote_desktop) {
            (PortalSession::Combined(s), Some(rd)) => rd
                .connect_to_eis(s, Default::default())
                .await
                .map_err(|e| format!("the RemoteDesktop portal refused the libei connection: {e}")),
            _ => Err("this portal session carries no remote control".into()),
        }
    }
}

/// The best cursor mode the portal offers. Metadata keeps the pointer out of the
/// pixels (a pointer move then costs no video frame, as with DXGI's pointer shape);
/// Embedded at least shows it. `None` (portal version 1, no property) leaves the
/// portal default, which is Hidden.
pub(crate) fn cursor_mode(available: Option<BitFlags<CursorMode>>) -> Option<CursorMode> {
    let available = available?;
    [CursorMode::Metadata, CursorMode::Embedded]
        .into_iter()
        .find(|m| available.contains(*m))
}

/// Run the handshake up to `Start`. `previous` is the stored restore token of the same
/// scope (a screen-only token cannot restore devices). `bound` limits the time spent
/// waiting for the dialog; `None` waits as long as the user takes. When `abandon`
/// resolves first, the session is closed (dismissing the dialog) and the result is
/// [`Failure::Abandoned`]; pass `std::future::pending()` to never abandon.
pub(crate) async fn start(
    scope: Scope,
    previous: Option<&str>,
    bound: Option<Duration>,
    abandon: impl Future<Output = ()>,
) -> Result<Granted, Failure> {
    let screencast = Screencast::new().await.map_err(Failure::Unreachable)?;
    // Failing to read the property only costs the pointer metadata.
    let cursor = cursor_mode(screencast.available_cursor_modes().await.ok());
    // One monitor (D4 lists monitors; on Wayland the user picks it in the dialog and
    // the restore token remembers the pick).
    let sources = SelectSourcesOptions::default()
        .set_sources(BitFlags::from(SourceType::Monitor))
        .set_multiple(false)
        .set_cursor_mode(cursor);
    match scope {
        Scope::Screen => {
            let session = screencast
                .create_session(Default::default())
                .await
                .map_err(Failure::Refused)?;
            let handshake = async {
                screencast
                    .select_sources(
                        &session,
                        sources
                            .set_persist_mode(PersistMode::ExplicitlyRevoked)
                            .set_restore_token(previous),
                    )
                    .await?;
                let started = screencast
                    .start(&session, None, Default::default())
                    .await?
                    .response()?;
                let token = started.restore_token().map(|token| StoredToken {
                    token: token.to_owned(),
                    covers_input: false,
                });
                Ok((started.streams().to_vec(), token))
            };
            match bounded(handshake, bound, abandon).await {
                Ok((streams, token)) => Ok(Granted {
                    session: PortalSession::Screen(session),
                    screencast,
                    remote_desktop: None,
                    streams,
                    token,
                }),
                Err(e) => {
                    PortalSession::Screen(session).close().await;
                    Err(e)
                }
            }
        }
        Scope::ScreenAndInput => {
            let remote_desktop = RemoteDesktop::new().await.map_err(Failure::Unreachable)?;
            let session = remote_desktop
                .create_session(Default::default())
                .await
                .map_err(Failure::Refused)?;
            let handshake = async {
                remote_desktop
                    .select_devices(
                        &session,
                        SelectDevicesOptions::default()
                            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                            .set_persist_mode(PersistMode::ExplicitlyRevoked)
                            .set_restore_token(previous),
                    )
                    .await?;
                screencast.select_sources(&session, sources).await?;
                let started = remote_desktop
                    .start(&session, None, Default::default())
                    .await?
                    .response()?;
                let covers_input = started
                    .devices()
                    .contains(DeviceType::Keyboard | DeviceType::Pointer);
                let token = started.restore_token().map(|token| StoredToken {
                    token: token.to_owned(),
                    covers_input,
                });
                Ok((started.streams().to_vec(), token))
            };
            match bounded(handshake, bound, abandon).await {
                Ok((streams, token)) => Ok(Granted {
                    session: PortalSession::Combined(session),
                    screencast,
                    remote_desktop: Some(remote_desktop),
                    streams,
                    token,
                }),
                Err(e) => {
                    PortalSession::Combined(session).close().await;
                    Err(e)
                }
            }
        }
    }
}

async fn bounded<T>(
    handshake: impl Future<Output = Result<T, ashpd::Error>>,
    bound: Option<Duration>,
    abandon: impl Future<Output = ()>,
) -> Result<T, Failure> {
    let deadline = async {
        match bound {
            Some(limit) => {
                tokio::time::sleep(limit).await;
                limit
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        result = handshake => result.map_err(Failure::Refused),
        limit = deadline => Err(Failure::TimedOut(limit)),
        () = abandon => Err(Failure::Abandoned),
    }
}

/// One small runtime for all portal traffic. `ashpd` caches its D-Bus connection in a
/// process-wide static whose background tasks live on the runtime that created it, so a
/// per-call runtime would leave later calls with a dead connection. Worker threads
/// (not `current_thread`) so those tasks run without anyone blocking on the runtime.
pub(crate) fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    match RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("sharectrl-portal")
            .enable_all()
            .build()
    }) {
        Ok(rt) => Some(rt),
        Err(e) => {
            tracing::warn!("cannot start the portal runtime: {e}");
            None
        }
    }
}

/// Run `fut` on the portal runtime and wait up to `timeout`. Waiting on a channel (not
/// `Runtime::block_on`) keeps this safe to call from inside another tokio runtime.
/// `None` = no runtime or no answer in time; the future then still runs to completion
/// on the runtime, so it must clean up after itself.
pub(crate) fn block_on<T: Send + 'static>(
    fut: impl Future<Output = T> + Send + 'static,
    timeout: Duration,
) -> Option<T> {
    let rt = runtime()?;
    let (tx, rx) = mpsc::channel();
    rt.spawn(async move {
        // The receiver is gone after a timeout; nobody is left to tell.
        let _ = tx.send(fut.await);
    });
    rx.recv_timeout(timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::ResponseError;

    #[test]
    fn metadata_cursor_is_preferred_then_embedded() {
        let all = CursorMode::Hidden | CursorMode::Embedded | CursorMode::Metadata;
        assert_eq!(cursor_mode(Some(all)), Some(CursorMode::Metadata));
        let wlr = CursorMode::Hidden | CursorMode::Embedded;
        assert_eq!(cursor_mode(Some(wlr)), Some(CursorMode::Embedded));
        // Hidden alone is the default anyway; asking for it explicitly adds nothing.
        assert_eq!(cursor_mode(Some(BitFlags::from(CursorMode::Hidden))), None);
        assert_eq!(cursor_mode(None), None);
    }

    #[test]
    fn only_a_refused_cancel_counts_as_the_user_declining() {
        assert!(Failure::Refused(ashpd::Error::Response(ResponseError::Cancelled)).is_cancel());
        assert!(!Failure::Refused(ashpd::Error::Response(ResponseError::Other)).is_cancel());
        assert!(!Failure::Unreachable(ashpd::Error::NoResponse).is_cancel());
        assert!(!Failure::TimedOut(Duration::from_secs(1)).is_cancel());
    }

    #[test]
    fn a_dialog_nobody_answers_times_out() {
        let rt = runtime().unwrap();
        let never = std::future::pending::<Result<(), ashpd::Error>>();
        let r = rt.block_on(bounded(
            never,
            Some(Duration::from_millis(20)),
            std::future::pending(),
        ));
        assert!(matches!(r, Err(Failure::TimedOut(_))));
        let ok = async { Ok::<_, ashpd::Error>(7) };
        assert_eq!(
            rt.block_on(bounded(ok, None, std::future::pending()))
                .unwrap(),
            7
        );
    }

    #[test]
    fn an_abandoned_dialog_ends_at_once() {
        let rt = runtime().unwrap();
        let never = std::future::pending::<Result<(), ashpd::Error>>();
        let r = rt.block_on(bounded(never, None, async {}));
        assert!(matches!(r, Err(Failure::Abandoned)));
    }
}
