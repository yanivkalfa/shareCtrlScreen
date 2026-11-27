//! Session orchestration (Plan 04 §4 `engine`). Ties the crates together and
//! runs the control plane: it consumes signaling events, drives the connection
//! handshake (contract §3.2, ported from `host.js`/`viewer.js`), enforces the
//! access model (approve popup / password challenge-response, live view↔control
//! switch), and owns the media session (capture→encode→transport on the host,
//! transport→decode→render on the viewer).
//!
//! Everything here is OS-independent: every OS service (capture, codecs, the
//! video surface, input injection, clipboard, secrets) comes from the
//! [`platform_api::Platform`] the app passes to [`Engine::start`].

pub mod events;
pub mod handshake;
mod media;
mod modifiers;
mod session;
mod sessions;
pub mod transfer;
mod tuning;
pub mod turn;
mod workspace;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use platform_api::Platform;
use protocol::{Config, Permission, SignalMsg};
use signaling::{Event as SigEvent, SignalingClient};
use std::sync::Arc;
use std::time::Duration;

pub(crate) use events::SessionUi;
pub use events::{
    EndReason, PwReason, SessionEvent, SessionId, SessionPhase, SurfaceTag, TabStep, UiEvent,
    WindowId,
};
pub use handshake::{ApprovalDecision, Decider};
pub use workspace::WorkspaceSel;

/// How long auto-login waits for the OS secret store to open a saved password
/// before asking the user instead. Long enough to answer a keyring unlock
/// prompt, short enough that a wedged store does not leave the connect hanging.
pub(crate) const SECRET_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// What this computer is doing, in one word ([`Engine::role`]). Sessions are
/// told apart by their [`SessionId`]; this is the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Idle,
    /// Being controlled — we sent the offer and stream our screen.
    Host {
        peer: String,
        permission: Permission,
    },
    /// Controlling — we watch the peer's screen and send input.
    Viewer {
        peer: String,
        permission: Permission,
    },
}

/// How a session finds a network path to the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IcePolicy {
    /// Host, STUN server-reflexive and TURN relay candidates (normal use).
    #[default]
    Full,
    /// Bind to 127.0.0.1, no STUN/TURN: both peers on this machine. For the
    /// loopback test harness and diagnostics.
    LoopbackOnly,
}

/// The peer a signaling message is from (or, for a relay error, about), which
/// picks the queue it is handled on. `None` for messages that concern no single
/// peer (TURN credentials, unknown kinds): those are handled immediately.
fn signal_peer(msg: &SignalMsg) -> Option<String> {
    match msg {
        SignalMsg::ConnectRequest { from, .. }
        | SignalMsg::ConnectResponse { from, .. }
        | SignalMsg::PasswordRequired { from, .. }
        | SignalMsg::Signal { from, .. }
        | SignalMsg::EndSession { from, .. } => from.clone(),
        SignalMsg::RelayError { to, .. } => to.clone(),
        _ => None,
    }
}

/// Why [`Engine::connect`] refused to start a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// This computer is hosting, or letting a viewer in (D1).
    Hosting,
    /// The account is being switched (I10).
    Switching,
    /// The selected account is signed out: its computers are not reachable
    /// until its password is entered (I13).
    SignedOut,
    /// A session with that computer already exists (one per remote, I1).
    AlreadyConnected(SessionId),
    /// A session with that computer, or on that surface, is still ending:
    /// its threads are being joined (it holds both until it is released).
    Ending(SessionId),
    /// Another session presents on that surface.
    SurfaceInUse(SessionId),
    /// That is this computer's own ID.
    SelfId,
    /// Not a computer ID.
    InvalidId,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConnectError::Hosting => {
                "Someone is connected or connecting to this computer: end that first"
            }
            ConnectError::Switching => "The account is being switched: try again in a moment",
            ConnectError::SignedOut => "Sign in to the account first",
            ConnectError::AlreadyConnected(_) => "You are already connected to that computer",
            ConnectError::Ending(_) => {
                "Still disconnecting from that computer: try again in a moment"
            }
            ConnectError::SurfaceInUse(_) => "That window already shows another computer",
            ConnectError::SelfId => "That's this computer's ID",
            ConnectError::InvalidId => "That does not look like a valid ID",
        })
    }
}

impl std::error::Error for ConnectError {}

/// Why [`Engine::move_session`] refused (plans/09 W2, 09a §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// No such viewer session.
    NotFound,
    /// The session ended or is ending.
    Ended,
    /// It is being started, or another move of it is still under way: one
    /// at a time.
    Busy,
    /// That surface already belongs to a session (this one included).
    SurfaceInUse,
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MoveError::NotFound => "That session is gone",
            MoveError::Ended => "That session is ending",
            MoveError::Busy => "That session is busy: try again in a moment",
            MoveError::SurfaceInUse => "That window already shows a computer",
        })
    }
}

impl std::error::Error for MoveError {}

/// A session to end, sent to the engine by a thread that holds no engine (a
/// transport thread whose link died) or by a timer.
pub(crate) struct EndRequest {
    pub(crate) id: SessionId,
    pub(crate) reason: EndReason,
    /// Only if the session's request is still this attempt, unanswered (the
    /// answer timeout of one connect-request, 08a §5.2).
    pub(crate) if_unanswered: Option<u64>,
}

/// The engine. Owns config, the signaling client and the sessions.
pub struct Engine {
    config_path: PathBuf,
    config: Arc<Mutex<Config>>,
    /// False when the config must not be written back this run (see
    /// [`protocol::Loaded::writable`]): every save is skipped, so the user's
    /// file is never overwritten from a state that could lose data.
    writable: AtomicBool,
    /// The workspace store's state: which workspace is reachable, the stamper,
    /// the switch epoch, live tokens (workspace.rs). Locked before `reg` and
    /// `config`.
    ws: Mutex<workspace::WsState>,
    /// Woken after each change to the active workspace (account sync).
    ws_changed: Arc<tokio::sync::Notify>,
    /// The sessions: the host one, the viewer ones, the admission in progress
    /// (session/registry.rs). Locked after `ws`; nothing is taken under it.
    /// Shared with the reapers, which release ended sessions (`Arc`).
    reg: Arc<Mutex<session::registry::Registry>>,
    /// Sessions whose link died on their transport thread, for the engine to
    /// end (the thread holds no engine).
    end_tx: tokio::sync::mpsc::UnboundedSender<EndRequest>,
    /// The runtime the engine was started in, for its timers (UI commands
    /// may call in from threads outside it).
    rt: tokio::runtime::Handle,
    signaling: SignalingClient,
    /// Whether the signaling socket is currently registered with the server. The
    /// UI queries this on boot because the one-shot `ServerStatus` event can fire
    /// before the WebView subscribes (startup race — the dot would stay stuck).
    connected: Arc<AtomicBool>,
    decider: Arc<dyn Decider>,
    ui: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// Pending password nonces per peer: as host, the challenge issued to each
    /// requesting viewer; as viewer, the challenge each host sent.
    pending_nonce: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Viewer decode capabilities per requesting peer, for codec negotiation (§3).
    pending_caps: Arc<Mutex<std::collections::HashMap<String, Vec<String>>>>,
    /// What each requesting peer's viewer advertised besides its codecs
    /// (pop-outs D14, `hstate` plans/10 U1); consumed at accept time like
    /// `pending_caps`.
    viewer_features: Arc<Mutex<std::collections::HashMap<String, session::ViewerFeatures>>>,
    /// TURN relay servers minted by the signaling server (Cloudflare TURN),
    /// expanded to one `IceServer` per URL. Fetched on register and cached; the
    /// pipeline reads them when gathering the relayed candidate.
    turn_servers: Arc<Mutex<Vec<protocol::IceServer>>>,
    /// Per-peer signaling queues: each peer's messages are handled in arrival
    /// order on that peer's own task, so a slow step for one peer (an approval
    /// prompt, a keyring unlock, ICE gathering) never stalls another peer's.
    peer_queues:
        Mutex<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<SignalMsg>>>,
    /// OS services.
    platform: Arc<dyn Platform>,
    /// The live "Translate modifiers for the remote OS" setting, shared with
    /// the running sessions' key translators so a change applies to the next
    /// key without a reconnect. Kept equal to the workspace's preference.
    translate_modifiers: Arc<AtomicBool>,
    ice_policy: Mutex<IcePolicy>,
    /// The windows with a web dialog (Settings, a confirm) open over their
    /// video: their video is hidden meanwhile (08a §6.18, plans/09 J4). Read
    /// by every session's view.
    overlays: session::active::Overlays,
    /// The ActiveGate's applier queue (session/active.rs): every main video
    /// is shown and hidden through it, in the order decided.
    view_ops: session::active::ViewOps,
    /// The viewer session holding the shortcut hook, one at a time (B2).
    hook_owner: Arc<Mutex<Option<SessionId>>>,
    /// The tab-switch chord keys being held, shared by every session's key
    /// translator (D3: a held chord key's repeats and release follow the
    /// keyboard into the next tab, and are taken there too).
    tab_chord_keys: modifiers::TabChordKeys,
    /// Test hook (`test-hooks` feature only): how long a viewer session's
    /// start waits before building anything (ms), so a test can end a session
    /// in the middle of its start.
    #[cfg(feature = "test-hooks")]
    test_start_delay_ms: std::sync::atomic::AtomicU64,
}

impl Engine {
    /// Load config from the platform's config dir, spawn signaling for our UUID,
    /// and return the engine plus the UI and signaling event streams. `decider`
    /// answers approve-mode popups (the app wires it to its modal). Must be called
    /// inside an entered Tokio runtime.
    pub fn start(
        platform: Arc<dyn Platform>,
        decider: Arc<dyn Decider>,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
        tokio::sync::mpsc::UnboundedReceiver<SigEvent>,
    ) {
        let config_path = platform.paths().config_dir.join("config.json");
        let loaded = Config::load(&config_path);
        for note in &loaded.notes {
            tracing::warn!("{note}");
        }
        let config = loaded.config;
        let (signaling, sig_rx) =
            SignalingClient::spawn(config.server_url.clone(), config.uuid.clone());
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (end_tx, mut end_rx) = tokio::sync::mpsc::unbounded_channel();
        let ws = workspace::WsState::at_launch(&config, loaded.legacy_token);
        // The selected workspace's setting (a signed-out account's stored one).
        let prefs = workspace::prefs_in(&config, &ws.sel);
        let translate_modifiers = Arc::new(AtomicBool::new(prefs.translate_modifiers));
        let engine = Arc::new(Self {
            config_path,
            config: Arc::new(Mutex::new(config)),
            writable: AtomicBool::new(loaded.writable),
            ws: Mutex::new(ws),
            ws_changed: Arc::new(tokio::sync::Notify::new()),
            reg: Arc::new(Mutex::new(Default::default())),
            end_tx,
            rt: tokio::runtime::Handle::current(),
            signaling,
            connected: Arc::new(AtomicBool::new(false)),
            decider,
            ui: ui_tx,
            pending_nonce: Arc::new(Mutex::new(Default::default())),
            pending_caps: Arc::new(Mutex::new(Default::default())),
            viewer_features: Arc::new(Mutex::new(Default::default())),
            turn_servers: Arc::new(Mutex::new(Vec::new())),
            peer_queues: Mutex::new(Default::default()),
            platform,
            translate_modifiers,
            ice_policy: Mutex::new(IcePolicy::Full),
            overlays: Default::default(),
            view_ops: session::active::applier(&tokio::runtime::Handle::current()),
            hook_owner: Arc::new(Mutex::new(None)),
            tab_chord_keys: Default::default(),
            #[cfg(feature = "test-hooks")]
            test_start_delay_ms: Default::default(),
        });
        // Sessions whose link died end here, one at a time. Weak: this task
        // must not keep the engine alive.
        let weak = Arc::downgrade(&engine);
        tokio::spawn(async move {
            while let Some(req) = end_rx.recv().await {
                let Some(engine) = weak.upgrade() else { return };
                engine.end_requested(req);
            }
        });
        // Host passwords the migration did not carry over (automatic sign-in
        // was off): their OS secret-store items go too — but only once the
        // migrated file is on disk; until then the old file still refers to
        // them (P1 review).
        if loaded.persisted {
            engine.forget_sealed(loaded.forget_sealed);
        }
        (engine, ui_rx, sig_rx)
    }

    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    /// Whether signaling is currently registered with the server. Queried by the
    /// UI on boot to back-fill the status dot past the startup event race.
    pub fn server_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether this run may write the config file (see `writable`).
    pub fn is_writable(&self) -> bool {
        self.writable.load(Ordering::Relaxed)
    }

    /// Persist a change to this computer's own settings (Settings → device
    /// fields). The person's data goes through [`Engine::with_workspace`].
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.config.lock();
        f(&mut cfg);
        self.save(&cfg);
    }

    /// Write `cfg` to disk, unless this run must not (see `writable`). A
    /// failed write is logged; the in-memory value stays authoritative.
    pub(crate) fn save(&self, cfg: &Config) {
        if !self.writable.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = cfg.persist(&self.config_path) {
            tracing::warn!(
                "config: could not save {} ({e})",
                self.config_path.display()
            );
        }
    }

    /// Dispatch one inbound signaling event without waiting for it to be handled.
    /// The app's signaling loop calls this.
    ///
    /// Messages from (or about) a peer go to that peer's own queue, handled in
    /// arrival order on its own task; everything else is handled right here.
    /// Ordering only matters within one peer (its connect-response must precede
    /// its offer), and a step that waits — the host's approval prompt, a keyring
    /// unlock for a saved password, ICE gathering — must not hold up any other
    /// peer, nor the registration and TURN bookkeeping below.
    pub async fn dispatch_signaling(self: &Arc<Self>, event: SigEvent) {
        let SigEvent::Message(mut msg) = event else {
            self.handle_signaling(event).await;
            return;
        };
        let Some(peer) = signal_peer(&msg) else {
            self.route(msg).await;
            return;
        };
        // Send under the map lock: the worker checks "empty" and removes its
        // entry under the same lock, so a message can never be queued on a
        // worker that has just decided to exit.
        let mut queues = self.peer_queues.lock();
        if let Some(tx) = queues.get(&peer) {
            match tx.send(msg) {
                Ok(()) => return,
                // The worker is gone (it panicked): start a fresh one below.
                Err(returned) => msg = returned.0,
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(msg);
        queues.insert(peer.clone(), tx);
        drop(queues);
        let engine = Arc::clone(self);
        tokio::spawn(async move { engine.peer_worker(peer, rx).await });
    }

    /// One peer's signaling, in order. Exits after a quiet minute (removing its
    /// queue under the map lock only if nothing arrived meanwhile).
    async fn peer_worker(
        self: Arc<Self>,
        peer: String,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<SignalMsg>,
    ) {
        const IDLE: Duration = Duration::from_secs(60);
        loop {
            match tokio::time::timeout(IDLE, rx.recv()).await {
                Ok(Some(msg)) => self.route(msg).await,
                Ok(None) => return,
                Err(_) => {
                    let mut queues = self.peer_queues.lock();
                    if rx.is_empty() {
                        queues.remove(&peer);
                        return;
                    }
                }
            }
        }
    }

    /// Drive one inbound signaling event to completion. Reached through
    /// [`Engine::dispatch_signaling`], which calls it for the events that
    /// concern no single peer (registration, server loss).
    pub async fn handle_signaling(&self, event: SigEvent) {
        match event {
            SigEvent::Registered => {
                tracing::info!("signaling: registered with server");
                self.connected.store(true, Ordering::Relaxed);
                let _ = self.ui.send(UiEvent::ServerStatus("connected"));
                // Fetch TURN relay credentials now so they're cached before the
                // user starts a session (needed for cross-network / strict NAT).
                let _ = self.signaling.send(SignalMsg::Turn);
            }
            SigEvent::RegisterError(reason) => {
                let _ = self
                    .ui
                    .send(UiEvent::Toast(format!("server rejected: {reason}")));
            }
            SigEvent::Disconnected => {
                self.connected.store(false, Ordering::Relaxed);
                let _ = self.ui.send(UiEvent::ServerStatus("reconnecting"));
            }
            SigEvent::Message(msg) => self.route(msg).await,
        }
    }

    /// Router mirroring signaling.js `route()` → host/viewer handlers.
    async fn route(&self, msg: SignalMsg) {
        match msg {
            SignalMsg::ConnectRequest {
                from: Some(from),
                password,
                caps,
                ..
            } => {
                // Only a first attempt carries caps (a password retry sends
                // none, and keeps what the first said); each first attempt
                // replaces what an earlier, possibly declined one said.
                if let Some(c) = &caps {
                    let features = session::ViewerFeatures {
                        popout: c.popout,
                        hstate: c.hstate,
                    };
                    self.viewer_features.lock().insert(from.clone(), features);
                }
                let decode = caps.map(|c| c.decode).unwrap_or_default();
                self.on_connect_request(from, password, decode).await;
            }
            SignalMsg::ConnectResponse {
                from: Some(from),
                accepted,
                permission,
                reason,
                codec,
                os,
                caps,
                ..
            } => {
                let host = sessions::HostTraits { codec, os, caps };
                self.on_connect_response(from, accepted, permission, reason, host)
                    .await;
            }
            SignalMsg::PasswordRequired {
                from: Some(from),
                nonce,
                ..
            } => {
                self.on_password_required(from, nonce).await;
            }
            SignalMsg::Signal {
                from: Some(from),
                data,
                ..
            } => {
                self.on_signal(from, data);
            }
            SignalMsg::EndSession {
                from: Some(from), ..
            } => {
                self.on_remote_end(from);
            }
            SignalMsg::RelayError { reason, to } if reason == "peer-offline" => {
                // Only the sessions with that peer end — not any id the relay
                // reports.
                self.on_peer_offline(to);
            }
            SignalMsg::TurnCredentials { ice_servers, error } => {
                self.on_turn_credentials(ice_servers, error);
            }
            _ => {} // contract §3: unknown/others ignored
        }
    }

    /// Cache TURN relay servers minted by the signaling server. Each URL becomes
    /// one `IceServer` sharing the username/credential; the pipeline gathers a
    /// relayed candidate from the `turn:` entries.
    fn on_turn_credentials(
        &self,
        ice_servers: Option<protocol::IceServers>,
        error: Option<String>,
    ) {
        if let Some(err) = error {
            tracing::warn!("TURN credentials unavailable: {err} (cross-network relay disabled)");
            return;
        }
        let Some(servers) = ice_servers else { return };
        let expanded: Vec<protocol::IceServer> = servers
            .urls
            .iter()
            .map(|url| protocol::IceServer {
                urls: url.clone(),
                username: servers.username.clone(),
                credential: servers.credential.clone(),
            })
            .collect();
        let turn_count = expanded
            .iter()
            .filter(|s| s.urls.starts_with("turn"))
            .count();
        tracing::info!("TURN credentials received: {turn_count} relay URL(s) cached");
        *self.turn_servers.lock() = expanded;
    }

    /// The cached TURN/STUN relay servers (from the signaling server).
    pub fn turn_servers(&self) -> Vec<protocol::IceServer> {
        self.turn_servers.lock().clone()
    }

    // ---- the person's data used by sessions ----------------------------------------

    /// UI action: clear the recent-ids list. Returns the (now empty) list.
    pub fn clear_recents(&self) -> Vec<String> {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.clear_recents(now);
        });
        Vec::new()
    }

    /// UI action: name, rename or (with an empty `name`) un-name a remote ID.
    /// Returns every name, so the UI can redraw from one source of truth.
    pub fn set_remote_name(
        &self,
        id: &str,
        name: &str,
    ) -> std::collections::BTreeMap<String, String> {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.set_remote_name(id, name, now);
            w.remote_names.clone()
        })
        .unwrap_or_default()
    }

    /// Record a connection to `peer` in the recents of the active workspace
    /// (none while signed out). Persisted, so recents survive a restart.
    pub(crate) fn add_recent(&self, peer: &str) {
        self.with_workspace(|w, s| {
            let now = s.now();
            w.add_recent(peer, now);
        });
    }

    /// Hand sealed values whose config entries are gone to the store's `forget`,
    /// off the runtime (a keyring may prompt); nothing waits for it.
    pub(crate) fn forget_sealed(&self, sealed: Vec<String>) {
        if sealed.is_empty() {
            return;
        }
        let store = self.platform.secrets();
        tokio::task::spawn_blocking(move || {
            for s in &sealed {
                store.forget(s);
            }
        });
    }

    /// UI action: forget every saved host password of the active workspace
    /// (other accounts' are untouched).
    pub fn clear_saved_logins(&self) {
        let all = self
            .with_workspace(|w, _| std::mem::take(&mut w.saved_logins).into_values().collect())
            .unwrap_or_default();
        self.forget_sealed(all);
    }

    // ---- platform & surface ------------------------------------------------

    /// The OS services this engine runs on.
    pub fn platform(&self) -> Arc<dyn Platform> {
        self.platform.clone()
    }

    pub fn set_ice_policy(&self, policy: IcePolicy) {
        *self.ice_policy.lock() = policy;
    }

    pub(crate) fn ice_policy(&self) -> IcePolicy {
        *self.ice_policy.lock()
    }

    pub(crate) fn translate_modifiers_handle(&self) -> Arc<AtomicBool> {
        self.translate_modifiers.clone()
    }

    // Accessors used by the session module.
    pub(crate) fn signaling(&self) -> &SignalingClient {
        &self.signaling
    }

    /// Where a session's transport thread reports that its link died (it
    /// holds no engine): the engine ends that session (see `start`).
    pub(crate) fn ended_sender(&self) -> tokio::sync::mpsc::UnboundedSender<EndRequest> {
        self.end_tx.clone()
    }
}

/// Loopback test hooks (the `test-hooks` feature). They expose the host's live
/// rate state and let a test drive a synthetic bandwidth estimate, so the
/// end-to-end congestion behaviour can be asserted over real str0m without a
/// network shaper. Never compiled into a shipping build.
#[cfg(feature = "test-hooks")]
impl Engine {
    fn with_host_session<R>(&self, f: impl FnOnce(&session::Session) -> Option<R>) -> Option<R> {
        self.reg
            .lock()
            .host
            .as_ref()
            .and_then(|h| h.session.as_ref())
            .and_then(f)
    }

    /// The host session's current encoder CBR target (bps), if a host session is
    /// live. This TRACKS the bandwidth estimate (clamped), it is not pinned.
    #[doc(hidden)]
    pub fn test_encoder_target_bps(&self) -> Option<u32> {
        self.with_host_session(|s| s.encoder_target_bps())
    }

    /// The host session's raw bandwidth estimate (bps).
    #[doc(hidden)]
    pub fn test_raw_link_bps(&self) -> Option<u32> {
        self.with_host_session(|s| s.raw_link_bps())
    }

    /// Whether the host session's viewer paused its main stream (P6).
    #[doc(hidden)]
    pub fn test_video_paused(&self) -> Option<bool> {
        self.with_host_session(|s| s.video_paused())
    }

    /// Encoded units the host session's main stream has sent.
    #[doc(hidden)]
    pub fn test_main_units(&self) -> Option<u64> {
        self.with_host_session(|s| s.main_units())
    }

    /// The host session's current encode size `(w, h)`.
    #[doc(hidden)]
    pub fn test_encode_dims(&self) -> Option<(u32, u32)> {
        self.with_host_session(|s| s.encode_dims())
    }

    /// Force the host's bandwidth estimate to `bps` (0 releases it back to the
    /// real str0m estimate). No effect on a viewer session.
    #[doc(hidden)]
    pub fn test_inject_bwe(&self, bps: u32) {
        self.with_host_session(|s| {
            s.inject_bwe(bps);
            Some(())
        });
    }

    /// Cut session `id`'s link as a dead network would: its transport declares
    /// it dead on its next pass and the session ends with `LinkLost`.
    #[doc(hidden)]
    pub fn test_sever(&self, id: SessionId) {
        let reg = self.reg.lock();
        let session = reg
            .host
            .as_ref()
            .filter(|h| h.id == id)
            .and_then(|h| h.session.as_ref())
            .or_else(|| reg.viewers.get(&id).and_then(|v| v.session.as_ref()));
        if let Some(s) = session {
            s.sever();
        }
    }

    /// Hand viewer session `id`'s most recent main-stream delta frames to its
    /// media loop once more: video that arrives later than the control
    /// channel, as on a slow link.
    #[doc(hidden)]
    pub fn test_replay_viewer_video(&self, id: SessionId) {
        let reg = self.reg.lock();
        if let Some(s) = reg.viewers.get(&id).and_then(|v| v.session.as_ref()) {
            s.replay_video();
        }
    }

    /// How many frames [`test_replay_viewer_video`](Self::test_replay_viewer_video)
    /// has handed on in session `id`.
    #[doc(hidden)]
    pub fn test_viewer_video_replayed(&self, id: SessionId) -> u32 {
        let reg = self.reg.lock();
        reg.viewers
            .get(&id)
            .and_then(|v| v.session.as_ref())
            .map_or(0, |s| s.replayed_video())
    }

    /// Make every viewer session's start wait `ms` before building anything,
    /// so a test can end it in the middle of its start.
    #[doc(hidden)]
    pub fn test_delay_viewer_start(&self, ms: u64) {
        self.test_start_delay_ms.store(ms, Ordering::SeqCst);
    }

    pub(crate) fn test_viewer_start_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.test_start_delay_ms.load(Ordering::SeqCst))
    }
}
