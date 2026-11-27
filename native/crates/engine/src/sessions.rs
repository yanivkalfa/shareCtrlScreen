//! Session lifecycle on both sides (contract §3.2, plans/08 P2, 08a §5.2,
//! §6.2–§6.6, §6.15): admitting a viewer and hosting it, connecting out to
//! hosts as any number of viewer sessions, the password exchange, routing the
//! WebRTC payloads, and ending sessions — each one on its own.
//!
//! **One session at a time, both ways (D1, I3).** Hosting (or letting a viewer
//! in) and viewing never overlap: every check and claim happens under the
//! registry lock (`reg`), after the workspace lock (`ws`) where both are
//! needed, so an incoming request and an outgoing connect cannot both pass.
//!
//! **Ending.** [`Engine::end`] takes a session out of the registry, detaches
//! its surface at once (input stops, the video is hidden), emits
//! `Phase(Ended)`, then stops and joins its threads on a reaper thread and
//! emits `Released` — only then may the app destroy the surface (08a §6.5).
//! The transport thread reports a dead link through the engine's ended channel
//! (`end_tx`), never by touching engine state itself.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use platform_api::{Codec, VideoSurface};
use protocol::{Config, ControlMsg, Mode, Permission, SignalData, SignalMsg};

use crate::events::{
    EndReason, PwReason, SessionEvent, SessionId, SessionPhase, UiEvent, WindowId,
};
use crate::handshake::{self, ApprovalDecision};
use crate::session::registry::{HostSlot, PendingLogin, ViewerPhase, ViewerSlot, REQUEST_TIMEOUT};
use crate::{modifiers, session, ConnectError, Engine, MoveError, Role, SECRET_PROMPT_TIMEOUT};

/// Holds `Registry::admitting` for one peer and clears it when dropped, so an
/// admission that ends any way (accepted, denied, a wrong password, a panic)
/// never leaves this computer stuck "busy".
struct Admission<'a> {
    engine: &'a Engine,
    peer: String,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let mut reg = self.engine.reg.lock();
        if reg.admitting.as_deref() == Some(self.peer.as_str()) {
            reg.admitting = None;
        }
    }
}

/// What a host says about itself in an accepting connect-response.
pub(crate) struct HostTraits {
    /// The codec it will stream (absent: H.264).
    pub(crate) codec: Option<String>,
    /// Its OS family, for modifier translation (absent: unknown).
    pub(crate) os: Option<String>,
    /// Its capabilities beyond the base contract.
    pub(crate) caps: Option<protocol::HostCaps>,
}

fn refuse(engine: &Engine, to: String, reason: &str) {
    let _ = engine.signaling.send(SignalMsg::ConnectResponse {
        to: Some(to),
        from: None,
        accepted: false,
        permission: None,
        reason: Some(reason.into()),
        codec: None,
        os: None,
        caps: None,
    });
}

impl Engine {
    // ---- events ------------------------------------------------------------------

    fn emit(&self, id: SessionId, peer: &str, host: bool, ev: SessionEvent) {
        let _ = self.ui.send(UiEvent::Session {
            id,
            peer: peer.to_string(),
            host,
            ev,
        });
    }

    /// The UI side of session `id`, for its threads.
    pub(crate) fn session_ui(&self, id: SessionId, peer: &str, host: bool) -> crate::SessionUi {
        crate::SessionUi {
            id,
            peer: peer.to_string(),
            host,
            tx: self.ui.clone(),
        }
    }

    // ---- what is running ---------------------------------------------------------

    /// What this computer is doing, as one role: hosting, else viewing (the
    /// first viewer session past its request), else idle. A summary for the
    /// UI and tests; sessions are told apart by their [`SessionId`].
    pub fn role(&self) -> Role {
        let reg = self.reg.lock();
        if let Some(h) = &reg.host {
            return Role::Host {
                peer: h.peer.clone(),
                permission: h.permission,
            };
        }
        reg.viewers
            .values()
            .find(|v| matches!(v.phase, ViewerPhase::Negotiating | ViewerPhase::Live))
            .map_or(Role::Idle, |v| Role::Viewer {
                peer: v.peer.clone(),
                permission: v.permission,
            })
    }

    /// The viewer sessions, oldest first.
    pub fn viewer_sessions(&self) -> Vec<SessionId> {
        self.reg.lock().viewers.keys().copied().collect()
    }

    /// The host session, if this computer is being viewed or controlled.
    pub fn host_session(&self) -> Option<SessionId> {
        self.reg.lock().host.as_ref().map(|h| h.id)
    }

    /// The computer at the other end of session `id`.
    pub fn session_peer(&self, id: SessionId) -> Option<String> {
        let reg = self.reg.lock();
        reg.host
            .as_ref()
            .filter(|h| h.id == id)
            .map(|h| h.peer.clone())
            .or_else(|| reg.viewers.get(&id).map(|v| v.peer.clone()))
    }

    // ---- host side (contract §3.2 step 2) ------------------------------------------

    pub(crate) async fn on_connect_request(
        &self,
        from: String,
        password: Option<String>,
        decode_caps: Vec<String>,
    ) {
        let cfg = self.config.lock().clone();
        let mode = cfg.effective_mode();
        // An approval prompt, or a password proof answering a challenge we
        // issued, is an admission in progress. A bare first password request
        // (we only send a challenge) is not — the viewer may never answer it —
        // and neither is a proof with no challenge behind it: it cannot succeed,
        // and letting it claim the admission would let any peer that knows our
        // ID keep this computer "busy" by streaming bad proofs.
        let admits = match mode {
            Mode::Approve => true,
            Mode::Password => password.is_some() && self.pending_nonce.lock().contains_key(&from),
        };

        // Busy: one session at a time, and one admission at a time (D1).
        // Checked and claimed in one critical section, because each peer is
        // handled on its own task.
        let busy = {
            let mut reg = self.reg.lock();
            let busy = reg.busy_for(&from);
            if !busy && admits {
                reg.admitting = Some(from.clone());
            }
            busy
        };
        if busy {
            refuse(self, from, "busy");
            return;
        }
        // Released however the admission ends (accepted, denied, wrong password).
        // On accept the host session exists before this drops, so there is no gap.
        let admission = admits.then(|| Admission {
            engine: self,
            peer: from.clone(),
        });

        // Remember the viewer's decode caps for codec negotiation at accept time.
        // Only the first attempt carries caps; a password retry sends none, so
        // don't clobber the stored list with an empty one.
        if !decode_caps.is_empty() {
            self.pending_caps.lock().insert(from.clone(), decode_caps);
        }

        match mode {
            Mode::Password => {
                self.handle_password_request(from, password, &cfg, admission)
                    .await
            }
            Mode::Approve => {
                self.handle_approve_request(from).await;
                drop(admission);
            }
        }
    }

    /// `admission` is held while the proof is checked and through `accept`; a
    /// wrong proof releases it before the anti-guessing delay, which slows
    /// that one peer down but must not keep everyone else out.
    async fn handle_password_request(
        &self,
        from: String,
        password: Option<String>,
        cfg: &Config,
        admission: Option<Admission<'_>>,
    ) {
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
                if admission.is_some() && cfg.verify_proof(&nonce, &proof) {
                    self.accept(from, cfg.password_permission).await;
                    drop(admission);
                } else {
                    drop(admission);
                    // §6 security: 2 s artificial delay before rejecting.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    refuse(self, from, "bad-password");
                }
            }
        }
    }

    async fn handle_approve_request(&self, from: String) {
        let _ = self
            .ui
            .send(UiEvent::ApprovalRequest { from: from.clone() });
        match self.decider.decide(&from).await {
            ApprovalDecision::Deny => refuse(self, from, "denied"),
            ApprovalDecision::Allow(permission) => self.accept(from, permission).await,
        }
    }

    /// Accept a viewer with `permission`, start hosting, and begin negotiation
    /// as the offerer (the host owns the media, contract §3.2 step 4).
    async fn accept(&self, peer: String, permission: Permission) {
        // Codec negotiation from the viewer's advertised caps (§3) — done before
        // the response so the viewer is told which codec to decode with.
        let decode = self.pending_caps.lock().remove(&peer).unwrap_or_default();
        let viewer: Vec<Codec> = decode
            .iter()
            .filter_map(|s| Codec::from_caps_str(s))
            .collect();
        let codec = Codec::negotiate(&self.platform.codecs().host_encodable(), &viewer);
        tracing::info!("negotiated codec: {}", codec.as_caps_str());

        // Become the host, under the registry lock and only when nothing else
        // runs. The admission held by our caller already keeps every other
        // session out, so this cannot fail today; it is the last line of the
        // one-session rule (D1) should that ever change.
        let id = {
            let mut reg = self.reg.lock();
            // The same test the admission passed (an overdue request does not
            // count), so an admitted viewer is not refused at the last step.
            if reg.host.is_some() || reg.busy_for(&peer) {
                None
            } else {
                let id = reg.reserve();
                reg.host = Some(HostSlot {
                    id,
                    peer: peer.clone(),
                    permission,
                    starting: false,
                    end_requested: None,
                    ended: Arc::default(),
                    session: None,
                });
                Some(id)
            }
        };
        let Some(id) = id else {
            tracing::warn!("not accepting {peer}: a session started meanwhile");
            refuse(self, peer, "busy");
            return;
        };

        let _ = self.signaling.send(SignalMsg::ConnectResponse {
            to: Some(peer.clone()),
            from: None,
            accepted: true,
            permission: Some(permission.as_str().into()),
            reason: None,
            codec: Some(codec.as_caps_str().to_string()),
            // The viewer translates modifier keys for this OS (D8).
            os: modifiers::OsFamily::from_name(self.platform.info().name)
                .wire_name()
                .map(String::from),
            // It pauses a background tab's main stream on request (P6).
            caps: Some(protocol::HostCaps { vpause: true }),
        });
        self.emit(
            id,
            &peer,
            true,
            SessionEvent::Phase(SessionPhase::Negotiating { permission }),
        );
        self.add_recent(&peer);
        let features = self
            .viewer_features
            .lock()
            .remove(&peer)
            .unwrap_or_default();
        session::begin_host(self, id, peer, permission, features, codec);
    }

    /// Host action: switch the live permission (view↔control) mid-session.
    pub fn set_permission(&self, permission: Permission) {
        let reg = &mut *self.reg.lock();
        if let Some(h) = reg.host.as_mut() {
            h.permission = permission;
            if let Some(s) = &h.session {
                // Update the host injection gate, then notify the viewer (§4.2).
                s.set_control(permission == Permission::Control);
                s.send_ctl(&ControlMsg::Perm { value: permission });
            }
        }
    }

    /// Host: switch the shared monitor mid-session. Takes effect directly; the
    /// new state comes back as `SessionEvent::Displays`. Not saved: the
    /// "Monitor to share" setting stays the default for the next session.
    pub fn host_select_display(&self, display: String) {
        if let Some(s) = self
            .reg
            .lock()
            .host
            .as_ref()
            .and_then(|h| h.session.as_ref())
        {
            s.request_display(display);
        }
    }

    /// Host: end the session (the viewer is told).
    pub fn end_hosting(&self) {
        if let Some(id) = self.host_session() {
            self.disconnect(id);
        }
    }

    // ---- viewer side (contract §3.2) ---------------------------------------------

    /// Connect out to `host_uuid` as a new viewer session presented on
    /// `surface` (its tab's, made by the app). Sends the first
    /// connect-request and returns the new session's id.
    ///
    /// Refused mid-switch, while the selected account is signed out (I10,
    /// I13), while this computer is hosting or letting a viewer in (D1), for
    /// this computer's own ID, and when a session with that host already
    /// exists (one per remote, I1). All checked and claimed in one critical
    /// section — the workspace state, then the registry — so a switch cannot
    /// slip between the check and the claim: either it sees this request and
    /// ends it, or this request sees the switch and is refused. The recent is
    /// written to the workspace the request started in.
    pub fn connect(
        &self,
        host_uuid: &str,
        surface: Arc<dyn VideoSurface>,
    ) -> Result<SessionId, ConnectError> {
        self.connect_in(host_uuid, surface, WindowId::MAIN)
    }

    /// [`connect`](Self::connect) with the video in `window` (plans/09: a
    /// detached window's card reconnects in that window).
    pub fn connect_in(
        &self,
        host_uuid: &str,
        surface: Arc<dyn VideoSurface>,
        window: WindowId,
    ) -> Result<SessionId, ConnectError> {
        let host_uuid = host_uuid.trim().to_lowercase();
        if !protocol::is_valid_uuid_v4(&host_uuid) {
            return Err(ConnectError::InvalidId);
        }
        if host_uuid == self.config.lock().uuid {
            return Err(ConnectError::SelfId);
        }
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
        let (id, started_in) = {
            let ws = self.ws.lock();
            if let Some(e) = ws.connect_blocked() {
                return Err(e);
            }
            let mut reg = self.reg.lock();
            if reg.hosting() {
                return Err(ConnectError::Hosting);
            }
            if let Some(existing) = reg.viewer_by_peer(&host_uuid) {
                return Err(ConnectError::AlreadyConnected(existing));
            }
            // An ended session with that host, or on that surface, whose
            // threads are still being joined: nothing new starts on top.
            if let Some(ending) = reg.peer_in_use(&host_uuid) {
                return Err(ConnectError::Ending(ending));
            }
            match reg.surface_in_use(&surface) {
                Some((other, true)) => return Err(ConnectError::Ending(other)),
                Some((other, false)) => return Err(ConnectError::SurfaceInUse(other)),
                None => {}
            }
            let id = reg.reserve();
            let started_in = ws.sel.workspace_key().map(|k| (k.to_string(), ws.epoch));
            let cell = session::current::SurfaceCell::new(surface, window);
            let mut slot = ViewerSlot::requesting(host_uuid.clone(), Some(cell));
            slot.workspace = started_in.clone();
            reg.viewers.insert(id, slot);
            // Queued (not awaited) inside the section, so a switch's
            // end-session for this request can only follow it, never precede it.
            let _ = self.signaling.send(SignalMsg::ConnectRequest {
                to: Some(host_uuid.clone()),
                from: None,
                password: None,
                caps: Some(protocol::signaling::Caps {
                    decode,
                    popout: true,
                    // It shows why the picture is gone when the host is on
                    // its lock screen (plans/10 U1).
                    hstate: true,
                }),
            });
            (id, started_in)
        };
        self.emit(
            id,
            &host_uuid,
            false,
            SessionEvent::Phase(SessionPhase::Requesting),
        );
        if let Some((key, epoch)) = started_in {
            self.add_recent_in(&key, epoch, &host_uuid);
        }
        self.time_request(id, 1);
        Ok(id)
    }

    /// End session `id` with `Timeout` if attempt `attempt` of its request is
    /// still unanswered after [`REQUEST_TIMEOUT`] (08a §5.2), whatever the UI
    /// does — so a request can never stay forever, holding its host (I1).
    fn time_request(&self, id: SessionId, attempt: u64) {
        let end_tx = self.end_tx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(REQUEST_TIMEOUT).await;
            let _ = end_tx.send(crate::EndRequest {
                id,
                reason: EndReason::Timeout,
                if_unanswered: Some(attempt),
            });
        });
    }

    /// Carry out an end sent over the ended channel (a dead link, a timer).
    pub(crate) fn end_requested(&self, req: crate::EndRequest) {
        if let Some(attempt) = req.if_unanswered {
            let unanswered = self
                .reg
                .lock()
                .viewers
                .get(&req.id)
                .is_some_and(|v| v.phase == ViewerPhase::Requesting && v.attempt == attempt);
            if !unanswered {
                return;
            }
            // Tell the host too, so an approval prompt there does not stay up
            // for a viewer that gave up.
            self.disconnect_with(req.id, req.reason);
            return;
        }
        self.end(req.id, req.reason);
    }

    pub(crate) async fn on_password_required(&self, from: String, nonce: String) {
        let Some((id, stale)) = ({
            let mut reg = self.reg.lock();
            reg.viewer_by_peer(&from).and_then(|id| {
                let v = reg.viewers.get_mut(&id)?;
                (v.phase == ViewerPhase::Requesting).then(|| {
                    let stale = std::mem::take(&mut v.auto_attempted);
                    (id, stale)
                })
            })
        }) else {
            tracing::warn!("ignoring a password request from {from}: we did not ask it");
            return;
        };
        self.pending_nonce.lock().insert(from.clone(), nonce);

        // A second challenge from a host we just auto-submitted to means the
        // saved password is no longer valid. Forget it and prompt normally,
        // rather than retrying a dead credential forever.
        if stale {
            self.forget_saved_login(&from);
            self.prompt_for_password(id, &from, PwReason::SavedRejected);
            return;
        }

        // Auto-login: replay a password this host has already accepted.
        if let Some(plain) = self.saved_login(&from).await {
            tracing::info!("auto-login: submitting saved password for {from}");
            self.send_password(id, plain, false, ViewerPhase::Requesting);
            return;
        }
        self.prompt_for_password(id, &from, PwReason::First);
    }

    /// Ask the user for the host's password — only if the request is still
    /// waiting (the saved-password lookup before it may outlast a cancel or a
    /// timeout; an ended session is never prompted for). While the prompt is
    /// up the request waits on the user, not the network, so it keeps this
    /// computer busy for as long as it takes; Cancel ends it.
    fn prompt_for_password(&self, id: SessionId, host: &str, reason: PwReason) {
        let waiting = match self.reg.lock().viewers.get_mut(&id) {
            Some(v) if v.phase == ViewerPhase::Requesting => {
                v.phase = ViewerPhase::PasswordRequired;
                v.busy_until = None;
                true
            }
            _ => false,
        };
        if waiting {
            self.emit(
                id,
                host,
                false,
                SessionEvent::Phase(SessionPhase::PasswordRequired { reason }),
            );
        }
    }

    /// UI action: answer session `id`'s password prompt. `remember` asks to
    /// save the password — but only once the host has actually accepted it
    /// (see `on_connect_response`). Ignored unless the prompt is up.
    pub fn submit_password(&self, id: SessionId, plaintext: String, remember: bool) {
        self.send_password(id, plaintext, remember, ViewerPhase::PasswordRequired);
    }

    /// Answer the host's challenge for session `id` — typed by the user (the
    /// prompt is up: `PasswordRequired`) or saved (the request is still
    /// waiting: `Requesting`). Any other phase is a stray answer, ignored.
    fn send_password(&self, id: SessionId, plaintext: String, remember: bool, from: ViewerPhase) {
        let auto = from == ViewerPhase::Requesting;
        let workspace = self.workspace_sel().workspace_key().map(String::from);
        let sent = {
            let mut reg = self.reg.lock();
            match reg.viewers.get_mut(&id) {
                Some(v) if v.phase == from => {
                    v.pending_login = Some(PendingLogin {
                        plaintext: plaintext.clone(),
                        remember,
                        workspace,
                    });
                    v.auto_attempted = auto;
                    // Waiting on the network again: the answer timeout starts over.
                    v.resent();
                    Some((v.peer.clone(), v.attempt))
                }
                _ => None,
            }
        };
        let Some((host, attempt)) = sent else {
            return;
        };
        self.time_request(id, attempt);
        let nonce = self.pending_nonce.lock().remove(&host).unwrap_or_default();
        // proof = SHA256( SHA256(plaintext) + ":" + nonce ).
        let proof = handshake::compute_proof(&plaintext, &nonce);
        let _ = self.signaling.send(SignalMsg::ConnectRequest {
            to: Some(host.clone()),
            from: None,
            password: Some(proof),
            caps: None,
        });
        self.emit(
            id,
            &host,
            false,
            SessionEvent::Phase(SessionPhase::Requesting),
        );
    }

    /// `host`: what an accepting host said about itself.
    pub(crate) async fn on_connect_response(
        &self,
        from: String,
        accepted: bool,
        permission: Option<String>,
        reason: Option<String>,
        host: HostTraits,
    ) {
        let HostTraits { codec, os, caps } = host;
        let perm = match permission.as_deref() {
            Some("control") => Permission::Control,
            _ => Permission::View,
        };
        // Only a host we asked may answer, and only once. On acceptance the
        // session moves on in the same step, under the registry lock, unless
        // this computer started hosting (or admitting a viewer) while waiting:
        // then a second session must not start (D1). One critical section, so
        // an incoming request cannot slip between the check and the change.
        let outcome = {
            let mut reg = self.reg.lock();
            let blocked = reg.hosting();
            let Some(id) = reg.viewer_by_peer(&from) else {
                drop(reg);
                tracing::warn!("ignoring a connect-response from {from}: we did not ask it");
                return;
            };
            let Some(v) = reg.viewers.get_mut(&id) else {
                return;
            };
            if v.phase != ViewerPhase::Requesting {
                drop(reg);
                tracing::warn!("ignoring a second connect-response from {from}");
                return;
            }
            let pending = v.pending_login.take();
            let auto = std::mem::take(&mut v.auto_attempted);
            if accepted && !blocked {
                v.phase = ViewerPhase::Negotiating;
                v.permission = perm;
                // The host told us which codec it will stream; decode with
                // the same (§3). And which OS it runs, for modifier
                // translation; absent = unknown = keys forwarded literally.
                v.codec = codec
                    .as_deref()
                    .and_then(Codec::from_caps_str)
                    .unwrap_or(Codec::H264);
                v.host_os = os
                    .as_deref()
                    .map_or(modifiers::OsFamily::Unknown, modifiers::OsFamily::from_name);
                v.host_vpause = caps.is_some_and(|c| c.vpause);
            }
            (id, blocked, pending, auto)
        };
        let (id, blocked, pending, auto) = outcome;

        if accepted && blocked {
            // Tell that host to drop the session it just opened for us.
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(from),
                from: None,
            });
            self.end(id, EndReason::Busy);
            return;
        }
        if !accepted {
            let r = reason.unwrap_or_else(|| "denied".into());
            // A rejected password is reported as a plain refusal, not a second
            // challenge — so this is the ONLY place a stale saved credential can
            // be detected. Without dropping it here, auto-login would keep
            // replaying a password the host no longer accepts and the prompt
            // would never reappear: a permanent lockout from that host.
            let message = if r == "bad-password" && auto {
                self.forget_saved_login(&from);
                "Saved password was rejected — forgotten, please connect again".to_string()
            } else {
                format!("connection {r}")
            };
            self.end(id, EndReason::Rejected(message));
            return;
        }
        // The host accepted us, so any password we just sent is known-good —
        // this is the only point at which it's worth saving. Sealed by the
        // platform's secret store; if that fails we simply don't save.
        if let Some(p) = pending {
            if let (true, Some(workspace)) = (p.remember, p.workspace) {
                self.save_login(workspace, from.clone(), p.plaintext);
            }
        }
        tracing::info!("viewer session {id} with {from}; awaiting the host's offer");
        self.emit(
            id,
            &from,
            false,
            SessionEvent::Phase(SessionPhase::Negotiating { permission: perm }),
        );
    }

    /// WebRTC negotiation payloads (offer/answer/ICE). An offer starts a
    /// viewer session — but only from a host this engine was accepted by;
    /// anything else goes to the live session with that same peer.
    pub(crate) fn on_signal(&self, from: String, data: SignalData) {
        match data {
            SignalData::Offer { sdp } => {
                let id = {
                    let reg = self.reg.lock();
                    reg.viewer_by_peer(&from).filter(|id| {
                        reg.viewers
                            .get(id)
                            .is_some_and(|v| v.phase == ViewerPhase::Negotiating)
                    })
                };
                match id {
                    Some(id) => session::start_viewer(self, id, sdp),
                    None => tracing::warn!(
                        "ignoring a WebRTC offer from {from}: not a host we connected to"
                    ),
                }
            }
            other => {
                let reg = self.reg.lock();
                let host = reg
                    .host
                    .as_ref()
                    .filter(|h| h.peer == from)
                    .and_then(|h| h.session.as_ref());
                let viewer = reg
                    .viewer_by_peer(&from)
                    .and_then(|id| reg.viewers.get(&id))
                    .and_then(|v| v.session.as_ref());
                match host.or(viewer) {
                    Some(s) => s.signal(other),
                    None => tracing::debug!("signal from {from} with no session — dropped"),
                }
            }
        }
    }

    // ---- per-session actions -----------------------------------------------------

    /// Viewer action: ask the host for a fresh keyframe ("Refresh" button — e.g.
    /// to clear residual smear after packet loss).
    pub fn request_refresh(&self, id: SessionId) {
        self.with_viewer(id, |s| s.send_ctl(&ControlMsg::KeyframeRequest { s: 0 }));
    }

    /// Viewer: ask the host to show monitor `display`; it honours it only
    /// while this viewer has control. The new state comes back as
    /// `SessionEvent::Displays`.
    pub fn select_display(&self, id: SessionId, display: String) {
        self.with_viewer(id, |s| {
            s.send_ctl(&ControlMsg::SelectDisplay { id: display })
        });
    }

    /// Viewer (D14): ask the host to ALSO stream monitor `display`, for a
    /// separate window. Needs control. When the host agrees, a
    /// `SessionEvent::Displays` lists it under `popouts`; the app then opens
    /// the window and calls [`Engine::attach_popout`].
    pub fn popout_display(&self, id: SessionId, display: String) {
        self.with_viewer(id, |s| s.send_ctl(&ControlMsg::Popout { id: display }));
    }

    /// Viewer (D14): the app opened the window for pop-out stream `slot` of
    /// session `id` and created `surface` in it (on the UI thread). Starts
    /// decoding and presenting that stream there, and sends its input. Returns
    /// the attachment's generation, which [`Engine::close_popout`] /
    /// [`Engine::detach_popout`] take; `None` when the session is gone or that
    /// slot is already attached.
    pub fn attach_popout(
        &self,
        id: SessionId,
        slot: u8,
        surface: Arc<dyn VideoSurface>,
    ) -> Option<u64> {
        session::open_viewer_popout(self, id, slot, surface)
    }

    /// Viewer (D14): whether pop-out stream `slot` of session `id` has a
    /// window attached (an old window for a reused slot may still be closing).
    pub fn popout_attached(&self, id: SessionId, slot: u8) -> bool {
        session::viewer_popout_attached(self, id, slot)
    }

    /// Viewer (D14): stop presenting pop-out stream `slot` and take its
    /// surface down, then tell the host to stop streaming it. Blocks until the
    /// window's threads are gone, so the app can destroy the window after it.
    /// Call it off the UI thread (a presenter may wait on that thread).
    pub fn close_popout(&self, id: SessionId, slot: u8, gen: Option<u64>) {
        session::close_viewer_popout(self, id, slot, gen, true);
    }

    /// Viewer (D14): like [`Engine::close_popout`] but without telling the
    /// host — for a window the host itself stopped streaming, or one left over
    /// from an ended session.
    pub fn detach_popout(&self, id: SessionId, slot: u8, gen: Option<u64>) {
        session::close_viewer_popout(self, id, slot, gen, false);
    }

    // ---- the ActiveGate (08a §6.4, §6.18) -----------------------------------------

    /// Put viewer sessions `on_screen` on screen and make `focused` the one
    /// with the keyboard (plans/09 W1, 09a §3.2). Their videos are shown once
    /// ready (not under their window's dialog, not in link trouble) and their
    /// hosts stream; every other viewer session is hidden and its host paused.
    /// Only `focused` exchanges the clipboard (re-baselined when it becomes
    /// focused, so text copied for another session is never sent here).
    /// `focused` need not be on screen: the clipboard stays with the session
    /// used last while another app is in front (plans/09 §7, blocker 5). It
    /// holds the shortcut hook only while it is also on screen and not under
    /// a dialog in its window.
    ///
    /// One atomic change under `reg`, so two racing callers cannot leave a
    /// mix of both answers. The gates only store atomics and queue to the
    /// applier (never block, never touch the OS), so holding the lock is
    /// allowed. What leaves the screen is queued before what comes on it, so
    /// "hide" always precedes "show" (I2).
    pub fn set_screen(&self, on_screen: &[SessionId], focused: Option<SessionId>) {
        let mut reg = self.reg.lock();
        let on: std::collections::BTreeSet<SessionId> = on_screen
            .iter()
            .copied()
            .filter(|i| reg.viewers.contains_key(i))
            .collect();
        let focused = focused.filter(|i| reg.viewers.contains_key(i));
        if reg.on_screen == on && reg.focused == focused {
            return;
        }
        let gate = |reg: &session::registry::Registry, id: SessionId| {
            reg.viewers.get(&id).and_then(|v| v.gate.clone())
        };
        let leaving: Vec<SessionId> = reg.on_screen.difference(&on).copied().collect();
        let coming: Vec<SessionId> = on.difference(&reg.on_screen).copied().collect();
        for id in leaving {
            if let Some(g) = gate(&reg, id) {
                g.set_on_screen(false);
            }
        }
        if reg.focused != focused {
            if let Some(g) = reg.focused.and_then(|id| gate(&reg, id)) {
                g.set_focused(false);
            }
        }
        for id in coming {
            if let Some(g) = gate(&reg, id) {
                g.set_on_screen(true);
            }
        }
        if reg.focused != focused {
            if let Some(g) = focused.and_then(|id| gate(&reg, id)) {
                g.set_focused(true);
            }
        }
        reg.on_screen = on;
        reg.focused = focused;
    }

    /// The viewer session with the keyboard, if any.
    pub fn focused_session(&self) -> Option<SessionId> {
        self.reg.lock().focused
    }

    /// A web dialog opened over `window`'s video (`true`) or the last one
    /// there closed (`false`): that window's video is hidden meanwhile, then
    /// shown and repainted again (08a §6.18, plans/09 J4). Returns once the
    /// hides and shows are done, so the UI paints a dialog only after the
    /// native video is out of its way (the surface contract). Must not be
    /// awaited on the UI thread: a backend's show/hide may run there.
    pub async fn set_overlay(&self, window: WindowId, open: bool) {
        {
            let mut overlays = self.overlays.lock();
            if open {
                overlays.insert(window);
            } else {
                overlays.remove(&window);
            }
        }
        let gates: Vec<_> = {
            let reg = self.reg.lock();
            reg.viewers
                .values()
                .filter_map(|v| v.gate.clone())
                .filter(|g| g.window() == window)
                .collect()
        };
        for g in gates {
            g.overlay_changed();
        }
        // Bounded: a UI thread stuck elsewhere must not hang the dialog.
        let (done, applied) = tokio::sync::oneshot::channel();
        if self
            .view_ops
            .send(session::active::ViewOp::Barrier(done))
            .is_ok()
            && tokio::time::timeout(std::time::Duration::from_secs(2), applied)
                .await
                .is_err()
        {
            tracing::warn!("video show/hide still pending after 2 s; opening the dialog anyway");
        }
    }

    /// Hand viewer session `id` to `surface` in `window` (plans/09 W2, 09a
    /// §3.2 and §4.2): in any phase before it ends, without a reconnect —
    /// the same session, the same host session. Never blocks: a live
    /// session's media thread builds a presenter there on its decoder's
    /// device and swaps (the input and drop sinks, cursor control, the view
    /// and the shortcut hook follow, "hide the old" before "show the new").
    /// `SessionEvent::SurfaceReleased` then names the surface the engine let
    /// go of: the old one when the move went through, `surface` when it
    /// failed (the session stays where it was; a toast says why).
    ///
    /// Refused while the session is being started or another move of it is
    /// under way (`Busy`), when `surface` belongs to a session already, and
    /// for a session that is gone or ending.
    pub fn move_session(
        &self,
        id: SessionId,
        surface: Arc<dyn VideoSurface>,
        window: WindowId,
    ) -> Result<(), MoveError> {
        use session::current::{Carrier, Target};
        // Checked and accepted under `reg`, like a connect's surface claim,
        // so no connect can take the surface in between.
        let (cell, peer, carrier) = {
            let reg = self.reg.lock();
            let Some(v) = reg.viewers.get(&id) else {
                return Err(if reg.reaping.contains_key(&id) {
                    MoveError::Ended
                } else {
                    MoveError::NotFound
                });
            };
            if v.starting {
                return Err(MoveError::Busy);
            }
            if reg.surface_in_use(&surface).is_some() {
                return Err(MoveError::SurfaceInUse);
            }
            // Every viewer slot made by `connect` has a cell.
            let Some(cell) = v.surface.clone() else {
                return Err(MoveError::NotFound);
            };
            let carrier = cell.request(Target { surface, window })?;
            (cell, v.peer.clone(), carrier)
        };
        let released = match carrier {
            Carrier::Swapped(old) => Some(old),
            Carrier::Media => None,
            // Its media thread is gone (its decoder failed): nothing draws,
            // so the sinks and the view move here, at once.
            Carrier::Caller => cell.complete(),
        };
        if let Some(old) = released {
            self.emit(
                id,
                &peer,
                false,
                SessionEvent::SurfaceReleased {
                    surface: crate::SurfaceTag::of(&old),
                },
            );
        }
        Ok(())
    }

    /// Put viewer session `id`'s [`session::TabGate`] in its slot and apply
    /// whether it is on screen and focused now — under `reg`, like every
    /// switch, so no switch can fall between the two.
    pub(crate) fn attach_gate(&self, id: SessionId, gate: Arc<session::TabGate>) {
        let mut reg = self.reg.lock();
        let on_screen = reg.on_screen.contains(&id);
        let focused = reg.focused == Some(id);
        // The slot stays while its start runs (an end then waits for the
        // starter), so it is always there; a missing one only means nothing
        // can switch it, and it stays off screen.
        if let Some(v) = reg.viewers.get_mut(&id) {
            gate.set_on_screen(on_screen);
            gate.set_focused(focused);
            v.gate = Some(gate);
        }
    }

    /// The app-wide held tab-chord keys (see `KeyTranslator::swallowed`).
    pub(crate) fn tab_chord_keys(&self) -> crate::modifiers::TabChordKeys {
        Arc::clone(&self.tab_chord_keys)
    }

    /// The UI event channel, for threads that report app-level events.
    pub(crate) fn ui_sender(&self) -> tokio::sync::mpsc::UnboundedSender<UiEvent> {
        self.ui.clone()
    }

    pub(crate) fn overlays(&self) -> session::active::Overlays {
        Arc::clone(&self.overlays)
    }

    pub(crate) fn view_ops(&self) -> session::active::ViewOps {
        self.view_ops.clone()
    }

    pub(crate) fn hook_owner(&self) -> Arc<parking_lot::Mutex<Option<SessionId>>> {
        Arc::clone(&self.hook_owner)
    }

    // ---- ending --------------------------------------------------------------------

    /// End session `id` (host or viewer) and tell its peer (best effort,
    /// contract §3.2 s5). A request not answered yet is cancelled at the host
    /// too, so it does not keep a session open for a viewer that left
    /// (08a §6.5 step 1).
    pub fn disconnect(&self, id: SessionId) {
        self.disconnect_with(id, EndReason::UserClosed);
    }

    /// End session `id` for `reason`, telling its peer.
    fn disconnect_with(&self, id: SessionId, reason: EndReason) {
        if let Some(peer) = self.session_peer(id) {
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(peer),
                from: None,
            });
            self.end(id, reason);
        }
    }

    /// End every session (hosting too) and tell their peers. Returns at once;
    /// each session's `Released` follows.
    pub fn end_all(&self) {
        let ids: Vec<SessionId> = {
            let reg = self.reg.lock();
            reg.host
                .iter()
                .map(|h| h.id)
                .chain(reg.viewers.keys().copied())
                .collect()
        };
        for id in ids {
            self.disconnect(id);
        }
    }

    /// End every viewer session (D6: an account switch disconnects the tabs;
    /// hosting is this computer's and goes on).
    pub(crate) fn end_viewer_sessions(&self, reason: EndReason) {
        let ids = self.viewer_sessions();
        for id in ids {
            let peer = self.reg.lock().viewers.get(&id).map(|v| v.peer.clone());
            if let Some(peer) = peer {
                let _ = self.signaling.send(SignalMsg::EndSession {
                    to: Some(peer),
                    from: None,
                });
            }
            self.end(id, reason.clone());
        }
    }

    pub(crate) fn on_remote_end(&self, from: String) {
        for id in self.sessions_with(&from) {
            self.end(id, EndReason::RemoteEnded);
        }
    }

    pub(crate) fn on_peer_offline(&self, to: Option<String>) {
        let ids = match to {
            Some(peer) => self.sessions_with(&peer),
            // Older servers omit `to`: only a session still waiting for its
            // host's answer can be the one the relay means.
            None => {
                let reg = self.reg.lock();
                reg.viewers
                    .iter()
                    .filter(|(_, v)| v.phase == ViewerPhase::Requesting)
                    .map(|(id, _)| *id)
                    .collect()
            }
        };
        for id in ids {
            self.end(id, EndReason::Offline);
        }
    }

    /// The sessions (host or viewer) with `peer`.
    fn sessions_with(&self, peer: &str) -> Vec<SessionId> {
        let reg = self.reg.lock();
        reg.host
            .iter()
            .filter(|h| h.peer == peer)
            .map(|h| h.id)
            .chain(reg.viewer_by_peer(peer))
            .collect()
    }

    /// End session `id` because it could not be started or kept going, telling
    /// its peer and the user why. Called by a starter too (`begin_host`,
    /// `start_viewer`): it gives up its start first, so the end happens now —
    /// for the reason asked for meanwhile, if one was.
    pub(crate) fn fail_session(&self, id: SessionId, why: &str) {
        let requested = {
            let mut reg = self.reg.lock();
            if let Some(h) = reg.host.as_mut().filter(|h| h.id == id) {
                h.starting = false;
                h.end_requested.take()
            } else if let Some(v) = reg.viewers.get_mut(&id) {
                v.starting = false;
                v.end_requested.take()
            } else {
                None
            }
        };
        if let Some(peer) = self.session_peer(id) {
            let _ = self.signaling.send(SignalMsg::EndSession {
                to: Some(peer),
                from: None,
            });
        }
        self.end(id, requested.unwrap_or(EndReason::Failed(why.to_string())));
    }

    /// End session `id`: `Phase(Ended(reason))` now, its surface detached and
    /// hidden now, then its threads stopped and joined on a reaper thread and
    /// `Released` last (08a §5.2, §6.5). Until released it stays in the
    /// registry as ending: it still makes this computer busy, still holds its
    /// peer and its surface, so nothing new starts on top of its threads. A
    /// session still being built is ended by its starter once what it builds
    /// exists (the end is recorded and carried out then). Ending a session
    /// that is already ending or gone does nothing: it ends exactly once.
    pub(crate) fn end(&self, id: SessionId, reason: EndReason) {
        enum Taken {
            Host(HostSlot),
            Viewer(ViewerSlot),
        }
        // Out of the live sessions and into the ending ones in one critical
        // section, so its peer and surface are never free in between.
        let (peer, host, ended, surface, session) = {
            let mut reg = self.reg.lock();
            let taken = if let Some(h) = reg.host.as_mut().filter(|h| h.id == id) {
                if h.starting {
                    h.end_requested.get_or_insert(reason);
                    return;
                }
                reg.host.take().map(Taken::Host)
            } else if let Some(v) = reg.viewers.get_mut(&id) {
                if v.starting {
                    v.end_requested.get_or_insert(reason);
                    return;
                }
                reg.viewers.remove(&id).map(Taken::Viewer)
            } else {
                None
            };
            let (peer, host, ended, surface, session) = match taken {
                None => return,
                Some(Taken::Host(h)) => (h.peer, true, h.ended, None, h.session),
                Some(Taken::Viewer(v)) => (v.peer, false, v.ended, v.surface, v.session),
            };
            // No tab is on screen until the UI picks the next one (08a §6.4).
            reg.on_screen.remove(&id);
            if reg.focused == Some(id) {
                reg.focused = None;
            }
            reg.reaping.insert(
                id,
                session::registry::Reaping {
                    peer: peer.clone(),
                    host,
                    surface: surface.clone(),
                },
            );
            (peer, host, ended, surface, session)
        };
        ended.store(true, Ordering::SeqCst);
        // Stop flag first, so its media thread does not present again (or
        // re-show the surface) while it winds down; then the surface lets go.
        if let Some(s) = &session {
            s.signal_stop();
        }
        // Hidden through the gate, after any show it queued before, and never
        // shown again.
        if let Some(v) = session.as_ref().and_then(|s| s.view()) {
            v.retire();
        }
        // No more moves; input and drops stop, the OS cursor comes back.
        if let Some(cell) = &surface {
            cell.retire();
        }
        tracing::info!("session {id} with {peer} ended: {reason:?}");
        self.emit(
            id,
            &peer,
            host,
            SessionEvent::Phase(SessionPhase::Ended(reason)),
        );
        self.reap(id, &peer, host, session, surface);
    }

    /// Join session `id`'s threads on their own thread, then hide its surface
    /// (a media thread that was just starting may have shown it after it was
    /// detached — and no newer session can hold it: the surface stays taken
    /// until this release), take it out of `reaping` and emit `Released`.
    fn reap(
        &self,
        id: SessionId,
        peer: &str,
        host: bool,
        session: Option<session::Session>,
        surface: Option<Arc<session::current::SurfaceCell>>,
    ) {
        let ui = self.session_ui(id, peer, host);
        let reg = Arc::clone(&self.reg);
        let view = session.as_ref().and_then(|s| s.view());
        let release = move || {
            if let Some(s) = session {
                s.stop_and_join();
            }
            // Retired again after the join: the last op the gate performs on
            // this surface is a hide. A session that never started has no
            // view (nothing showed its surface): hide it directly.
            match (&view, &surface) {
                (Some(v), _) => v.retire(),
                (None, Some(cell)) => cell.current().hide(),
                (None, None) => {}
            }
            reg.lock().reaping.remove(&id);
            ui.send(SessionEvent::Released);
        };
        if let Err(e) = std::thread::Builder::new()
            .name(format!("sct-reap-{id}"))
            .spawn(release)
        {
            // Out of threads: its own threads still stop on their flag; the
            // session stays marked ending, so nothing reuses its peer or
            // surface on top of threads nobody joined.
            tracing::error!("could not start the reaper for {id}: {e}");
        }
    }

    /// End every session and wait (up to `limit`) until all are released —
    /// their threads joined, so a host's held keys are let go and the Bye is
    /// on the wire (I9, 08a §6.17). For app exit and updates.
    pub async fn end_all_and_wait(&self, limit: std::time::Duration) {
        self.end_all();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let done = {
                let reg = self.reg.lock();
                reg.host.is_none() && reg.viewers.is_empty() && reg.reaping.is_empty()
            };
            if done || tokio::time::Instant::now() >= deadline {
                if !done {
                    tracing::warn!("sessions still winding down after {limit:?}; going on");
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // ---- the session module's view of the registry ----------------------------------

    /// Host session `id` is being built: its ended flag, if it still exists.
    /// From here until it is stored (or fails), an end is deferred to it.
    pub(crate) fn host_start(&self, id: SessionId) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        let mut reg = self.reg.lock();
        let h = reg.host.as_mut().filter(|h| h.id == id)?;
        h.starting = true;
        Some(h.ended.clone())
    }

    /// Viewer session `id` is being built: what it needs, if it is still
    /// waiting for its offer. From here until it is stored (or fails), an end
    /// is deferred to it.
    pub(crate) fn viewer_start(&self, id: SessionId) -> Option<session::ViewerStart> {
        let mut reg = self.reg.lock();
        let v = reg.viewers.get_mut(&id)?;
        if v.phase != ViewerPhase::Negotiating || v.starting {
            return None;
        }
        v.starting = true;
        Some(session::ViewerStart {
            peer: v.peer.clone(),
            surface: v.surface.clone(),
            codec: v.codec,
            host_os: v.host_os,
            host_vpause: v.host_vpause,
            permission: v.permission,
            ended: v.ended.clone(),
        })
    }

    /// Keep the started host session in its slot. `false` when an end was
    /// asked for while it was starting: it is ended now (its threads exist, so
    /// they are joined), and nothing more may be sent for it.
    pub(crate) fn store_host_session(&self, id: SessionId, s: session::Session) -> bool {
        let requested = {
            let mut reg = self.reg.lock();
            match reg.host.as_mut().filter(|h| h.id == id) {
                Some(h) => {
                    h.session = Some(s);
                    h.starting = false;
                    h.end_requested.take()
                }
                // Cannot happen (an end waits for the starter), but if it did
                // the threads would have no owner: stop them here.
                None => {
                    drop(reg);
                    reap_orphan(id, s);
                    return false;
                }
            }
        };
        self.finish_start(id, requested)
    }

    /// Keep the started viewer session in its slot (now live). `false` as for
    /// [`Engine::store_host_session`].
    pub(crate) fn store_viewer_session(&self, id: SessionId, s: session::Session) -> bool {
        let requested = {
            let mut reg = self.reg.lock();
            match reg.viewers.get_mut(&id) {
                Some(v) => {
                    v.phase = ViewerPhase::Live;
                    v.session = Some(s);
                    v.starting = false;
                    v.end_requested.take()
                }
                None => {
                    drop(reg);
                    reap_orphan(id, s);
                    return false;
                }
            }
        };
        self.finish_start(id, requested)
    }

    /// Carry out an end asked for while session `id` was starting.
    fn finish_start(&self, id: SessionId, requested: Option<EndReason>) -> bool {
        match requested {
            None => true,
            Some(reason) => {
                if let Some(peer) = self.session_peer(id) {
                    let _ = self.signaling.send(SignalMsg::EndSession {
                        to: Some(peer),
                        from: None,
                    });
                }
                self.end(id, reason);
                false
            }
        }
    }

    /// Run `f` on viewer session `id`'s live session, under the registry lock
    /// (so it cannot end meanwhile). `f` must not take `ws` or `reg`, send
    /// events or block.
    pub(crate) fn with_viewer<R>(
        &self,
        id: SessionId,
        f: impl FnOnce(&session::Session) -> R,
    ) -> Option<R> {
        let reg = self.reg.lock();
        reg.viewers.get(&id).and_then(|v| v.session.as_ref()).map(f)
    }
    // ---- saved host passwords (per workspace, D7) -----------------------------

    /// The saved password for `host` in the active workspace, if one is saved
    /// and the secret store opens it. Saved passwords are used whenever
    /// present (D7: typed once per host, remembered by default).
    ///
    /// `open` runs off the runtime and is bounded: a locked OS keyring shows an
    /// unlock prompt and waits for the user, and this runs on the host's
    /// signaling task, which other work for that host waits behind. No answer
    /// in time means "no saved password" and the user is asked normally.
    async fn saved_login(&self, host: &str) -> Option<String> {
        let sealed = self
            .with_workspace_read(|w| w.saved_logins.get(host).cloned())
            .flatten()?;
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

    /// Remove `host`'s saved password from the active workspace and let the
    /// OS store drop its copy (also the UI's "Forget saved password" on a
    /// tab: it then asks again on the next connection).
    pub fn forget_saved_login(&self, host: &str) {
        let host = host.trim().to_lowercase();
        let removed = self
            .with_workspace(|w, _| w.saved_logins.remove(&host))
            .flatten();
        self.forget_sealed(removed.into_iter().collect());
    }

    /// Seal `plaintext` and save it for `host` in `workspace` — the one that
    /// was active when the password was typed, so a password never lands in
    /// another account's workspace (I13). Detached from the caller: the host
    /// has just accepted the password, and `seal` may sit behind an OS unlock
    /// prompt that must not stall signaling. Reported through the log and a
    /// toast.
    fn save_login(&self, workspace: String, host: String, plaintext: String) {
        let store = self.platform.secrets();
        let config = Arc::clone(&self.config);
        let config_path = self.config_path.clone();
        let writable = self.writable.load(Ordering::Relaxed);
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
            // Released: the entry this one replaced, or this one if its
            // workspace is gone (its account was removed meanwhile).
            let release = {
                let mut cfg = config.lock();
                let release = match cfg.workspaces.get_mut(&workspace) {
                    None => Some(sealed),
                    Some(w) => {
                        tracing::info!("auto-login: saved password for {host}");
                        w.saved_logins.insert(host, sealed)
                    }
                };
                if writable {
                    if let Err(e) = cfg.persist(&config_path) {
                        tracing::warn!("config: could not save ({e})");
                    }
                }
                release
            };
            if let Some(old) = release {
                tokio::task::spawn_blocking(move || store.forget(&old));
            }
        });
    }
}

/// A started session with no slot to keep it (cannot happen: an end waits for
/// its starter). Its threads are stopped and joined on their own thread so
/// they never run unowned.
fn reap_orphan(id: SessionId, s: session::Session) {
    s.signal_stop();
    if let Err(e) = std::thread::Builder::new()
        .name(format!("sct-reap-{id}"))
        .spawn(move || s.stop_and_join())
    {
        tracing::error!("could not start the reaper for {id}: {e}");
    }
}
