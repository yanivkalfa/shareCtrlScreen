//! What the engine tells the app (plans/08a §3). App-level events concern the
//! computer (server link, incoming requests, the workspace); everything about
//! one session is tagged with that session's [`SessionId`], so several
//! sessions can run at once and the app can tell their events apart (I4).
//!
//! Session events come from the engine's own tasks and from each session's
//! threads (which hold a [`SessionUi`], not the engine).

use protocol::Permission;

/// One session, process-local and never reused within a run (08a §3). Never
/// persisted: it means nothing after the app exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Events the engine surfaces to the app UI.
#[derive(Debug, Clone)]
pub enum UiEvent {
    ServerStatus(&'static str),
    /// A viewer wants in (approve mode) — the UI shows the popup and calls back.
    ApprovalRequest {
        from: String,
    },
    /// A transient user-facing message about the computer, not a session.
    Toast(String),
    /// The active workspace changed underneath the UI (a sync merged changes
    /// from another computer, or the account was switched): reload it.
    WorkspaceChanged,
    /// The user pressed a tab-switch chord inside `session`'s video (D3,
    /// 08a §6.19): it was taken out of the key stream, never forwarded. The
    /// app acts on it only when that video is in the main window (plans/09
    /// Decision 2).
    TabShortcut {
        step: TabStep,
        session: SessionId,
    },
    /// Something happened in one session. `host`: this computer is the host
    /// of that session (it is being viewed or controlled).
    Session {
        id: SessionId,
        peer: String,
        host: bool,
        ev: SessionEvent,
    },
}

/// An app window a session's video can be in (plans/09): `WindowId::MAIN`
/// is the main window; the app numbers the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

impl WindowId {
    pub const MAIN: WindowId = WindowId(0);
}

/// Names one video surface the app made, in events about it (plans/09 W2):
/// the address of the surface. Unique among the surfaces alive: the app
/// holds each until it destroys it, and destroys it only after the engine
/// let go of it (`SurfaceReleased` / `Released`), so no other surface can
/// take the address while an event may still name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceTag(usize);

impl SurfaceTag {
    pub fn of(surface: &std::sync::Arc<dyn platform_api::VideoSurface>) -> Self {
        SurfaceTag(std::sync::Arc::as_ptr(surface) as *const () as usize)
    }
}

/// Which way a tab-switch chord goes (Ctrl+Alt+PageDown / PageUp; Cmd+Option
/// on a Mac).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStep {
    Next,
    Prev,
}

/// What happened in one session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Phase(SessionPhase),
    /// The peer has gone quiet but the session is not given up on yet.
    /// `secs_left` counts down to the point where it is dropped. Emitted about
    /// once a second so the UI can show a live countdown instead of leaving the
    /// user staring at a frozen picture wondering whether it is still alive.
    LinkTrouble {
        secs_left: u32,
    },
    /// The peer answered again — the session continues normally.
    LinkRestored,
    /// The host's monitors and the one being shared, for the monitor picker.
    /// On the host it comes from its own capture thread (at session start and
    /// after each switch); on the viewer from the host's `displays` message,
    /// and again whenever the viewer's control permission changes.
    /// `can_switch`: whether this side may switch right now (always on the
    /// host; only with control on the viewer).
    Displays {
        list: Vec<protocol::RemoteDisplay>,
        current: String,
        can_switch: bool,
        /// Monitors also streamed to pop-out windows (D14). The viewer's app
        /// opens a window per entry ([`crate::Engine::attach_popout`]) and
        /// closes those no longer listed ([`crate::Engine::close_popout`]).
        popouts: Vec<protocol::PopoutStream>,
    },
    /// A message about this session (a file sent, a monitor unavailable).
    Toast(String),
    /// Viewer: what the host's screen is (its `hstate`, plans/10 U1). Sent
    /// for every `hstate` the host sends; a host that never sends one (an
    /// older version) means `Normal` throughout. While `Blocked` the engine
    /// hides the session's video and pop-outs; the UI says why, from `why`
    /// (a `protocol::control::hstate::WHY_*` value, or one it does not know).
    /// `kind` names the secure screen while `Secure`.
    HostScreen {
        screen: protocol::HostScreen,
        kind: Option<String>,
        why: Option<String>,
    },
    /// The session's threads are joined and its decoder released: its video
    /// surface may now be destroyed (08a §5.2). Always follows
    /// `Phase(Ended(..))`. It covers every surface the session held: its
    /// current one and one a move was handing it to.
    Released,
    /// The engine let go of `surface` (plans/09 W2): a move finished (the
    /// old surface) or failed (the surface it was moving to). Destroy it.
    SurfaceReleased {
        surface: SurfaceTag,
    },
}

/// Where a session is (08a §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPhase {
    /// Viewer: the connect-request is out; the host has not answered.
    Requesting,
    /// Viewer: the host asked for its password; the user is being asked.
    PasswordRequired { reason: PwReason },
    /// Accepted with `permission`; WebRTC is being set up. Emitted on both
    /// sides (on the host when it accepts).
    Negotiating { permission: Permission },
    /// Over, for `reason`. A `Released` event follows.
    Ended(EndReason),
}

/// Why the password prompt is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwReason {
    /// The host asks for it and none is saved for it.
    First,
    /// The saved password for this host stopped working.
    SavedRejected,
}

/// Why a session ended. Each has at most one sentence for the user
/// ([`EndReason::message`], 08a §7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The user ended it (or cancelled the request).
    UserClosed,
    /// The other computer ended it.
    RemoteEnded,
    /// The host refused: the message says why ("connection busy").
    Rejected(String),
    /// Cancelled because this computer started hosting meanwhile (D1).
    Busy,
    /// The host did not answer in time.
    Timeout,
    /// The signaling server reports the other computer offline.
    Offline,
    /// The link died (no response, transport error).
    LinkLost(String),
    /// It could not be started or kept going here.
    Failed(String),
    /// The account was switched or signed out (D6).
    AccountSwitch,
}

impl EndReason {
    /// The sentence the user sees, if any (a session the user ended needs
    /// none).
    pub fn message(&self) -> Option<String> {
        match self {
            EndReason::UserClosed | EndReason::RemoteEnded | EndReason::AccountSwitch => None,
            EndReason::Rejected(m) => Some(m.clone()),
            EndReason::Busy => {
                Some("Connection cancelled: someone connected to this computer meanwhile".into())
            }
            EndReason::Offline => Some("Peer offline".into()),
            EndReason::Timeout => Some("No answer — timed out".into()),
            EndReason::LinkLost(why) => Some(format!("Disconnected: {why}")),
            EndReason::Failed(why) => Some(format!("Session ended: {why}")),
        }
    }
}

/// How one session's threads reach the UI (they hold no engine): every event
/// they send is tagged with their session.
#[derive(Clone)]
pub(crate) struct SessionUi {
    pub(crate) id: SessionId,
    pub(crate) peer: String,
    pub(crate) host: bool,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
}

impl SessionUi {
    pub(crate) fn send(&self, ev: SessionEvent) {
        let _ = self.tx.send(UiEvent::Session {
            id: self.id,
            peer: self.peer.clone(),
            host: self.host,
            ev,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_endings_the_user_did_not_cause_say_something() {
        assert_eq!(EndReason::UserClosed.message(), None);
        assert_eq!(EndReason::AccountSwitch.message(), None);
        assert_eq!(
            EndReason::Rejected("connection busy".into())
                .message()
                .as_deref(),
            Some("connection busy")
        );
        assert_eq!(
            EndReason::LinkLost("no response from the peer".into())
                .message()
                .as_deref(),
            Some("Disconnected: no response from the peer")
        );
    }
}
