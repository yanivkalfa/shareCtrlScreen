//! Session orchestration (Plan 04 §4 `engine`). Ties the crates together and
//! runs the control plane: it consumes signaling events, drives the connection
//! handshake (contract §3.2, ported from `host.js`/`viewer.js`), enforces the
//! access model (approve popup / password challenge-response, live view↔control
//! switch), and owns the media session (capture→encode→transport on the host,
//! transport→decode→render on the viewer).
//!
//! The control plane here is portable and unit-tested; the media pipeline
//! ([`pipeline`]) is Windows-only and wires `capture`/`codec`/`render`/`input`.

pub mod handshake;
#[cfg(windows)]
pub mod pipeline;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use protocol::{Config, ControlMsg, Mode, Permission, SignalData, SignalMsg};
use signaling::{Event as SigEvent, SignalingClient};
use std::sync::Arc;

pub use handshake::{ApprovalDecision, Decider};

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
}

/// The engine. Owns config + the signaling client and the current role.
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
}

impl Engine {
    /// Load config, spawn signaling for our UUID, and return the engine plus the
    /// UI event stream. `decider` answers approve-mode popups (the app wires it
    /// to the WebView2 modal).
    pub fn start(
        config_path: PathBuf,
        decider: Arc<dyn Decider>,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
        tokio::sync::mpsc::UnboundedReceiver<SigEvent>,
    ) {
        let config = Config::load(&config_path);
        let (signaling, sig_rx) =
            SignalingClient::spawn(config.server_url.clone(), config.uuid.clone());
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
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
        let _ = cfg.persist(&self.config_path);
    }

    /// Drive one inbound signaling event. Call this from the app's event loop.
    pub async fn handle_signaling(&self, event: SigEvent) {
        match event {
            SigEvent::Registered => {
                tracing::info!("signaling: registered with server");
                self.connected.store(true, Ordering::Relaxed);
                let _ = self.ui.send(UiEvent::ServerStatus("connected"));
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
                ..
            } => {
                self.on_connect_response(from, accepted, permission, reason, codec)
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
                let _ = self.ui.send(UiEvent::Toast("Peer offline".into()));
                let _ = to;
                self.end_session_local();
            }
            _ => {} // contract §3: unknown/others ignored
        }
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
        #[cfg(windows)]
        let codec = pipeline::set_negotiated_codec_from_caps(&decode);
        #[cfg(not(windows))]
        let codec = {
            let _ = &decode;
            "H264".to_string()
        };

        let _ = self.signaling.send(SignalMsg::ConnectResponse {
            to: Some(peer.clone()),
            from: None,
            accepted: true,
            permission: Some(permission.as_str().into()),
            reason: None,
            codec: Some(codec),
        });
        *self.role.lock() = Role::Host {
            peer: peer.clone(),
            permission,
        };
        let _ = self.ui.send(UiEvent::RoleChanged(self.role.lock().clone()));
        // Persist, not just mutate in memory, so recents survive a restart.
        self.update_config(|c| c.add_recent(&peer));
        #[cfg(windows)]
        pipeline::begin_host(self, peer, permission);
    }

    // ---- Viewer side (contract §3.2) -------------------------------------

    /// UI action: connect out to `host_uuid`. Sends the first connect-request.
    pub fn connect_to(&self, host_uuid: String) {
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host_uuid.clone()),
            from: None,
            password: None,
            caps: Some(protocol::signaling::Caps {
                decode: vec!["H264".into(), "HEVC".into(), "AV1".into()],
            }),
        });
        self.update_config(|c| c.add_recent(&host_uuid));
    }

    /// UI action: clear the recent-ids list. Returns the (now empty) list.
    pub fn clear_recents(&self) -> Vec<String> {
        self.update_config(|c| c.recent_ids.clear());
        Vec::new()
    }

    async fn on_password_required(&self, from: String, nonce: String) {
        // Stash the nonce, then let the UI collect the plaintext; the app calls
        // `submit_password` with the answer.
        self.pending_nonce.lock().insert(from.clone(), nonce);
        let _ = self.ui.send(UiEvent::PasswordRequired { from });
    }

    /// UI action: answer a `password-required` prompt (viewer side).
    pub fn submit_password(&self, host: String, plaintext: String) {
        let nonce = self.pending_nonce.lock().remove(&host).unwrap_or_default();
        // proof = SHA256( SHA256(plaintext) + ":" + nonce ).
        let proof = handshake::compute_proof(&plaintext, &nonce);
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host),
            from: None,
            password: Some(proof),
            caps: None,
        });
    }

    async fn on_connect_response(
        &self,
        from: String,
        accepted: bool,
        permission: Option<String>,
        reason: Option<String>,
        codec: Option<String>,
    ) {
        if !accepted {
            let r = reason.unwrap_or_else(|| "denied".into());
            let _ = self.ui.send(UiEvent::Toast(format!("connection {r}")));
            return;
        }
        // The host told us which codec it will stream; decode with the same (§3).
        #[cfg(windows)]
        pipeline::set_codec_from_str(codec.as_deref().unwrap_or("H264"));
        #[cfg(not(windows))]
        let _ = &codec;
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
        #[cfg(windows)]
        pipeline::begin_viewer(self, from, perm);
    }

    /// WebRTC negotiation payloads (offer/answer/ICE) — routed to the transport.
    async fn on_signal(&self, from: String, data: SignalData) {
        #[cfg(windows)]
        pipeline::on_signal(self, &from, data);
        #[cfg(not(windows))]
        {
            let _ = (from, data);
        }
    }

    // ---- Session lifecycle -----------------------------------------------

    /// Host action: switch the live permission (view↔control) mid-session.
    pub fn set_permission(&self, permission: Permission) {
        let mut role = self.role.lock();
        if let Role::Host { peer, .. } = &*role {
            let peer = peer.clone();
            *role = Role::Host { peer, permission };
            drop(role);
            #[cfg(windows)]
            {
                // Update the host injection gate, then notify the viewer (§4.2).
                pipeline::set_control(permission == Permission::Control);
                pipeline::send_ctl(self, &ControlMsg::Perm { value: permission });
            }
        }
    }

    /// End the session locally and tell the peer (best-effort, contract §3.2 s5).
    pub fn end_session(&self) {
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
        #[cfg(windows)]
        pipeline::teardown(self);
        *self.role.lock() = Role::Idle;
        let _ = self.ui.send(UiEvent::RoleChanged(Role::Idle));
    }

    // Accessors used by the pipeline module.
    pub(crate) fn signaling(&self) -> &SignalingClient {
        &self.signaling
    }
}
