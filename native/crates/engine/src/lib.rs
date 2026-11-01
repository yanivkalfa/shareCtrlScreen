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

pub mod handshake;
mod media;
mod modifiers;
mod session;
pub mod transfer;
mod tuning;
pub mod turn;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use platform_api::{Codec, Platform, VideoSurface};
use protocol::{Config, ControlMsg, Mode, Permission, SignalData, SignalMsg};
use signaling::{Event as SigEvent, SignalingClient};
use std::sync::Arc;
use std::time::Duration;

pub use handshake::{ApprovalDecision, Decider};

/// How long auto-login waits for the OS secret store to open a saved password
/// before asking the user instead. Long enough to answer a keyring unlock
/// prompt, short enough that a wedged store does not leave the connect hanging.
const SECRET_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Which side of a session this instance is currently playing. A machine
/// supports one active session at a time (contract §1 v1 simplification).
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

/// Events the engine surfaces to the app UI (WebView2 via Tauri events).
#[derive(Debug, Clone)]
pub enum UiEvent {
    ServerStatus(&'static str),
    /// A viewer wants in (approve mode) — the UI shows the popup and calls back.
    ApprovalRequest {
        from: String,
    },
    /// Viewer side: the host demands a password; the UI opens the prompt and
    /// answers with [`Engine::submit_password`].
    PasswordRequired {
        from: String,
    },
    /// The active session changed.
    RoleChanged(Role),
    /// A transient user-facing message.
    Toast(String),
    /// The peer has gone quiet but the session is not given up on yet.
    /// `secs_left` counts down to the point where it is dropped. Emitted about
    /// once a second so the UI can show a live countdown instead of leaving the
    /// user staring at a frozen picture wondering whether it is still alive.
    LinkTrouble {
        secs_left: u32,
    },
    /// The peer answered again — the session continues normally.
    LinkRestored,
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

/// The engine. Owns config, the signaling client, the current role and session.
pub struct Engine {
    config_path: PathBuf,
    config: Arc<Mutex<Config>>,
    signaling: SignalingClient,
    role: Arc<Mutex<Role>>,
    /// Whether the signaling socket is currently registered with the server. The
    /// UI queries this on boot because the one-shot `ServerStatus` event can fire
    /// before the WebView subscribes (startup race — the dot would stay stuck).
    connected: Arc<AtomicBool>,
    decider: Arc<dyn Decider>,
    ui: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// Pending host password nonces per requesting peer (approve/challenge).
    pending_nonce: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Viewer decode capabilities per requesting peer, for codec negotiation (§3).
    pending_caps: Arc<Mutex<std::collections::HashMap<String, Vec<String>>>>,
    /// TURN relay servers minted by the signaling server (Cloudflare TURN),
    /// expanded to one `IceServer` per URL. Fetched on register and cached; the
    /// pipeline reads them when gathering the relayed candidate.
    turn_servers: Arc<Mutex<Vec<protocol::IceServer>>>,
    /// Viewer: the password just submitted, held until the host accepts it.
    /// Only then is it worth saving — a rejected guess must never be persisted.
    /// `(host, plaintext, remember)`.
    pending_login: Arc<Mutex<Option<(String, String, bool)>>>,
    /// Viewer: the host we last auto-submitted a saved password to. If that host
    /// challenges us again, the saved credential is stale — drop it and fall back
    /// to prompting, instead of looping on a password that no longer works.
    auto_attempted: Arc<Mutex<Option<String>>>,
    /// Viewer: the host we sent a connect-request to and are waiting on. A
    /// connect-response from anyone else is ignored — otherwise any peer that
    /// knows our id could make us start a viewer session with it.
    outbound: Mutex<Option<String>>,
    /// OS services.
    platform: Arc<dyn Platform>,
    /// The viewer's native video surface, attached by the app once its window
    /// exists (`None` until then, and on platforms without one).
    surface: Mutex<Option<Arc<dyn VideoSurface>>>,
    /// The codec of the current/next session: chosen by the host at accept time,
    /// adopted by the viewer from the connect-response.
    negotiated: Mutex<Codec>,
    /// Viewer: the host's OS from its connect-response (unknown when it did not
    /// say). It arrives before the host's offer, so every viewer session knows
    /// it before its first key event.
    peer_os: Mutex<modifiers::OsFamily>,
    /// The live "Translate modifiers for the remote OS" setting, shared with
    /// the running session's key translator so a change applies to the next
    /// key without a reconnect. Kept equal to `config.translate_modifiers` by
    /// [`Engine::update_config`].
    translate_modifiers: Arc<AtomicBool>,
    /// The live session, if any (one at a time).
    pub(crate) session: Mutex<Option<session::Session>>,
    ice_policy: Mutex<IcePolicy>,
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
        let config = Config::load(&config_path);
        let (signaling, sig_rx) =
            SignalingClient::spawn(config.server_url.clone(), config.uuid.clone());
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let translate_modifiers = Arc::new(AtomicBool::new(config.translate_modifiers));
        let engine = Arc::new(Self {
            config_path,
            config: Arc::new(Mutex::new(config)),
            signaling,
            role: Arc::new(Mutex::new(Role::Idle)),
            connected: Arc::new(AtomicBool::new(false)),
            decider,
            ui: ui_tx,
            pending_nonce: Arc::new(Mutex::new(Default::default())),
            pending_caps: Arc::new(Mutex::new(Default::default())),
            turn_servers: Arc::new(Mutex::new(Vec::new())),
            pending_login: Arc::new(Mutex::new(None)),
            auto_attempted: Arc::new(Mutex::new(None)),
            outbound: Mutex::new(None),
            platform,
            surface: Mutex::new(None),
            negotiated: Mutex::new(Codec::H264),
            peer_os: Mutex::new(modifiers::OsFamily::Unknown),
            translate_modifiers,
            session: Mutex::new(None),
            ice_policy: Mutex::new(IcePolicy::Full),
        });
        (engine, ui_rx, sig_rx)
    }

    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    pub fn role(&self) -> Role {
        self.role.lock().clone()
    }

    /// Whether signaling is currently registered with the server. Queried by the
    /// UI on boot to back-fill the status dot past the startup event race.
    pub fn server_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Persist a config mutation (settings changes from the UI).
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.config.lock();
        f(&mut cfg);
        self.translate_modifiers
            .store(cfg.translate_modifiers, Ordering::Relaxed);
        let _ = cfg.persist(&self.config_path);
    }

    /// Drive one inbound signaling event. Call this from the app's event loop.
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
                ..
            } => {
                self.on_connect_response(from, accepted, permission, reason, codec, os)
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
                self.on_signal(from, data).await;
            }
            SignalMsg::EndSession {
                from: Some(from), ..
            } => {
                self.on_remote_end(from);
            }
            SignalMsg::RelayError { reason, to } if reason == "peer-offline" => {
                // Only react when the offline peer is the one we are in (or
                // trying to start) a session with — not any id the relay reports.
                let ours = match to.as_deref() {
                    Some(id) => self.is_session_peer(id),
                    None => true, // older servers omit `to`
                };
                if ours {
                    let _ = self.ui.send(UiEvent::Toast("Peer offline".into()));
                    self.end_session_local();
                }
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

    // ---- Host side (contract §3.2 step 2) --------------------------------

    async fn on_connect_request(
        &self,
        from: String,
        password: Option<String>,
        decode_caps: Vec<String>,
    ) {
        // Busy: one session at a time.
        if !matches!(*self.role.lock(), Role::Idle) {
            let _ = self.signaling.send(SignalMsg::ConnectResponse {
                to: Some(from),
                from: None,
                accepted: false,
                permission: None,
                reason: Some("busy".into()),
                codec: None,
                os: None,
            });
            return;
        }

        // Remember the viewer's decode caps for codec negotiation at accept time.
        // Only the first attempt carries caps; a password retry sends none, so
        // don't clobber the stored list with an empty one.
        if !decode_caps.is_empty() {
            self.pending_caps.lock().insert(from.clone(), decode_caps);
        }

        let cfg = self.config.lock().clone();
        match cfg.effective_mode() {
            Mode::Password => self.handle_password_request(from, password, &cfg).await,
            Mode::Approve => self.handle_approve_request(from).await,
        }
    }

    async fn handle_password_request(&self, from: String, password: Option<String>, cfg: &Config) {
        match password {
            None => {
                // Issue a fresh nonce (contract §3.2).
                let nonce = handshake::random_nonce();
                self.pending_nonce
                    .lock()
                    .insert(from.clone(), nonce.clone());
                let _ = self.signaling.send(SignalMsg::PasswordRequired {
                    to: Some(from),
                    from: None,
                    nonce,
                });
            }
            Some(proof) => {
                let nonce = self.pending_nonce.lock().remove(&from).unwrap_or_default();
                if cfg.verify_proof(&nonce, &proof) {
                    self.accept(from, cfg.password_permission).await;
                } else {
                    // §6 security: 2 s artificial delay before rejecting.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let _ = self.signaling.send(SignalMsg::ConnectResponse {
                        to: Some(from),
                        from: None,
                        accepted: false,
                        permission: None,
                        reason: Some("bad-password".into()),
                        codec: None,
                        os: None,
                    });
                }
            }
        }
    }

    async fn handle_approve_request(&self, from: String) {
        let _ = self
            .ui
            .send(UiEvent::ApprovalRequest { from: from.clone() });
        match self.decider.decide(&from).await {
            ApprovalDecision::Deny => {
                let _ = self.signaling.send(SignalMsg::ConnectResponse {
                    to: Some(from),
                    from: None,
                    accepted: false,
                    permission: None,
                    reason: Some("denied".into()),
                    codec: None,
                    os: None,
                });
            }
            ApprovalDecision::Allow(permission) => self.accept(from, permission).await,
        }
    }

    /// Accept a viewer with `permission`, become Host, and begin negotiation as
    /// the offerer (the host owns the media, contract §3.2 step 4).
    async fn accept(&self, peer: String, permission: Permission) {
        // Codec negotiation from the viewer's advertised caps (§3) — done before
        // the response so the viewer is told which codec to decode with.
        let decode = self.pending_caps.lock().remove(&peer).unwrap_or_default();
        let viewer: Vec<Codec> = decode
            .iter()
            .filter_map(|s| Codec::from_caps_str(s))
            .collect();
        let chosen = Codec::negotiate(&self.platform.codecs().host_encodable(), &viewer);
        *self.negotiated.lock() = chosen;
        tracing::info!("negotiated codec: {}", chosen.as_caps_str());
        let codec = chosen.as_caps_str().to_string();

        let _ = self.signaling.send(SignalMsg::ConnectResponse {
            to: Some(peer.clone()),
            from: None,
            accepted: true,
            permission: Some(permission.as_str().into()),
            reason: None,
            codec: Some(codec),
            // The viewer translates modifier keys for this OS (D8).
            os: modifiers::OsFamily::from_name(self.platform.info().name)
                .wire_name()
                .map(String::from),
        });
        *self.role.lock() = Role::Host {
            peer: peer.clone(),
            permission,
        };
        let _ = self.ui.send(UiEvent::RoleChanged(self.role.lock().clone()));
        // Persist, not just mutate in memory, so recents survive a restart.
        self.update_config(|c| c.add_recent(&peer));
        session::begin_host(self, peer, permission);
    }

    // ---- Viewer side (contract §3.2) -------------------------------------

    /// UI action: connect out to `host_uuid`. Sends the first connect-request.
    pub fn connect_to(&self, host_uuid: String) {
        // Advertise only codecs this viewer can actually hardware-decode. A
        // hardcoded list (e.g. always claiming AV1) makes the host negotiate a
        // codec we cannot decode → the viewer media loop dies → black screen.
        let decode: Vec<String> = self
            .platform
            .codecs()
            .viewer_decodable()
            .iter()
            .map(|c| c.as_caps_str().to_string())
            .collect();
        *self.outbound.lock() = Some(host_uuid.clone());
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host_uuid.clone()),
            from: None,
            password: None,
            caps: Some(protocol::signaling::Caps { decode }),
        });
        self.update_config(|c| c.add_recent(&host_uuid));
    }

    /// UI action: clear the recent-ids list. Returns the (now empty) list.
    pub fn clear_recents(&self) -> Vec<String> {
        self.update_config(|c| c.recent_ids.clear());
        Vec::new()
    }

    async fn on_password_required(&self, from: String, nonce: String) {
        self.pending_nonce.lock().insert(from.clone(), nonce);

        // A second challenge from a host we just auto-submitted to means the
        // saved password is no longer valid. Forget it and prompt normally,
        // rather than retrying a dead credential forever.
        let stale = self.auto_attempted.lock().as_deref() == Some(from.as_str());
        if stale {
            *self.auto_attempted.lock() = None;
            self.forget_saved_login(&from);
            let _ = self
                .ui
                .send(UiEvent::Toast("Saved password no longer works".into()));
            let _ = self.ui.send(UiEvent::PasswordRequired { from });
            return;
        }

        // Auto-login: replay a password this host has already accepted.
        if let Some(plain) = self.saved_login(&from).await {
            *self.auto_attempted.lock() = Some(from.clone());
            tracing::info!("auto-login: submitting saved password for {from}");
            self.submit_password(from, plain, false);
            return;
        }
        let _ = self.ui.send(UiEvent::PasswordRequired { from });
    }

    /// The stored password for `host`, if auto-login is on and we can decrypt it.
    ///
    /// `open` runs off the runtime and is bounded: a locked OS keyring shows an
    /// unlock prompt and waits for the user, and this is called from the
    /// signaling loop, which must keep serving (the host side of this same
    /// process, TURN, further requests). No answer in time means "no saved
    /// password" and the user is asked normally.
    async fn saved_login(&self, host: &str) -> Option<String> {
        let sealed = {
            let cfg = self.config.lock();
            if !cfg.auto_login {
                return None;
            }
            cfg.saved_logins.get(host)?.clone()
        };
        let store = self.platform.secrets();
        let open = tokio::task::spawn_blocking(move || store.open(&sealed));
        match tokio::time::timeout(SECRET_PROMPT_TIMEOUT, open).await {
            Ok(Ok(plain)) => plain,
            Ok(Err(e)) => {
                tracing::warn!("auto-login: secret store failed while opening: {e}");
                None
            }
            Err(_) => {
                tracing::warn!(
                    "auto-login: the secret store gave no answer in {}s (an OS unlock prompt?) \
                     — asking for the password instead",
                    SECRET_PROMPT_TIMEOUT.as_secs()
                );
                None
            }
        }
    }

    /// Remove `host`'s saved password and let the OS store drop its copy.
    fn forget_saved_login(&self, host: &str) {
        let mut removed = None;
        self.update_config(|c| removed = c.saved_logins.remove(host));
        self.forget_sealed(removed.into_iter().collect());
    }

    /// Hand sealed values whose config entries are gone to the store's `forget`,
    /// off the runtime (a keyring may prompt); nothing waits for it.
    fn forget_sealed(&self, sealed: Vec<String>) {
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

    /// Seal `plaintext` and save it for `host`, detached from the caller: the
    /// host has just accepted the password, and `seal` may sit behind an OS
    /// unlock prompt that must not stall the signaling loop. Reported through
    /// the log and a toast, as before.
    fn save_login(&self, host: String, plaintext: String) {
        let store = self.platform.secrets();
        let config = Arc::clone(&self.config);
        let config_path = self.config_path.clone();
        let ui = self.ui.clone();
        tokio::spawn(async move {
            let seal = {
                let store = Arc::clone(&store);
                tokio::task::spawn_blocking(move || store.seal(&plaintext))
            };
            let sealed = match seal.await {
                Ok(Some(sealed)) => sealed,
                Ok(None) => {
                    tracing::warn!("auto-login: could not encrypt password — not saving");
                    let _ = ui.send(UiEvent::Toast(
                        "Could not save the password securely".into(),
                    ));
                    return;
                }
                Err(e) => {
                    tracing::warn!("auto-login: secret store failed while sealing: {e}");
                    let _ = ui.send(UiEvent::Toast(
                        "Could not save the password securely".into(),
                    ));
                    return;
                }
            };
            let replaced = {
                let mut cfg = config.lock();
                cfg.auto_login = true;
                let old = cfg.saved_logins.insert(host.clone(), sealed);
                let _ = cfg.persist(&config_path);
                old
            };
            tracing::info!("auto-login: saved password for {host}");
            // The previous entry for this host is unreachable now; let the OS
            // store release it too.
            if let Some(old) = replaced {
                tokio::task::spawn_blocking(move || store.forget(&old));
            }
        });
    }

    /// UI action: answer a `password-required` prompt (viewer side). `remember`
    /// asks to save the password for auto-login — but only once the host has
    /// actually accepted it (see `on_connect_response`).
    pub fn submit_password(&self, host: String, plaintext: String, remember: bool) {
        let nonce = self.pending_nonce.lock().remove(&host).unwrap_or_default();
        // proof = SHA256( SHA256(plaintext) + ":" + nonce ).
        let proof = handshake::compute_proof(&plaintext, &nonce);
        *self.pending_login.lock() = Some((host.clone(), plaintext, remember));
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host),
            from: None,
            password: Some(proof),
            caps: None,
        });
    }

    /// UI action: forget every saved auto-login password.
    pub fn clear_saved_logins(&self) {
        let mut all = Vec::new();
        self.update_config(|c| {
            all = std::mem::take(&mut c.saved_logins).into_values().collect();
        });
        self.forget_sealed(all);
    }

    async fn on_connect_response(
        &self,
        from: String,
        accepted: bool,
        permission: Option<String>,
        reason: Option<String>,
        codec: Option<String>,
        os: Option<String>,
    ) {
        // Only the host we asked may answer, and only once.
        {
            let mut outbound = self.outbound.lock();
            if outbound.as_deref() != Some(from.as_str()) {
                tracing::warn!("ignoring a connect-response from {from}: we did not ask it");
                return;
            }
            *outbound = None;
        }
        if !accepted {
            *self.pending_login.lock() = None;
            let auto = self.auto_attempted.lock().take();
            let r = reason.unwrap_or_else(|| "denied".into());
            // A rejected password is reported as a plain refusal, not a second
            // challenge — so this is the ONLY place a stale saved credential can
            // be detected. Without dropping it here, auto-login would keep
            // replaying a password the host no longer accepts and the prompt
            // would never reappear: a permanent lockout from that host.
            if r == "bad-password" && auto.as_deref() == Some(from.as_str()) {
                self.forget_saved_login(&from);
                let _ = self.ui.send(UiEvent::Toast(
                    "Saved password was rejected — forgotten, please connect again".into(),
                ));
                return;
            }
            let _ = self.ui.send(UiEvent::Toast(format!("connection {r}")));
            return;
        }
        // The host accepted us, so any password we just sent is known-good — this
        // is the only point at which it's worth saving. Sealed by the platform's
        // secret store; if that fails we simply don't save (never plaintext).
        *self.auto_attempted.lock() = None;
        // Taken into a local first: the lock must not be held across the save
        // (`submit_password` on the UI thread takes it too).
        let pending = self.pending_login.lock().take();
        if let Some((host, plaintext, remember)) = pending {
            if remember && host == from {
                self.save_login(host, plaintext);
            }
        }
        // The host told us which codec it will stream; decode with the same (§3).
        *self.negotiated.lock() = codec
            .as_deref()
            .and_then(Codec::from_caps_str)
            .unwrap_or(Codec::H264);
        // And which OS it runs, for modifier translation; absent = unknown =
        // keys forwarded literally.
        let host_os = os
            .as_deref()
            .map_or(modifiers::OsFamily::Unknown, modifiers::OsFamily::from_name);
        tracing::info!("host OS: {host_os:?}");
        *self.peer_os.lock() = host_os;
        let perm = permission
            .as_deref()
            .map(|p| match p {
                "control" => Permission::Control,
                _ => Permission::View,
            })
            .unwrap_or(Permission::View);
        *self.role.lock() = Role::Viewer {
            peer: from.clone(),
            permission: perm,
        };
        let _ = self.ui.send(UiEvent::RoleChanged(self.role.lock().clone()));
        tracing::info!("viewer session with {from}; awaiting the host's offer");
    }

    /// WebRTC negotiation payloads (offer/answer/ICE) — routed to the transport.
    async fn on_signal(&self, from: String, data: SignalData) {
        session::on_signal(self, &from, data);
    }

    // ---- Session lifecycle -----------------------------------------------

    /// Host action: switch the live permission (view↔control) mid-session.
    pub fn set_permission(&self, permission: Permission) {
        let mut role = self.role.lock();
        if let Role::Host { peer, .. } = &*role {
            let peer = peer.clone();
            *role = Role::Host { peer, permission };
            drop(role);
            // Update the host injection gate, then notify the viewer (§4.2).
            session::set_control(self, permission == Permission::Control);
            session::send_ctl(self, &ControlMsg::Perm { value: permission });
        }
    }

    /// Viewer action: ask the host for a fresh keyframe ("Refresh" button — e.g.
    /// to clear residual smear after packet loss).
    pub fn request_refresh(&self) {
        if matches!(&*self.role.lock(), Role::Viewer { .. }) {
            session::send_ctl(self, &ControlMsg::KeyframeRequest);
        }
    }

    /// End the session locally and tell the peer (best-effort, contract §3.2 s5).
    pub fn end_session(&self) {
        // Also cancels a connect-request that has not been answered yet.
        *self.outbound.lock() = None;
        let peer = match &*self.role.lock() {
            Role::Host { peer, .. } | Role::Viewer { peer, .. } => Some(peer.clone()),
            Role::Idle => None,
        };
        if let Some(peer) = peer {
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(peer),
                from: None,
            });
        }
        self.end_session_local();
    }

    fn on_remote_end(&self, from: String) {
        let is_peer = matches!(
            &*self.role.lock(),
            Role::Host { peer, .. } | Role::Viewer { peer, .. } if *peer == from
        );
        if is_peer {
            self.end_session_local();
        }
    }

    fn end_session_local(&self) {
        *self.outbound.lock() = None;
        session::teardown(self);
        *self.role.lock() = Role::Idle;
        let _ = self.ui.send(UiEvent::RoleChanged(Role::Idle));
    }

    /// Whether `id` is the peer of the current session or of our pending
    /// connect-request.
    fn is_session_peer(&self, id: &str) -> bool {
        let in_session = matches!(
            &*self.role.lock(),
            Role::Host { peer, .. } | Role::Viewer { peer, .. } if peer == id
        );
        in_session || self.outbound.lock().as_deref() == Some(id)
    }

    /// End the current session because it could not be started or kept going,
    /// telling the user why.
    pub(crate) fn fail_session(&self, why: &str) {
        let _ = self
            .ui
            .send(UiEvent::Toast(format!("Session ended: {why}")));
        self.end_session();
    }

    // ---- platform & surface ------------------------------------------------

    /// The OS services this engine runs on.
    pub fn platform(&self) -> Arc<dyn Platform> {
        self.platform.clone()
    }

    /// Attach the viewer's video surface (the app creates it under its window).
    pub fn attach_surface(&self, surface: Arc<dyn VideoSurface>) {
        *self.surface.lock() = Some(surface);
    }

    pub(crate) fn surface(&self) -> Option<Arc<dyn VideoSurface>> {
        self.surface.lock().clone()
    }

    /// Viewer: hide/show the native video so web modals (settings) are visible
    /// mid-session. The stream keeps running; only presentation is hidden.
    pub fn set_video_visible(&self, visible: bool) {
        if let Some(s) = self.surface() {
            if visible {
                s.show();
            } else {
                s.hide();
            }
        }
    }

    pub fn set_ice_policy(&self, policy: IcePolicy) {
        *self.ice_policy.lock() = policy;
    }

    pub(crate) fn ice_policy(&self) -> IcePolicy {
        *self.ice_policy.lock()
    }

    pub(crate) fn negotiated_codec(&self) -> Codec {
        *self.negotiated.lock()
    }

    pub(crate) fn peer_os(&self) -> modifiers::OsFamily {
        *self.peer_os.lock()
    }

    pub(crate) fn translate_modifiers_handle(&self) -> Arc<AtomicBool> {
        self.translate_modifiers.clone()
    }

    // Accessors used by the session module.
    pub(crate) fn signaling(&self) -> &SignalingClient {
        &self.signaling
    }

    /// Clone of the UI event sender, so the transport thread can notify the UI
    /// directly when the connection dies (it doesn't hold `&Engine`).
    pub(crate) fn ui_sender(&self) -> tokio::sync::mpsc::UnboundedSender<UiEvent> {
        self.ui.clone()
    }

    /// Handle to the current role, so the transport thread can reset it to Idle
    /// on an unexpected disconnect.
    pub(crate) fn role_handle(&self) -> Arc<Mutex<Role>> {
        self.role.clone()
    }
}
