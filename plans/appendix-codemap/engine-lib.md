<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# engine-lib

## summary
engine-lib maps the control plane of the `engine` crate (native/crates/engine/src/lib.rs 640 lines, src/handshake.rs 73 lines, Cargo.toml 54 lines). `Engine` owns the parking_lot-guarded Config, the SignalingClient, the current Role (Idle/Host/Viewer), a connected AtomicBool, the Decider trait object, a tokio unbounded UI sender, and four per-peer maps/slots (pending_nonce, pending_caps, pending_login, auto_attempted) plus cached TURN servers. Engine::start (lib.rs:111-138) loads config, spawns signaling for the local UUID and returns (Arc<Engine>, UI receiver, signaling-event receiver). The app then drives handle_signaling (lib.rs:162) from the signaling receiver. No thread is spawned in these files and there are no statics or globals. The only channels are the UI tokio unbounded mpsc (sender in Engine.ui, lib.rs:88/122; receiver returned at lib.rs:116) and the signaling event receiver returned by SignalingClient::spawn (lib.rs:120-121, 117).

The control plane (signaling routing, approve/password handshake with SHA256 proof, saved-password auto-login, recents, busy gating, TURN cache, end-session handling) is OS-agnostic. Every media-side action is delegated to `pipeline` (Windows-only, lib.rs:17) via `#[cfg(windows)]` blocks, and each such site has a non-Windows stub. The stubs fix the codec to "H264" (lib.rs:371-373, 405, 537-538), drop WebRTC signaling (lib.rs:559-562), and skip begin_host/begin_viewer/teardown/set_control/send_ctl/KeyframeRequest. The important consequence: on macOS/Linux the engine still completes the whole accept/response/role-change flow (ConnectResponse accepted, role flips to Host/Viewer, RoleChanged emitted, recents and saved passwords persisted) while no media ever starts. The stubs therefore create a silent-failure path that a port must either close or make explicit.

Windows-only code is limited to these module gates (lib.rs:11-17: autostart, clipboard, pipeline), the call sites listed in platform_deps, and the Cargo.toml target sections (capture, codec, render, elevation, and the windows crate with Win32_Foundation/Graphics_Direct3D/Direct3D11/Security_Cryptography/System_DataExchange/System_Memory/System_Ole/System_Registry/UI_Shell). Ungated but Windows-backed code: secret::protect/unprotect (DPAPI per the comment at lib.rs:512, called ungated at lib.rs:458 and 517; implementation in secret.rs was not read). Tests in this unit are portable: handshake.rs:55-73 (proof round-trip against Config, nonce length). lib.rs has no tests.

## public_api
- lib.rs:15 pub mod handshake (ungated)
- lib.rs:18 pub mod secret (ungated)
- lib.rs:19 pub mod transfer (ungated)
- lib.rs:20 pub mod turn (ungated)
- lib.rs:12 pub mod autostart (cfg windows)
- lib.rs:14 pub mod clipboard (cfg windows)
- lib.rs:17 pub mod pipeline (cfg windows)
- lib.rs:30 pub use handshake::{ApprovalDecision, Decider}
- lib.rs:34-47 pub enum Role { Idle, Host{peer,permission}, Viewer{peer,permission} }
- lib.rs:50-75 pub enum UiEvent { ServerStatus(&'static str), ApprovalRequest{from}, PasswordRequired{from}, RoleChanged(Role), Toast(String), LinkTrouble{secs_left:u32}, LinkRestored }. LinkTrouble/LinkRestored are declared but never emitted in lib.rs; they are presumably sent from pipeline.
- lib.rs:78-105 pub struct Engine (fields are private)
- lib.rs:111 pub fn start(config_path: PathBuf, decider: Arc<dyn Decider>) -> (Arc<Engine>, UnboundedReceiver<UiEvent>, UnboundedReceiver<SigEvent>)
- lib.rs:140 pub fn config(&self) -> Config (clone)
- lib.rs:144 pub fn role(&self) -> Role (clone)
- lib.rs:150 pub fn server_connected(&self) -> bool (AtomicBool Relaxed)
- lib.rs:155 pub fn update_config(&self, f: FnOnce(&mut Config)) (locks, mutates, persists, ignores persist error)
- lib.rs:162 pub async fn handle_signaling(&self, SigEvent)
- lib.rs:270 pub fn turn_servers(&self) -> Vec<IceServer>
- lib.rs:398 pub fn connect_to(&self, host_uuid: String) (sends ConnectRequest with caps, adds recent)
- lib.rs:416 pub fn clear_recents(&self) -> Vec<String>
- lib.rs:464 pub fn submit_password(&self, host: String, plaintext: String, remember: bool)
- lib.rs:478 pub fn clear_saved_logins(&self)
- lib.rs:568 pub fn set_permission(&self, Permission) (host only)
- lib.rs:585 pub fn request_refresh(&self) (viewer only, sends KeyframeRequest on windows)
- lib.rs:593 pub fn end_session(&self) (sends EndSession if in a session, then local teardown)
- lib.rs:625 pub(crate) fn signaling(&self) -> &SignalingClient
- lib.rs:631 pub(crate) fn ui_sender(&self) -> UnboundedSender<UiEvent> (cloned for the transport thread)
- lib.rs:637 pub(crate) fn role_handle(&self) -> Arc<Mutex<Role>> (transport thread resets role to Idle)
- handshake.rs:9-13 pub enum ApprovalDecision { Deny, Allow(Permission) }
- handshake.rs:18-23 pub trait Decider: Send + Sync { fn decide<'a>(&'a self, from: &'a str) -> Pin<Box<dyn Future<Output=ApprovalDecision> + Send + 'a>>; } (boxed future, no async-trait)
- handshake.rs:26-34 pub struct DenyAll (always Deny)
- handshake.rs:37-42 pub fn random_nonce() -> String (16 bytes from rand::thread_rng, lower-case hex, 32 chars)
- handshake.rs:46-53 pub fn compute_proof(plaintext: &str, nonce: &str) -> String = hex(SHA256( Config::hash(plaintext) bytes + b":" + nonce bytes ))

## platform_deps
- **Codec negotiation on host accept: pipeline::set_negotiated_codec_from_caps(&decode) returns the codec string; non-Windows hardcodes "H264" and ignores caps** @ lib.rs:367-374
  - purpose: Pick the codec the host will encode with, from the viewer's advertised decode caps, and send it in ConnectResponse.codec
  - portable: Need a per-OS encoder capability probe and a negotiation function that returns one codec name. The stub returns H264 unconditionally, and the Windows version may return something else, so the non-Windows implementation must match the wire contract (codec string the viewer decodes).
- **pipeline::begin_host(self, peer, permission) starts the host media session (capture->encode->transport)** @ lib.rs:391-392 (called from accept after role=Host and recents persisted)
  - purpose: Start host media after acceptance
  - portable: Non-Windows: never called, so an accepted host has no media. Replacement must start capture/encode/transport on the same thread model or explicitly refuse acceptance.
- **pipeline::viewer_decode_caps() -> Vec<String> of hardware-decodable codecs advertised in ConnectRequest caps** @ lib.rs:402-405
  - purpose: Advertise only codecs this viewer can hardware-decode (comment lib.rs:399-401: advertising an undecodable codec causes a black screen)
  - portable: Non-Windows fixed to ["H264"]. Replacement must probe real decoders (VideoToolbox/VA-API/etc.) or the host can negotiate an undecodable codec.
- **pipeline::set_codec_from_str(codec_or_"H264") configures the viewer decoder from the host's chosen codec** @ lib.rs:535-538
  - purpose: Apply the codec from ConnectResponse before begin_viewer
  - portable: Non-Windows discards the codec (let _ = &codec). Replacement must select a decoder, and must do so before the first packet arrives.
- **pipeline::begin_viewer(self, peer, perm) starts the viewer media loop (transport->decode->render)** @ lib.rs:551-552
  - purpose: Start viewer media after acceptance
  - portable: Non-Windows: never called. Viewer role is set but no picture is rendered.
- **pipeline::on_signal(self, &from, SignalData) routes WebRTC offer/answer/ICE into the transport** @ lib.rs:556-563
  - purpose: All SDP/ICE traffic from the relay
  - portable: Non-Windows drops the payload silently. Any port must consume SignalData, or cross-network and LAN negotiation will never complete.
- **pipeline::set_control(bool) host input gate and pipeline::send_ctl(self, &ControlMsg::Perm{value}) notification** @ lib.rs:574-579 (inside set_permission, Host only; role is updated before this runs)
  - purpose: Enable/disable injection of viewer input on the host and tell the viewer its permission (contract §4.2)
  - portable: Non-Windows updates only the Role enum, with no gate and no notification. Replacement must enforce the input gate in the injector and send the ControlMsg over the control channel.
- **pipeline::send_ctl(self, &ControlMsg::KeyframeRequest)** @ lib.rs:585-590 (viewer Refresh button)
  - purpose: Ask host for a keyframe to clear packet-loss smear
  - portable: Non-Windows is a no-op.
- **pipeline::teardown(self) releases media, transport and input resources before role is reset** @ lib.rs:617-622 (end_session_local, called from end_session, on_remote_end, and RelayError peer-offline at lib.rs:227-231)
  - purpose: Tear down the media session on any end path
  - portable: Non-Windows: no teardown, role just resets to Idle. Any port's media resources must be torn down here, and teardown runs before role=Idle (order at lib.rs:618-620).
- **Module gating: autostart, clipboard, pipeline compiled only on Windows** @ lib.rs:11-17; module-level comment lib.rs:8-9 says the media pipeline is Windows-only
  - purpose: Compile-time split between portable control plane and Windows media
  - portable: A macOS/Linux port must provide equivalent modules, or keep the cfg split and fill the stubs at the call sites above.
- **DPAPI-backed secret::protect(&str)->Option<String> and secret::unprotect(&str)->Option<String>. secret.rs was not read; the comment at lib.rs:512 says DPAPI. The call sites are NOT cfg-gated.** @ lib.rs:458 (unprotect in saved_login), lib.rs:517 (protect in on_connect_response); Cargo.toml:45 Win32_Security_Cryptography feature
  - purpose: Persist saved viewer passwords encrypted at rest for auto-login
  - portable: Behavioural contract: protect returns None on failure (caller then refuses to save, never falls back to plaintext, lib.rs:513-531); unprotect returns None on failure. Non-Windows needs a per-user at-rest encryption with a key that cannot be copied with the config file. The ungated call sites require secret.rs to have a non-Windows impl, which must be verified.
- **Cargo: [target.'cfg(windows)'.dependencies] capture, codec, render, elevation** @ Cargo.toml:33-37
  - purpose: Media and elevation crates only on Windows
  - portable: Not referenced directly in lib.rs/handshake.rs (only via pipeline/autostart/clipboard). elevation is not referenced from lib.rs at all.
- **Cargo: windows crate with features Win32_Foundation, Win32_Graphics_Direct3D, Win32_Graphics_Direct3D11, Win32_Security_Cryptography (DPAPI), Win32_System_DataExchange (clipboard), Win32_System_Memory, Win32_System_Ole, Win32_System_Registry (autostart), Win32_UI_Shell** @ Cargo.toml:39-51
  - purpose: Windows API bindings for the Windows-only modules and secret.rs
  - portable: Enabled only on cfg(windows). The lib.rs control plane itself uses no Windows types.
- **Unconditional dependencies transport and input, which lib.rs and handshake.rs never reference** @ Cargo.toml:15-16; no `transport::` or `input::` path appears in lib.rs or handshake.rs
  - purpose: Presumably used only by pipeline/transfer/turn (not read)
  - portable: Whether input (likely a Windows SendInput injector) and transport compile on macOS/Linux must be checked in their crates. They are unconditional here, so they block a non-Windows build of the engine if they are Windows-bound.
- **Portable crates: str0m (workspace), if-addrs 0.15.0 (interface enumeration), hmac 0.12, sha1 0.10, md-5 0.10 (TURN long-term credential MESSAGE-INTEGRITY), rand 0.8, sha2, hex, parking_lot, tokio, serde_json, tracing** @ Cargo.toml:17-31
  - purpose: Crypto, TURN auth, ICE, runtime
  - portable: All are cross-platform. rand 0.8 uses the OS RNG via thread_rng (portable). if-addrs is portable, but interface naming and semantics differ per OS.

## portable_parts
- lib.rs:34-47 Role enum (Clone, PartialEq)
- lib.rs:50-75 UiEvent enum
- lib.rs:78-105 Engine struct and its field types
- lib.rs:111-138 Engine::start (uses SignalingClient::spawn and Config::load, both OS-agnostic as used here)
- lib.rs:140-159 config, role, server_connected, update_config
- lib.rs:162-183 handle_signaling (Registered/RegisterError/Disconnected/Message)
- lib.rs:186-237 route (ConnectRequest, ConnectResponse, PasswordRequired, Signal, EndSession, RelayError peer-offline, TurnCredentials)
- lib.rs:242-272 on_turn_credentials and turn_servers
- lib.rs:276-307 on_connect_request (busy gate, decode-caps store, mode dispatch)
- lib.rs:309-341 handle_password_request (nonce issue, proof verify, 2 s delay)
- lib.rs:343-360 handle_approve_request
- lib.rs:364-393 accept (except the cfg-gated pipeline lines 368-374 and 391-392)
- lib.rs:398-413 connect_to (except the cfg-gated caps lines 402-405)
- lib.rs:416-419 clear_recents
- lib.rs:421-448 on_password_required (stale-credential detection, auto-login, prompt)
- lib.rs:451-459 saved_login
- lib.rs:464-475 submit_password
- lib.rs:478-480 clear_saved_logins
- lib.rs:482-553 on_connect_response (except cfg lines 535-536 and 551-552)
- lib.rs:593-615 end_session and on_remote_end
- lib.rs:625-639 pub(crate) accessors
- handshake.rs:1-53 entire file (Decider, DenyAll, random_nonce, compute_proof)

## abstraction_points
- Media seam (the `#[cfg(windows)] pipeline::*` call sites). Functions the port must provide, with signatures inferred from the call sites (the pipeline source was not read): set_negotiated_codec_from_caps(&[String]) -> String (lib.rs:369); viewer_decode_caps() -> Vec<String> (lib.rs:403); begin_host(&Engine, String, Permission) (lib.rs:392); begin_viewer(&Engine, String, Permission) (lib.rs:552); set_codec_from_str(&str) (lib.rs:536); on_signal(&Engine, &str, SignalData) (lib.rs:558); set_control(bool) (lib.rs:577); send_ctl(&Engine, &ControlMsg) (lib.rs:578, 588); teardown(&Engine) (lib.rs:619). Recommended shape: a trait or a per-OS module exposing these exact functions, so lib.rs needs only cfg-free call sites.
- Decider trait (handshake.rs:18-23) is already the abstraction for the approval popup. The app supplies the impl (WebView2 modal on Windows, 30 s auto-deny per the comment at handshake.rs:15-17). A port supplies a native dialog or tray prompt behind the same trait.
- Secret storage seam: secret::protect(&str) -> Option<String> and secret::unprotect(&str) -> Option<String> (call sites lib.rs:458, 517). Replacement: per-OS keychain or DPAPI-equivalent. The contract is that failure returns None and nothing is ever stored in plaintext.
- Stub-vs-real codec capability seam: viewer_decode_caps and set_negotiated_codec_from_caps (lib.rs:402-405, 368-369). These must consult real decoder and encoder capabilities per OS, and the advertised list must reflect only real decoders.
- UI event seam: UiEvent over a tokio unbounded channel (lib.rs:88, 122). Also the transport thread gets ui_sender() and role_handle() (lib.rs:631, 637) so it can post events and reset role without holding &Engine.

## cross_module_contracts
- signaling::SignalingClient::spawn(server_url: String, uuid: String) -> (SignalingClient, Receiver<signaling::Event>) (lib.rs:120-121). Its thread/task model is not read here. Events used: Registered, RegisterError(reason), Disconnected, Message(SignalMsg).
- SignalingClient::send(SignalMsg) is used everywhere with `let _ =` (lib.rs:170, 284, 317, 349, 376, 406, 469, 599), so send errors are ignored. The port must not assume that a send failure is surfaced.
- SignalMsg variants used (protocol crate): ConnectRequest{to,from,password,caps: Option<signaling::Caps{decode: Vec<String>}>}; ConnectResponse{to,from,accepted,permission: Option<String>,reason: Option<String>,codec: Option<String>}; PasswordRequired{to,from,nonce}; Signal{to,from,data: SignalData}; EndSession{to,from}; RelayError{reason,to}; Turn; TurnCredentials{ice_servers: Option<IceServers{urls,username,credential}>, error: Option<String>}. Wire contract is defined by protocol/signaling, not read here.
- protocol::Config methods used: load(&Path) -> Config (no Result, lib.rs:119), persist(&Path) -> Result (lib.rs:158), effective_mode() -> Mode{Password,Approve}, password_permission (field), verify_proof(&nonce,&proof) -> bool, hash(&str) -> String (handshake.rs:47), set_password (tests), add_recent(&str) (lib.rs:390, 412), recent_ids (Vec), saved_logins: map host->sealed String, auto_login: bool, server_url, uuid. None of protocol was read.
- protocol::Permission{Control,View} with as_str() (lib.rs:380); permission parsing treats any string other than "control" as View (lib.rs:541-544). ControlMsg::{Perm{value: Permission}, KeyframeRequest} (lib.rs:578, 588). IceServer{urls,username,credential} (lib.rs:255-259).
- Ownership and threading: Engine is Arc-shared. Config, role, pending_* maps and TURN cache are parking_lot::Mutex (non-poisoning, sync). No guard is held across an .await in the lines read (checked: lib.rs:283, 324, 427, 452-457, 465, 491-492, 514-515, 586, 594, 608-611, 620). Nested locking: lib.rs:515 holds the pending_login guard while calling update_config (config lock). Keep this order (pending_login then config) if pipeline also locks these.
- Blocking IO: update_config calls cfg.persist (file write) while holding the config mutex, synchronously inside async fns (lib.rs:155-159). Persist errors are discarded.
- Async runtime: handle_signaling and its helpers are async and rely on tokio (tokio::time::sleep at lib.rs:329, tokio mpsc). The caller must run them on a tokio runtime with the time feature. Whether events are processed serially is decided by the caller (the app), which is not read here.

## gotchas
- Non-Windows accept path is incomplete: accept() (lib.rs:364-393) sends ConnectResponse accepted=true, sets Role::Host, persists recents, and emits RoleChanged, but on non-Windows begin_host is never called. The peer receives an accepted connection with no media. Likewise on_connect_response (lib.rs:482-553) sets Role::Viewer with no begin_viewer. A port must either start media or refuse before sending accept.
- Media-stub silent failures on non-Windows: on_signal discards SignalData (lib.rs:559-562), so WebRTC negotiation never starts. set_permission only changes the Role (lib.rs:574-580) and does not gate or notify the peer. request_refresh is a no-op (lib.rs:585-590). end_session_local skips teardown (lib.rs:617-622).
- Codec is hardcoded: non-Windows always negotiates "H264" (lib.rs:373, 405) and ignores host-provided codec (lib.rs:537-538). connect_to always advertises ["H264"] regardless of decoders (lib.rs:399-405), with the comment at lib.rs:399-401 warning that an undecodable advertised codec causes a black screen.
- Invariant stated in comment: a session is one at a time (lib.rs:32-33, 282). Enforced by a role check at lib.rs:283-293, which is check-then-act. The lock is released before accept() sets the role (lib.rs:384), and the approve path awaits the decider in between (lib.rs:347). If the caller processes signaling events concurrently, two requests can both pass the busy check. This is only safe if handle_signaling runs serially. The engine does not enforce that.
- The password path holds the 2 s delay inside the event handler (lib.rs:328-329, comment "§6 security: 2 s artificial delay"). If the caller awaits handle_signaling serially, a bad-password attempt blocks all other signaling events for 2 s.
- The password nonce is single-use: handle_password_request removes it on every proof attempt (lib.rs:324, `remove(&from).unwrap_or_default()`). A missing nonce becomes an empty string and is verified against the proof, which is then accepted only if it matches the empty-nonce hash. The code does not reject an absent nonce explicitly. Check verify_proof's behaviour with an empty nonce.
- Proof formula: SHA256( SHA256(plaintext) + ":" + nonce ) (lib.rs:466, handshake.rs:44-53). The first SHA256 is taken over Config::hash(plaintext) as a string, not raw bytes. Any port of protocol::Config::hash must produce the same string on both sides, or authentication breaks silently.
- Saved-password safety invariants (commented): a password is saved only after the host accepts it (lib.rs:97-99, 511-512). A rejected guess is never persisted. DPAPI-protect failure means no save and no plaintext fallback (lib.rs:512-513, 525-531). Stale saved credentials are detected in two places, on_password_required (lib.rs:424-438) and on_connect_response bad-password with auto set (lib.rs:496-507), so a dead credential does not cause a permanent lockout. The rejected-auto case returns early without emitting a refusal toast (lib.rs:499-507).
- on_connect_response has no check that `from` is the host we asked, and no check that we have an outstanding request. Any accepted:true ConnectResponse sets Role::Viewer and calls begin_viewer (lib.rs:482-553). It also takes pending_login regardless of whether `from` matches (lib.rs:515-516; only the save is host-gated). A port should bind responses to an outbound request.
- decode_caps retained across retries: pending_caps is only written when the request carries non-empty caps (lib.rs:296-300), because a password retry sends none. accept() removes them (lib.rs:367). A rejected or unanswered request leaves stale caps keyed by peer in pending_caps. Nothing clears them except accept.
- RelayError peer-offline: the handler at lib.rs:227-231 ignores the `to` field (`let _ = to;`) and tears down the current session whatever peer the error refers to. If the server can send this for other peers, the active session is dropped incorrectly.
- TURN: credentials are fetched only on Registered (lib.rs:168-170) and cached in turn_servers (lib.rs:266). An error response keeps the previous cache, with only a warn (lib.rs:247-250). There is no expiry or refresh timer in the engine, so long-lived processes depend on reconnects to refresh credentials. Empty ice_servers returns early (lib.rs:251) without clearing the cache.
- LinkTrouble and LinkRestored are declared in UiEvent (lib.rs:66-74) but never emitted in lib.rs. They must come from pipeline/transport on Windows, so a non-Windows port has no liveness UI unless it emits them itself.
- Engine::start drops the UI receiver’s consumer obligations: the caller owns both receivers and must pump sig_rx into handle_signaling. No task is spawned here, so a caller that forgets this gets no signaling processing.
- Recents are persisted synchronously before the connection exists: connect_to calls add_recent (lib.rs:412) even if the request is later denied or the peer is offline.
- Peer-authority on end: on_remote_end only ends the session when `from` equals the current peer (lib.rs:607-615). end_session sends EndSession to that peer and then tears down locally regardless of whether the send succeeds (lib.rs:593-605).
- The busy reply for ConnectRequest is sent to a non-peer (busy path, lib.rs:284-291), so an attacker can probe whether a host is in a session. That is intended behaviour, not a bug, but it is an information leak to preserve or change deliberately.

## tests
- handshake.rs:59-67 proof_verifies_against_config (portable; uses Config::set_password, verify_proof, compute_proof) — OS-agnostic, pure CPU.
- handshake.rs:69-72 nonce_is_16_bytes_hex (portable; asserts length 32). Note: it checks the length only, not hex characters or entropy.
- lib.rs has no #[cfg(test)] module. Control-plane transitions (accept, busy, auto-login, stale credential, peer-offline) are untested in this unit.
- secret.rs, turn.rs and transfer.rs have tests that were not read; their portability is unknown.
- Cargo.toml:53-54 dev-dependency tracing-subscriber only.
