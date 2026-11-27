//! End-to-end loopback: two real engines in one process, a real in-process
//! signaling relay, real str0m WebRTC over UDP on 127.0.0.1 — and mock OS
//! backends. Runs on every OS with no GPU, display or permissions, and exercises
//! everything above the OS line: handshake, ICE/DTLS/SCTP, RTP video with
//! keyframe gating, input routing and the permission gate, clipboard sync,
//! file push, and teardown.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::handshake::{ApprovalDecision, Decider};
use engine::{Engine, IcePolicy, PwReason, Role, SessionEvent, SessionId, SessionPhase, UiEvent};
use futures_util::{SinkExt, StreamExt};
use platform_api::mock::MockPlatform;
use platform_api::{Clipboard, FileDrop, VideoInput};
use protocol::{Config, InputMsg, Mode, Permission};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;

// ---- a minimal signaling relay implementing the server contract ----------------

type Peers = Arc<AsyncMutex<std::collections::HashMap<String, mpsc::UnboundedSender<String>>>>;

/// Start the relay; returns its `ws://` URL. Contract: `register` → `registered`;
/// a message with `to` is forwarded to that peer with `to` replaced by `from`;
/// an unknown target gets `relay-error peer-offline`; the literal ping → pong.
async fn start_relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peers: Peers = Arc::default();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let peers = peers.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let (mut sink, mut stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                tokio::spawn(async move {
                    while let Some(text) = rx.recv().await {
                        if sink.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                });
                let mut me: Option<String> = None;
                while let Some(Ok(msg)) = stream.next().await {
                    let Message::Text(text) = msg else { continue };
                    if text == r#"{"type":"ping"}"# {
                        let _ = tx.send(r#"{"type":"pong"}"#.into());
                        continue;
                    }
                    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
                        continue;
                    };
                    match v["type"].as_str() {
                        Some("register") => {
                            let uuid = v["uuid"].as_str().unwrap_or_default().to_string();
                            peers.lock().await.insert(uuid.clone(), tx.clone());
                            let _ = tx.send(
                                serde_json::json!({"type": "registered", "uuid": uuid}).to_string(),
                            );
                            me = Some(uuid);
                        }
                        _ => {
                            let (Some(from), Some(to)) =
                                (me.clone(), v["to"].as_str().map(str::to_string))
                            else {
                                continue;
                            };
                            let obj = v.as_object_mut().unwrap();
                            obj.remove("to");
                            obj.insert("from".into(), from.into());
                            match peers.lock().await.get(&to) {
                                Some(target) => {
                                    let _ = target.send(v.to_string());
                                }
                                None => {
                                    let _ = tx.send(
                                        serde_json::json!({
                                            "type": "relay-error",
                                            "reason": "peer-offline",
                                            "to": to
                                        })
                                        .to_string(),
                                    );
                                }
                            }
                        }
                    }
                }
                if let Some(me) = me {
                    peers.lock().await.remove(&me);
                }
            });
        }
    });
    format!("ws://{addr}")
}

// ---- harness -------------------------------------------------------------------

struct AllowControl;

impl Decider for AllowControl {
    fn decide<'a>(
        &'a self,
        _from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async { ApprovalDecision::Allow(Permission::Control) })
    }
}

/// An approval prompt that stays open until the test releases it: `decide`
/// records that it was asked, then waits for a permit before allowing control.
struct HeldApproval {
    asked: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<tokio::sync::Semaphore>,
}

impl Decider for HeldApproval {
    fn decide<'a>(
        &'a self,
        _from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async move {
            self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
            // Closing the semaphore is never done; a failed acquire would mean the
            // test tore down, and a denial is the safe answer then.
            match self.release.acquire().await {
                Ok(permit) => {
                    permit.forget();
                    ApprovalDecision::Allow(Permission::Control)
                }
                Err(_) => ApprovalDecision::Deny,
            }
        })
    }
}

struct Peer {
    engine: Arc<Engine>,
    platform: Arc<MockPlatform>,
    uuid: String,
    toasts: Arc<std::sync::Mutex<Vec<String>>>,
    /// How many password prompts the engine asked this side's UI to show.
    password_prompts: Arc<std::sync::atomic::AtomicU32>,
    /// Every role the engine reported, in order.
    roles: Arc<std::sync::Mutex<Vec<Role>>>,
    /// How many "link trouble" countdown ticks the engine reported.
    link_trouble: Arc<std::sync::atomic::AtomicU32>,
    /// Every monitor announcement this side's UI received.
    displays: Arc<std::sync::Mutex<Vec<Announced>>>,
    /// The pop-out streams `(slot, monitor id)` the latest announcement listed.
    popouts: Arc<std::sync::Mutex<Vec<(u8, String)>>>,
    /// Every session event this side received: (session, what happened).
    session_log: Arc<std::sync::Mutex<Vec<(SessionId, String)>>>,
}

/// One monitor announcement: `(ids, current, can_switch)`.
type Announced = (Vec<String>, String, bool);

impl Peer {
    /// Connect to `host` and make it the tab on screen, as the app's
    /// `connect_to` does (only the active session shows its video, P3).
    fn open(&self, host: &str) -> SessionId {
        let id = self
            .engine
            .connect(host, self.platform.surface.clone())
            .expect("this computer is not hosting");
        self.engine.set_screen(&[id], Some(id));
        id
    }

    /// The latest monitor announcement this side's UI received.
    fn last_displays(&self) -> Option<Announced> {
        self.displays.lock().unwrap().last().cloned()
    }
}

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sct-loopback-{}-{name}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_peer(name: &str, relay: &str) -> Peer {
    start_peer_on(name, relay, "mock")
}

/// A peer whose mock platform reports `os` as its [`PlatformInfo::name`].
///
/// [`PlatformInfo::name`]: platform_api::PlatformInfo::name
fn start_peer_on(name: &str, relay: &str, os: &'static str) -> Peer {
    start_peer_with(name, relay, |p| p.os_name = os)
}

/// A peer whose mock platform `configure` adjusts before the engine starts.
fn start_peer_with(name: &str, relay: &str, configure: impl FnOnce(&mut MockPlatform)) -> Peer {
    start_peer_full(name, relay, configure, Arc::new(AllowControl))
}

/// A peer with a chosen approval `decider` (approve mode is the default).
fn start_peer_full(
    name: &str,
    relay: &str,
    configure: impl FnOnce(&mut MockPlatform),
    decider: Arc<dyn Decider>,
) -> Peer {
    let root = temp_root(name);
    let mut platform = MockPlatform::new(&root);
    configure(&mut platform);
    let platform = Arc::new(platform);
    // Seed the config before the engine loads it: our relay, no public STUN.
    let cfg = Config {
        server_url: relay.to_string(),
        ice_servers: Vec::new(),
        ..Config::default()
    };
    std::fs::create_dir_all(&platform.paths.config_dir).unwrap();
    cfg.persist(&platform.paths.config_dir.join("config.json"))
        .unwrap();
    let uuid = cfg.uuid.clone();

    let (engine, mut ui_rx, mut sig_rx) = Engine::start(platform.clone(), decider);
    engine.set_ice_policy(IcePolicy::LoopbackOnly);

    // The same dispatch path as the app: per-peer tasks.
    let e = engine.clone();
    tokio::spawn(async move {
        while let Some(ev) = sig_rx.recv().await {
            e.dispatch_signaling(ev).await;
        }
    });
    let toasts: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let roles: Arc<std::sync::Mutex<Vec<Role>>> = Arc::default();
    let link_trouble: Arc<std::sync::atomic::AtomicU32> = Arc::default();
    let password_prompts: Arc<std::sync::atomic::AtomicU32> = Arc::default();
    let pp = password_prompts.clone();
    let displays: Arc<std::sync::Mutex<Vec<Announced>>> = Arc::default();
    let popouts: Arc<std::sync::Mutex<Vec<(u8, String)>>> = Arc::default();
    let session_log: Arc<std::sync::Mutex<Vec<(SessionId, String)>>> = Arc::default();
    let sl = session_log.clone();
    let (t, r, lt, ds, po) = (
        toasts.clone(),
        roles.clone(),
        link_trouble.clone(),
        displays.clone(),
        popouts.clone(),
    );
    tokio::spawn(async move {
        while let Some(ev) = ui_rx.recv().await {
            match ev {
                UiEvent::Toast(msg) => t.lock().unwrap().push(msg),
                UiEvent::Session { id, peer, host, ev } => {
                    sl.lock().unwrap().push((id, event_kind(&ev)));
                    match ev {
                        SessionEvent::Toast(msg) => t.lock().unwrap().push(msg),
                        SessionEvent::Phase(SessionPhase::Negotiating { permission }) => {
                            r.lock().unwrap().push(if host {
                                Role::Host { peer, permission }
                            } else {
                                Role::Viewer { peer, permission }
                            })
                        }
                        SessionEvent::Phase(SessionPhase::Ended(reason)) => {
                            if let Some(msg) = reason.message() {
                                t.lock().unwrap().push(msg);
                            }
                            r.lock().unwrap().push(Role::Idle);
                        }
                        SessionEvent::Phase(SessionPhase::PasswordRequired { reason }) => {
                            if reason == PwReason::SavedRejected {
                                t.lock()
                                    .unwrap()
                                    .push("Saved password no longer works".into());
                            }
                            pp.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        SessionEvent::LinkTrouble { .. } => {
                            lt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        SessionEvent::Displays {
                            list,
                            current,
                            can_switch,
                            popouts,
                        } => {
                            *po.lock().unwrap() =
                                popouts.into_iter().map(|p| (p.s, p.id)).collect();
                            ds.lock().unwrap().push((
                                list.into_iter().map(|d| d.id).collect(),
                                current,
                                can_switch,
                            ))
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });
    Peer {
        engine,
        platform,
        uuid,
        toasts,
        password_prompts,
        roles,
        link_trouble,
        displays,
        popouts,
        session_log,
    }
}

/// Poll `cond` until it holds or `timeout` passes.
async fn eventually(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A short label for a session event, for the session log.
fn event_kind(ev: &SessionEvent) -> String {
    match ev {
        SessionEvent::Phase(SessionPhase::Requesting) => "requesting".into(),
        SessionEvent::Phase(SessionPhase::PasswordRequired { .. }) => "password".into(),
        SessionEvent::Phase(SessionPhase::Negotiating { .. }) => "negotiating".into(),
        SessionEvent::Phase(SessionPhase::Ended(r)) => format!("ended:{r:?}"),
        SessionEvent::LinkTrouble { .. } => "trouble".into(),
        SessionEvent::LinkRestored => "restored".into(),
        SessionEvent::Displays { .. } => "displays".into(),
        SessionEvent::Toast(_) => "toast".into(),
        SessionEvent::HostScreen { screen, kind, why } => format!(
            "hostscreen:{}:{}:{}",
            screen.as_str(),
            kind.as_deref().unwrap_or(""),
            why.as_deref().unwrap_or("")
        ),
        SessionEvent::Released => "released".into(),
        SessionEvent::SurfaceReleased { surface } => format!("surface-released:{surface:?}"),
    }
}
/// The one viewer session of `p` (the tests start at most one per engine,
/// except the multi-session test, which names its sessions).
fn sid(p: &Peer) -> SessionId {
    *p.engine
        .viewer_sessions()
        .first()
        .expect("a viewer session")
}
fn injected(p: &Peer) -> Vec<InputMsg> {
    p.platform.injector.lock().unwrap().events.clone()
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

// ---- the test -------------------------------------------------------------------

/// While the host's approval prompt is open for one viewer, a second viewer is
/// answered "busy" at once — signaling for one peer no longer waits on another
/// peer's prompt (plans/08 P0) — and the one-session rule still holds: only the
/// first viewer gets in once the prompt is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_viewer_is_told_busy_while_the_first_awaits_approval() {
    let relay = start_relay().await;
    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let host = start_peer_full(
        "host-held",
        &relay,
        |_| {},
        Arc::new(HeldApproval {
            asked: asked.clone(),
            release: release.clone(),
        }),
    );
    let first = start_peer("viewer-first", &relay);
    let second = start_peer("viewer-second", &relay);
    let t = Duration::from_secs(15);
    eventually("all registered", t, || {
        host.engine.server_connected()
            && first.engine.server_connected()
            && second.engine.server_connected()
    })
    .await;

    first.open(&host.uuid);
    eventually("the host's prompt is open for the first viewer", t, || {
        asked.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;

    second.open(&host.uuid);
    eventually("the second viewer is told busy", t, || {
        second
            .toasts
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "connection busy")
    })
    .await;
    // Answered while the first prompt is still open: nothing was decided yet.
    assert!(
        matches!(host.engine.role(), Role::Idle),
        "the busy answer must not wait for the first viewer's prompt"
    );

    release.add_permits(1);
    let first_id = first.uuid.clone();
    eventually("the first viewer is admitted", t, || {
        matches!(host.engine.role(), Role::Host { ref peer, .. } if *peer == first_id)
            && matches!(first.engine.role(), Role::Viewer { .. })
    })
    .await;
    assert!(
        matches!(second.engine.role(), Role::Idle),
        "only one session at a time"
    );
}

/// A pending connect-request makes this computer busy for incoming ones (D1),
/// and cancelling its password prompt ends it, so this computer takes incoming
/// connections again (P0 review: a cancelled prompt used to leave it answering
/// "busy" to everyone until a restart).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_password_prompt_does_not_leave_this_computer_busy() {
    let relay = start_relay().await;
    let locked = start_peer("host-password", &relay);
    locked.engine.update_config(|c| {
        c.mode = Mode::Password;
        c.set_password(Some("correct horse"));
    });
    let me = start_peer("viewer-then-host", &relay);
    let other = start_peer("other-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("all registered", t, || {
        locked.engine.server_connected()
            && me.engine.server_connected()
            && other.engine.server_connected()
    })
    .await;

    me.open(&locked.uuid);
    eventually("the password prompt", t, || {
        me.password_prompts
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    })
    .await;

    // The prompt is up: this computer is connecting, so it is busy.
    other.open(&me.uuid);
    eventually("the other viewer is told busy", t, || {
        other
            .toasts
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "connection busy")
    })
    .await;

    // Cancel, as the prompt's Cancel button does. The ended session keeps
    // this computer busy until it is released (its threads joined), so wait
    // for that before the next request: answering "busy" until then is the
    // design (P2), not the stuck-busy bug this test guards against.
    me.engine.end_all_and_wait(t).await;
    // The other viewer's refused request is still being released too (it
    // holds that host and surface until then, `ConnectError::Ending`).
    other.engine.end_all_and_wait(t).await;
    other.open(&me.uuid);
    let other_id = other.uuid.clone();
    eventually("the other viewer is let in", t, || {
        matches!(me.engine.role(), Role::Host { ref peer, .. } if *peer == other_id)
            && matches!(other.engine.role(), Role::Viewer { .. })
    })
    .await;
    // And while hosting, this computer cannot connect out (D1).
    assert!(me
        .engine
        .connect(&locked.uuid, me.platform.surface.clone())
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_session_over_real_webrtc() {
    let relay = start_relay().await;
    let host = start_peer("host", &relay);
    let viewer = start_peer("viewer", &relay);
    let t = Duration::from_secs(15);

    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;

    // Handshake (approve mode → AllowControl), then WebRTC over loopback UDP.
    viewer.open(&host.uuid);
    eventually("roles set", t, || {
        matches!(
            host.engine.role(),
            Role::Host {
                permission: Permission::Control,
                ..
            }
        ) && matches!(viewer.engine.role(), Role::Viewer { .. })
    })
    .await;

    // ---- video: capture → mock H.264 → RTP → depacketize → decode → present
    eventually("30 frames presented", t, || {
        viewer.platform.surface.state().presented.len() >= 30
    })
    .await;
    {
        let s = viewer.platform.surface.state();
        assert!(s.visible, "surface shown once the decoder exists");
        let stamps: Vec<u64> = s.presented.iter().filter_map(|p| p.stamp).collect();
        assert_eq!(
            stamps.len(),
            s.presented.len(),
            "every frame carries its stamp"
        );
        // D11: the cursor is repainted between video frames by re-presenting the
        // LAST decoded frame with the current cursor, so the same stamp can
        // appear on consecutive presents. A re-present always repeats the newest
        // decoded frame, so collapsing consecutive duplicates leaves the stream
        // of DECODED frames — which must still arrive strictly in order (the
        // catch-up pipeline never reorders or goes backward).
        let decoded: Vec<u64> = stamps
            .iter()
            .enumerate()
            .filter(|&(i, s)| i == 0 || stamps[i - 1] != *s)
            .map(|(_, s)| *s)
            .collect();
        assert!(
            decoded.windows(2).all(|w| w[0] < w[1]),
            "decoded frames arrive in order (D11 re-presents de-duplicated): {stamps:?}"
        );
        let first = &s.presented[0];
        assert_eq!((first.width, first.height), (640, 360));
    }

    // ---- input: viewer key → ctl channel → host injector (control granted)
    assert!(viewer.platform.surface.user_input(VideoInput::Key {
        code: "KeyA",
        down: true
    }));
    eventually("KeyA injected on the host", t, || {
        injected(&host)
            .iter()
            .any(|m| matches!(m, InputMsg::KeyDown { code } if code == "KeyA"))
    })
    .await;

    // ---- clipboard: viewer copy → host clipboard, and no echo back
    viewer
        .platform
        .clipboard
        .user_copies("hello from the viewer");
    eventually("clipboard synced to the host", t, || {
        host.platform.clipboard.get_text().as_deref() == Some("hello from the viewer")
    })
    .await;
    let viewer_count = viewer.platform.clipboard.change_count();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        viewer.platform.clipboard.change_count(),
        viewer_count,
        "the host must not echo the text back"
    );

    // ---- file push: drop on the viewer → host Downloads + paste chord
    let src = temp_root("drop").join("report.txt");
    std::fs::write(&src, b"quarterly numbers").unwrap();
    assert!(viewer.platform.surface.user_drop(FileDrop {
        paths: vec![src],
        nx: 0.25,
        ny: 0.75,
    }));
    let downloads = host.platform.paths.downloads_dir.clone();
    eventually("file received on the host", t, || {
        files_in(&downloads)
            .iter()
            .any(|p| std::fs::read(p).ok().as_deref() == Some(b"quarterly numbers".as_slice()))
    })
    .await;
    eventually("pasted where it was dropped", t, || {
        host.platform.injector.lock().unwrap().focus_requests == vec![(0.25, 0.75)]
    })
    .await;

    // ---- permission: revoke control → held input released, new input refused
    host.engine.set_permission(Permission::View);
    eventually("held keys released on revoke", t, || {
        host.platform
            .injector
            .lock()
            .unwrap()
            .held_keys()
            .is_empty()
    })
    .await;
    let before = injected(&host).len();
    assert!(viewer.platform.surface.user_input(VideoInput::Key {
        code: "KeyB",
        down: true
    }));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !injected(&host)[before..]
            .iter()
            .any(|m| matches!(m, InputMsg::KeyDown { code } if code == "KeyB")),
        "a view-only peer's input must not be injected"
    );

    // ---- teardown from the viewer; the host follows
    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
    // Waited for, not asserted at once: the role is Idle as soon as the session
    // leaves the registry, and only then is the surface's hide queued on the
    // view's applier, which performs it asynchronously (plans/10 §10 F2).
    eventually("surface hidden after the session", t, || {
        !viewer.platform.surface.state().visible
    })
    .await;
    // The session's own events: accepted, then ended by the user and
    // released once its threads were joined (08a §5.2).
    eventually("the viewer session is released", t, || {
        let log = viewer.session_log.lock().unwrap();
        let id = log.first().map(|(s, _)| *s);
        let kinds: Vec<&str> = log
            .iter()
            .filter(|(s, _)| Some(*s) == id)
            .map(|(_, k)| k.as_str())
            .collect();
        kinds.contains(&"negotiating")
            && kinds.contains(&"ended:UserClosed")
            && kinds.last() == Some(&"released")
    })
    .await;
    let toasts = host.toasts.lock().unwrap().clone();
    assert!(
        !toasts.iter().any(|t| t.contains("unavailable")),
        "no capability errors on the host: {toasts:?}"
    );
}

// ---- the host's real cursor image (D9) ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_cursor_image_reaches_the_viewer_and_lands_on_the_hotspot() {
    let relay = start_relay().await;
    let host = start_peer("cursor-host", &relay);
    let viewer = start_peer("cursor-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);

    // The mock host reports its image once, with its first frame, before any
    // channel is open: it must still arrive (reliable ctl, buffered until open)
    // and be drawn wherever later position messages put the pointer.
    eventually("frames drawn with the host's cursor image", t, || {
        viewer
            .platform
            .surface
            .state()
            .presented
            .iter()
            .filter(|p| p.cursor_image_at.is_some())
            .count()
            >= 10
    })
    .await;

    let shape = platform_api::mock::mock_cursor_shape();
    {
        let s = viewer.platform.surface.state();
        // Received once (the content is unchanged), with the host's geometry and
        // the pixels converted to premultiplied alpha (opaque white stays white,
        // transparent texels are all-zero).
        let installed: Vec<_> = s.cursor_sprites.iter().flatten().collect();
        assert_eq!(installed.len(), 1, "installed once: {:?}", s.cursor_sprites);
        let sprite = installed[0];
        assert_eq!((sprite.width, sprite.height), (shape.width, shape.height));
        assert_eq!(sprite.hotspot, shape.hotspot);
        assert_eq!(
            sprite.frame,
            (640, 360),
            "measured in the host's frame pixels"
        );
        assert_eq!(
            sprite.bgra, shape.bgra,
            "white/transparent survives premultiply"
        );

        // Pixel check: on this 1:1 surface the image's opaque block (image
        // x ∈ [1,5), y ∈ [2,8), hotspot (2,3)) must cover exactly the 4×6 pixels
        // at pointer + (−1, −1) — for every frame where it is not clipped.
        let (sw, sh) = s.client_size;
        assert_eq!((sw, sh), (640, 360));
        let mut checked = 0;
        for p in s.presented.iter() {
            let (Some((nx, ny)), Some(at)) = (p.cursor, p.cursor_image_at) else {
                continue;
            };
            let (px, py) = (
                (nx * f64::from(sw)).round() as i32,
                (ny * f64::from(sh)).round() as i32,
            );
            if px < 2 || py < 2 || px + 4 > sw as i32 || py + 6 > sh as i32 {
                continue; // partly off-screen: clipped, a different rectangle
            }
            assert_eq!(
                at,
                platform_api::Rect {
                    x: px - 1,
                    y: py - 1,
                    w: 4,
                    h: 6
                },
                "pointer at ({px},{py})"
            );
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} unclipped frames to check");
    }

    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
}

/// A 256×256 pointer image (the wire maximum: 256 KiB of pixels in 16 parts of
/// ~33 KiB of JSON, ≈ 526 KB, four times SCTP's whole 128 KiB send buffer),
/// distinct per `seed`.
fn big_cursor(seed: u8) -> platform_api::CursorShape {
    let side = 256u32;
    let mut bgra = Vec::with_capacity((side * side * 4) as usize);
    for i in 0..side * side {
        let v = (i as u8).wrapping_mul(31).wrapping_add(seed);
        // Straight alpha 255 everywhere, so premultiplying keeps the bytes.
        bgra.extend_from_slice(&[v, seed, v ^ seed, 255]);
    }
    platform_api::CursorShape {
        width: side,
        height: side,
        hotspot: platform_api::Point {
            x: i32::from(seed),
            y: 255 - i32::from(seed),
        },
        bgra,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maximum_size_cursor_images_in_a_burst_all_arrive_intact() {
    // Before ctl had a send queue, str0m refused every part past its 128 KiB
    // send buffer and the driver dropped them: any image of 128 px or more
    // lost its tail, was counted as sent, and never showed. Six maximum-size
    // images back to back (the first shape plus a burst of five: 96 parts,
    // ≈ 3.2 MB on ctl, queued before the channel opens and flushed at open)
    // must all get through, in order, the last one complete and current —
    // without the viewer ever reporting link trouble while it drains (pongs
    // wait behind the parts; every ctl message counts as proof of life). On
    // localhost the drain takes milliseconds, so this guards the plumbing;
    // the slow-uplink case rests on that rule in the driver.
    let relay = start_relay().await;
    let burst: Vec<_> = (2..=6).map(big_cursor).collect();
    let last = burst[burst.len() - 1].clone();
    let host = start_peer_with("bigcur-host", &relay, |p| {
        p.capture.cursor_shape = Some(big_cursor(1));
        p.capture.cursor_burst = burst;
    });
    let viewer = start_peer("bigcur-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);

    eventually("the last 256×256 image installed and drawn", t, || {
        let s = viewer.platform.surface.state();
        let installed = s
            .cursor_sprites
            .last()
            .and_then(|s| s.as_ref())
            .is_some_and(|s| s.hotspot == last.hotspot);
        installed
            && s.presented
                .last()
                .is_some_and(|p| p.cursor_image_at.is_some())
    })
    .await;
    {
        let s = viewer.platform.surface.state();
        let Some(Some(sprite)) = s.cursor_sprites.last() else {
            panic!("no image installed");
        };
        assert_eq!((sprite.width, sprite.height), (256, 256));
        assert_eq!(sprite.frame, (640, 360));
        assert!(
            sprite.bgra == last.bgra,
            "every byte of all 16 parts arrived"
        );
    }
    // The session survived the 3.2 MB on ctl (pings queued behind it), and
    // neither side ever reported link trouble.
    assert!(matches!(viewer.engine.role(), Role::Viewer { .. }));
    for (who, p) in [("viewer", &viewer), ("host", &host)] {
        assert_eq!(
            p.link_trouble.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{who} reported link trouble"
        );
    }

    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
}

/// A raw protocol client, to play a peer that does not follow the rules.
async fn raw_client(relay: &str, uuid: &str) -> mpsc::UnboundedSender<String> {
    let (ws, _) = tokio_tungstenite::connect_async(relay).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(
        serde_json::json!({"type": "register", "uuid": uuid, "v": 1}).to_string(),
    ))
    .await
    .unwrap();
    // Wait for "registered" so later sends are relayed.
    while let Some(Ok(Message::Text(t))) = stream.next().await {
        if t.contains("\"registered\"") {
            break;
        }
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move { while stream.next().await.is_some() {} });
    tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsolicited_responses_and_offers_are_ignored() {
    let relay = start_relay().await;
    let victim = start_peer("victim", &relay);
    let t = Duration::from_secs(10);
    eventually("registered", t, || victim.engine.server_connected()).await;

    let stranger = "0b6f2b3e-6c1a-4e7d-9a2b-1c3d5e7f9a0b";
    let tx = raw_client(&relay, stranger).await;
    // "You are now viewing me" — the victim never asked.
    tx.send(
        serde_json::json!({
            "type": "connect-response", "to": victim.uuid, "accepted": true,
            "permission": "control", "codec": "H264"
        })
        .to_string(),
    )
    .unwrap();
    // A WebRTC offer, which would make the victim open a session to the stranger.
    tx.send(
        serde_json::json!({
            "type": "signal", "to": victim.uuid,
            "data": {"kind": "offer", "sdp": "v=0"}
        })
        .to_string(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Not merely idle now: it must never have reacted at all.
    assert_eq!(
        *victim.roles.lock().unwrap(),
        Vec::<Role>::new(),
        "role never changed"
    );
    assert_eq!(
        *victim.toasts.lock().unwrap(),
        Vec::<String>::new(),
        "no session was attempted"
    );
    assert_eq!(victim.engine.role(), Role::Idle);
    assert!(!victim.platform.surface.state().visible);
    assert!(victim.platform.surface.state().presented.is_empty());
}

// ---- modifier translation for the host's OS (D8) --------------------------------

/// The key events in `msgs` as "+Code" (down) / "-Code" (up).
fn key_log(msgs: &[InputMsg]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            InputMsg::KeyDown { code } => Some(format!("+{code}")),
            InputMsg::KeyUp { code } => Some(format!("-{code}")),
            _ => None,
        })
        .collect()
}

/// A controlling session between a viewer reporting `viewer_os` and a host
/// reporting `host_os`, with input proven to flow end to end. `tag` keeps the
/// peers' temp dirs apart from those of tests running in parallel.
async fn controlled_session(
    tag: &str,
    relay: &str,
    viewer_os: &'static str,
    host_os: &'static str,
) -> (Peer, Peer) {
    let host = start_peer_on(&format!("{tag}-host"), relay, host_os);
    let viewer = start_peer_on(&format!("{tag}-viewer"), relay, viewer_os);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    eventually("video presented", t, || {
        matches!(
            host.engine.role(),
            Role::Host {
                permission: Permission::Control,
                ..
            }
        ) && !viewer.platform.surface.state().presented.is_empty()
    })
    .await;
    // A plain key is never translated: once it arrives, the ctl channel is up.
    press(&viewer, &host, &["+KeyA", "-KeyA"], &["+KeyA", "-KeyA"]).await;
    (viewer, host)
}

/// Press `keys` on the viewer's surface and assert the host injected exactly
/// `expect` for them, in order.
async fn press(viewer: &Peer, host: &Peer, keys: &[&'static str], expect: &[&str]) {
    let before = injected(host).len();
    for &k in keys {
        let (down, code) = match k.split_at(1) {
            ("+", c) => (true, c),
            ("-", c) => (false, c),
            _ => panic!("bad test key {k}"),
        };
        assert!(viewer
            .platform
            .surface
            .user_input(VideoInput::Key { code, down }));
    }
    let what = format!("{keys:?} injected as {expect:?}");
    eventually(&what, Duration::from_secs(15), || {
        key_log(&injected(host)[before..]).len() >= expect.len()
    })
    .await;
    // Anything extra (a stray release, a doubled press) arrives within this.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(key_log(&injected(host)[before..]), expect, "for {keys:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mac_viewer_keys_mean_the_same_on_a_windows_host() {
    let relay = start_relay().await;
    let (viewer, host) = controlled_session("mac-win", &relay, "macos", "windows").await;

    // Cmd+C is Copy: Ctrl+C on Windows.
    press(
        &viewer,
        &host,
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
        &["+ControlLeft", "+KeyC", "-KeyC", "-ControlLeft"],
    )
    .await;
    // Cmd+Tab is the app switcher: Alt+Tab.
    press(
        &viewer,
        &host,
        &["+MetaLeft", "+Tab", "-Tab", "-MetaLeft"],
        &[
            "+ControlLeft",
            "+AltLeft",
            "-ControlLeft",
            "+Tab",
            "-Tab",
            "-AltLeft",
        ],
    )
    .await;

    // A Cmd held while the setting is switched off is released as the Ctrl it
    // was sent as; the next Cmd is literal.
    press(&viewer, &host, &["+MetaLeft"], &["+ControlLeft"]).await;
    viewer.engine.set_pref("translateModifiers", false);
    press(&viewer, &host, &["-MetaLeft"], &["-ControlLeft"]).await;
    press(
        &viewer,
        &host,
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
    )
    .await;
    // And back on: translated again, and nothing is left held on the host.
    viewer.engine.set_pref("translateModifiers", true);
    press(
        &viewer,
        &host,
        &["+MetaLeft", "+KeyV", "-KeyV", "-MetaLeft"],
        &["+ControlLeft", "+KeyV", "-KeyV", "-ControlLeft"],
    )
    .await;
    assert!(host
        .platform
        .injector
        .lock()
        .unwrap()
        .held_keys()
        .is_empty());
    viewer.engine.end_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_viewer_ctrl_c_is_cmd_c_on_a_mac_host() {
    let relay = start_relay().await;
    let (viewer, host) = controlled_session("win-mac", &relay, "windows", "macos").await;
    press(
        &viewer,
        &host,
        &["+ControlLeft", "+KeyC", "-KeyC", "-ControlLeft"],
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
    )
    .await;
    press(
        &viewer,
        &host,
        &["+AltLeft", "+Tab", "-Tab", "-AltLeft"],
        &[
            "+AltLeft",
            "+MetaLeft",
            "-AltLeft",
            "+Tab",
            "-Tab",
            "-MetaLeft",
        ],
    )
    .await;
    viewer.engine.end_all();
}

// ---- V2: the host follows the bandwidth estimate down in BITRATE, full res -----

/// Drive a constrained-then-recovering link over the real str0m session (via the
/// `test-hooks` bandwidth injector, since loopback has no congestion of its own)
/// and prove the V2 model end to end:
///   * the encoder's CBR target TRACKS the estimate down to the ~500 kbps floor
///     instead of pinning at the old 2.5 Mbps (the regression) — and tracks a mid
///     value and back up;
///   * degradation is spent ONLY on bitrate: the host's encode size stays FULL
///     resolution throughout — V2 removed V1's resolution ladder, so the link
///     estimate never changes the encode size (that caused the constant
///     soften/re-sharpen flap);
///   * frames keep flowing throughout (no stall) and neither side reports link
///     trouble — i.e. no self-sustaining collapse.
///
/// Resolution is asserted from the HOST's configured encode size: the mock encoder
/// passes the captured frame's dimensions straight through (it does not resample
/// the way a real H.264 encoder does), so this watches the size the host actually
/// configures. The viewer side is checked for liveness (frames keep arriving); the
/// device checklist (plans/checklists/V2.md) covers on-screen sharpness/steadiness.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn constrained_link_tracks_bwe_down_and_recovers() {
    let relay = start_relay().await;
    let host = start_peer("bwe-host", &relay);
    let viewer = start_peer("bwe-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);

    // Up and running at full resolution, with frames reaching the viewer, before
    // we touch the link.
    eventually("video flowing at full size", t, || {
        host.engine.test_encode_dims() == Some((640, 360))
            && !viewer.platform.surface.state().presented.is_empty()
    })
    .await;
    let observe = |phase: &str| {
        eprintln!(
            "V2 {phase}: applied={:?} bps, bwe={:?} bps, encode_dims={:?}",
            host.engine.test_encoder_target_bps(),
            host.engine.test_raw_link_bps(),
            host.engine.test_encode_dims(),
        );
    };
    // The encode size must stay FULL (640x360) regardless of the link — V2 spends
    // degradation on bitrate only, never on resolution.
    let assert_full_res = |phase: &str| {
        assert_eq!(
            host.engine.test_encode_dims(),
            Some((640, 360)),
            "{phase}: encode size changed with the link (V1 ladder regression)"
        );
    };

    // --- constrained: 300 kbps. The target must track DOWN to the ~500 kbps floor
    // (NOT pin at the old 2.5 Mbps), while the encode size stays full.
    host.engine.test_inject_bwe(300_000);
    eventually(
        "encoder target tracks the link down to the floor",
        t,
        || {
            host.engine
                .test_encoder_target_bps()
                .is_some_and(|b| b <= 600_000)
        },
    )
    .await;
    assert_eq!(host.engine.test_raw_link_bps(), Some(300_000));
    assert!(
        host.engine.test_encoder_target_bps().unwrap() < 2_500_000,
        "regression: target pinned at the old floor instead of tracking BWE"
    );
    assert_full_res("constrained");
    observe("constrained");
    // Frames keep flowing while constrained — no stall, no self-sustaining
    // collapse. Count progress from here.
    let presented_before = viewer.platform.surface.state().presented.len();
    eventually("frames keep flowing while constrained", t, || {
        viewer.platform.surface.state().presented.len() >= presented_before + 15
    })
    .await;
    // The resolution never dropped during the constrained phase.
    assert_full_res("constrained (after frames flowed)");

    // --- mid link: 2 Mbps held. V3 smooths the target, so it no longer snaps to
    // the exact estimate (that 1:1 follow is what made the picture pulse) — a
    // SUSTAINED mid value moves it up into a mid band and keeps it there. The
    // encode size is still full.
    host.engine.test_inject_bwe(2_000_000);
    eventually(
        "target rises to a mid band on a sustained mid estimate",
        t,
        || {
            host.engine
                .test_encoder_target_bps()
                .is_some_and(|b| (1_300_000..=2_100_000).contains(&b))
        },
    )
    .await;
    assert_full_res("mid");
    observe("mid");

    // --- recovered: 6 Mbps held. A sustained high estimate raises the target back
    // toward the ceiling (smoothed, so it climbs over a couple of seconds rather
    // than snapping); encode size full.
    host.engine.test_inject_bwe(6_000_000);
    eventually(
        "encoder target climbs back up on a sustained high estimate",
        t,
        || {
            host.engine
                .test_encoder_target_bps()
                .is_some_and(|b| b >= 4_000_000)
        },
    )
    .await;
    assert_full_res("recovered");
    observe("recovered");
    // The recovered link keeps delivering frames to the viewer too.
    let presented_recovered = viewer.platform.surface.state().presented.len();
    eventually("frames keep flowing after recovery", t, || {
        viewer.platform.surface.state().presented.len() >= presented_recovered + 15
    })
    .await;

    // No link trouble was ever reported — the fix is smooth, not a collapse.
    for (who, p) in [("viewer", &viewer), ("host", &host)] {
        assert_eq!(
            p.link_trouble.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{who} reported link trouble"
        );
    }

    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
}

/// V3: the encoder bitrate must stay STEADY on a noisy-but-stable link and still
/// follow a REAL sustained trend. This mirrors the owner's live host log, where
/// GCC's estimate stabbed between ~500–900 kbps and ~3–4 Mbps every second or two
/// on a link that was really multi-Mbps capable; V2 applied each swing 1:1, so the
/// full-resolution picture pulsed blurry→sharp→blurry. The smoothing lives in the
/// pure `tuning::RateFilter` (unit-tested directly); this proves it end to end over
/// the real str0m session via the `test-hooks` bandwidth injector.
///
/// Phase 1 (OSCILLATING): alternate the injected estimate ~300k↔~4M every ~1 s for
/// ~18 s and sample the APPLIED encoder target. It must change only a HANDFUL of
/// times (not once per swing) and stay inside a bounded mid band — i.e. steady, not
/// pulsing 500↔4000. Phase 2 (SUSTAINED DOWN): hold ~800k and the target must come
/// down. Phase 3 (SUSTAINED UP): hold ~6M and it must climb back toward the ceiling.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noisy_link_stays_steady_but_follows_a_sustained_trend() {
    let relay = start_relay().await;
    let host = start_peer("steady-host", &relay);
    let viewer = start_peer("steady-viewer", &relay);
    let t = Duration::from_secs(25);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    eventually("video flowing at full size", t, || {
        host.engine.test_encode_dims() == Some((640, 360))
            && !viewer.platform.surface.state().presented.is_empty()
    })
    .await;

    // Settle the filter at a known mid value FROM BELOW so the oscillation starts
    // from a deterministic steady point (not mid-ramp): pull it to the floor, then
    // let a sustained 2 Mbps raise it to a stable mid target.
    host.engine.test_inject_bwe(300_000);
    eventually("pulled to the floor", t, || {
        host.engine
            .test_encoder_target_bps()
            .is_some_and(|b| b <= 700_000)
    })
    .await;
    host.engine.test_inject_bwe(2_000_000);
    eventually("settled at a mid target", t, || {
        host.engine
            .test_encoder_target_bps()
            .is_some_and(|b| (1_300_000..=1_950_000).contains(&b))
    })
    .await;

    // --- Phase 1: oscillate ~300k ↔ ~4M every ~1 s for ~18 s, sampling the applied
    // target every ~300 ms. The raw link reading must prove the injector really is
    // swinging; the applied target must NOT.
    let mut samples: Vec<u32> = Vec::new();
    let mut raw_seen: Vec<u32> = Vec::new();
    let mut high = false;
    let osc_start = Instant::now();
    let mut next_flip = Instant::now();
    while osc_start.elapsed() < Duration::from_secs(18) {
        if Instant::now() >= next_flip {
            high = !high;
            host.engine
                .test_inject_bwe(if high { 4_000_000 } else { 300_000 });
            next_flip = Instant::now() + Duration::from_secs(1);
        }
        if let Some(b) = host.engine.test_encoder_target_bps() {
            samples.push(b);
        }
        if let Some(r) = host.engine.test_raw_link_bps() {
            raw_seen.push(r);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // The injector genuinely swung (raw reading saw both extremes) — otherwise the
    // steadiness below would be meaningless.
    assert!(
        raw_seen.iter().any(|&r| r <= 400_000) && raw_seen.iter().any(|&r| r >= 3_500_000),
        "raw link reading never swung — the oscillation injector did nothing: {raw_seen:?}"
    );

    // The applied target changed only a handful of times — NOT once per swing (V2
    // would have changed ~18 times). And it stayed inside a bounded mid band the
    // whole time: no 500↔4000 pulsing.
    let changes = samples.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        changes <= 4,
        "applied target pulsed with the noise: {changes} changes across {} samples: {samples:?}",
        samples.len()
    );
    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    assert!(
        min >= 1_000_000 && max <= 3_000_000,
        "applied target left the steady band under noise: min={min} max={max}"
    );

    // --- Phase 2: a REAL sustained drop to ~800 kbps must bring the target down.
    host.engine.test_inject_bwe(800_000);
    eventually("target follows a sustained drop", t, || {
        host.engine
            .test_encoder_target_bps()
            .is_some_and(|b| b <= 1_200_000)
    })
    .await;

    // --- Phase 3: a REAL sustained recovery to ~6 Mbps must bring it back up.
    host.engine.test_inject_bwe(6_000_000);
    eventually("target follows a sustained rise", t, || {
        host.engine
            .test_encoder_target_bps()
            .is_some_and(|b| b >= 4_000_000)
    })
    .await;

    // No link trouble through any of it — steady, not a collapse.
    for (who, p) in [("viewer", &viewer), ("host", &host)] {
        assert_eq!(
            p.link_trouble.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{who} reported link trouble"
        );
    }

    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_os_and_linux_windows_pairs_are_untouched() {
    let literal = [
        "+MetaLeft",
        "+KeyC",
        "-KeyC",
        "-MetaLeft",
        "+ControlLeft",
        "+Tab",
        "-Tab",
        "-ControlLeft",
    ];
    let relay = start_relay().await;
    for (viewer_os, host_os) in [("macos", "macos"), ("linux-x11", "windows")] {
        let tag = format!("{viewer_os}-{host_os}");
        let (viewer, host) = controlled_session(&tag, &relay, viewer_os, host_os).await;
        press(&viewer, &host, &literal, &literal).await;
        viewer.engine.end_all();
        eventually("both idle", Duration::from_secs(15), || {
            host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
        })
        .await;
    }
}

/// Monitor switching mid-session: a controlling viewer switches the host to
/// its second monitor (the picture changes size, input follows it), a
/// view-only viewer cannot, and the host can switch back from its own side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_switch_follows_picture_input_and_the_control_gate() {
    let relay = start_relay().await;
    let second = platform_api::DisplayInfo {
        id: "mock-1".into(),
        name: "Second".into(),
        x: 640,
        y: 0,
        width: 800,
        height: 600,
        scale: 1.0,
        primary: false,
    };
    let host = start_peer_with("monsw-host", &relay, |p| {
        p.extra_displays.push(second.clone())
    });
    let viewer = start_peer("monsw-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);

    // Both UIs learn the host's monitors; the viewer may switch (control).
    let both = vec!["mock-0".to_string(), "mock-1".to_string()];
    eventually("viewer told about the host's monitors", t, || {
        viewer.last_displays() == Some((both.clone(), "mock-0".into(), true))
    })
    .await;
    assert_eq!(
        host.last_displays(),
        Some((both.clone(), "mock-0".into(), true))
    );
    eventually("first monitor presented", t, || {
        viewer
            .platform
            .surface
            .state()
            .presented
            .iter()
            .any(|p| (p.width, p.height) == (640, 360))
    })
    .await;

    // The viewer's window is 700x400: the 640x360 monitor fits 1:1, and the
    // 800x600 one must be encoded to fit it once switched to.
    viewer.platform.surface.set_client_size((700, 400));
    tokio::time::sleep(Duration::from_millis(1500)).await; // size debounce + delivery

    // Viewer switches. (The mock encoder emits the captured size; the encode
    // size the host chose is read from its test hook.)
    viewer.engine.select_display(sid(&viewer), "mock-1".into());
    eventually("viewer told the switch happened", t, || {
        viewer.last_displays() == Some((both.clone(), "mock-1".into(), true))
    })
    .await;
    eventually("second monitor presented", t, || {
        viewer
            .platform
            .surface
            .state()
            .presented
            .iter()
            .any(|p| (p.width, p.height) == (800, 600))
    })
    .await;
    // The encode size the host chose, re-fit to the viewer's window
    // (observable only through the `test-hooks` feature).
    #[cfg(feature = "test-hooks")]
    assert_eq!(host.engine.test_encode_dims(), Some((532, 400)));
    eventually("input mapped onto the second monitor", t, || {
        host.platform.injector.lock().unwrap().target
            == Some(platform_api::Rect {
                x: 640,
                y: 0,
                w: 800,
                h: 600,
            })
    })
    .await;
    assert_eq!(
        host.last_displays().map(|d| d.1),
        Some("mock-1".to_string())
    );

    // View-only: the picker is disabled, and a request is ignored anyway.
    host.engine.set_permission(Permission::View);
    eventually("viewer picker disabled", t, || {
        viewer.last_displays() == Some((both.clone(), "mock-1".into(), false))
    })
    .await;
    let announced = host.displays.lock().unwrap().len();
    viewer.engine.select_display(sid(&viewer), "mock-0".into());
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        host.displays.lock().unwrap().len(),
        announced,
        "a view-only viewer must not switch monitors"
    );

    // The host can always switch, from its own side; input goes back to the
    // primary path.
    host.engine.host_select_display("mock-0".into());
    eventually("viewer told the host switched back", t, || {
        viewer.last_displays() == Some((both.clone(), "mock-0".into(), false))
    })
    .await;
    eventually("input back on the primary", t, || {
        host.platform.injector.lock().unwrap().target.is_none()
    })
    .await;

    // An unknown id changes nothing and says why.
    host.engine.host_select_display("gone".into());
    eventually("toast for an unknown monitor", t, || {
        host.toasts
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("no longer connected"))
    })
    .await;
    assert_eq!(
        host.last_displays().map(|d| d.1),
        Some("mock-0".to_string())
    );

    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
}

/// Pop-out windows (D14): a controlling viewer streams a second host monitor
/// to its own surface on a spare video track, at that monitor's size, while
/// the main window keeps its own; input from either window lands on the
/// monitor that window shows; closing the window stops the stream; revoking
/// control closes it from the host side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn popout_window_streams_a_second_monitor_with_its_own_input() {
    let relay = start_relay().await;
    let second = platform_api::DisplayInfo {
        id: "mock-1".into(),
        name: "Second".into(),
        x: 640,
        y: 0,
        width: 800,
        height: 600,
        scale: 1.0,
        primary: false,
    };
    let host = start_peer_with("popout-host", &relay, |p| {
        p.extra_displays.push(second.clone())
    });
    let viewer = start_peer("popout-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    eventually("main window presenting", t, || {
        viewer.last_displays().is_some_and(|d| d.2)
            && !viewer.platform.surface.state().presented.is_empty()
    })
    .await;

    // Ask for the second monitor in its own window; the host assigns stream 1.
    viewer.engine.popout_display(sid(&viewer), "mock-1".into());
    eventually("pop-out announced", t, || {
        *viewer.popouts.lock().unwrap() == vec![(1, "mock-1".to_string())]
    })
    .await;
    assert_eq!(
        *host.popouts.lock().unwrap(),
        vec![(1, "mock-1".to_string())],
        "the host's UI lists it too"
    );
    // The app opens the window and hands its surface over.
    let pop = platform_api::mock::MockSurface::new((800, 600));
    let gen1 = viewer.engine.attach_popout(sid(&viewer), 1, pop.clone());
    assert!(gen1.is_some());
    assert!(
        viewer
            .engine
            .attach_popout(sid(&viewer), 1, pop.clone())
            .is_none(),
        "one window per stream"
    );
    eventually("second monitor presented in the pop-out", t, || {
        pop.state()
            .presented
            .iter()
            .any(|p| (p.width, p.height) == (800, 600))
    })
    .await;
    // The main window still shows the first monitor, still live.
    let main_before = viewer.platform.surface.state().presented.len();
    eventually("main window still presenting", t, || {
        viewer.platform.surface.state().presented.len() > main_before + 3
    })
    .await;
    assert!(viewer
        .platform
        .surface
        .state()
        .presented
        .iter()
        .all(|p| (p.width, p.height) == (640, 360)));

    // Input from the pop-out maps onto the second monitor ...
    assert!(pop.user_input(VideoInput::Button {
        button: 0,
        down: true,
        nx: 0.25,
        ny: 0.5,
    }));
    assert!(pop.user_input(VideoInput::Button {
        button: 0,
        down: false,
        nx: 0.25,
        ny: 0.5,
    }));
    let rect = platform_api::Rect {
        x: 640,
        y: 0,
        w: 800,
        h: 600,
    };
    eventually("pop-out click injected on the second monitor", t, || {
        let log = host.platform.injector.lock().unwrap();
        log.target == Some(rect)
            && log
                .events
                .iter()
                .any(|e| matches!(e, InputMsg::ButtonUp { x, .. } if (*x - 0.25).abs() < 1e-9))
    })
    .await;
    // ... and from the main window back onto the first.
    assert!(viewer
        .platform
        .surface
        .user_input(VideoInput::Move { nx: 0.75, ny: 0.5 }));
    eventually("main-window input back on the primary", t, || {
        host.platform.injector.lock().unwrap().target.is_none()
    })
    .await;

    // Closing the window stops the stream on the host.
    viewer.engine.close_popout(sid(&viewer), 1, gen1);
    assert!(!viewer.engine.popout_attached(sid(&viewer), 1));
    eventually("host stopped the pop-out stream", t, || {
        host.popouts.lock().unwrap().is_empty() && viewer.popouts.lock().unwrap().is_empty()
    })
    .await;
    let frames = pop.state().presented.len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        pop.state().presented.len(),
        frames,
        "closed window presents nothing"
    );

    // Pop it out again, then revoke control: the host closes it.
    viewer.engine.popout_display(sid(&viewer), "mock-1".into());
    eventually("pop-out announced again", t, || {
        !viewer.popouts.lock().unwrap().is_empty()
    })
    .await;
    let pop2 = platform_api::mock::MockSurface::new((800, 600));
    let slot = viewer.popouts.lock().unwrap()[0].0;
    let gen2 = viewer
        .engine
        .attach_popout(sid(&viewer), slot, pop2.clone());
    assert!(gen2.is_some() && gen2 != gen1);
    // A late close of the OLD window (same slot) must not touch the new one.
    viewer.engine.close_popout(sid(&viewer), slot, gen1);
    assert!(viewer.engine.popout_attached(sid(&viewer), slot));
    host.engine.set_permission(Permission::View);
    eventually("revoking control closes pop-outs", t, || {
        viewer.popouts.lock().unwrap().is_empty() && host.popouts.lock().unwrap().is_empty()
    })
    .await;
    // A view-only viewer cannot open one.
    viewer.engine.popout_display(sid(&viewer), "mock-1".into());
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(host.popouts.lock().unwrap().is_empty());
    // The window is still attached when the session ends: by the time the
    // session is released (when the app destroys pop-out windows), the
    // window's threads are stopped and its surface let go (P2 review M1).
    let session = sid(&viewer);
    assert!(viewer.engine.popout_attached(session, slot));
    viewer.engine.end_all();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
    eventually("the session is released", t, || {
        viewer
            .session_log
            .lock()
            .unwrap()
            .iter()
            .any(|(s, k)| *s == session && k == "released")
    })
    .await;
    let s = pop2.state();
    assert!(!s.visible, "the pop-out surface is hidden before Released");
    assert!(!s.cursor_control, "and gives the OS cursor back");
}

/// The workspace store (plans/08 P1): each account sees only its own data, a
/// signed-out account's data is unreachable and connecting is refused (I13),
/// sign-up adopts the local workspace and "Add" merges it, and switching away
/// from a viewer session ends it (D6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accounts_keep_their_own_workspaces() {
    use engine::{ConnectError, WorkspaceSel};
    const X: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const Y: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let relay = start_relay().await;
    let me = start_peer("workspaces", &relay);
    let e = &me.engine;
    assert_eq!(e.workspace_sel(), WorkspaceSel::Local);

    // Local data, then sign-up adopts it.
    e.set_remote_name(X, "Office");
    assert!(e.set_pref("clipboardSync", false));
    let a = e.upsert_account("https://s.example", "ann");
    e.store_token(&a, "token-a".into(), true)
        .await
        .expect("the mock secret store seals");
    e.adopt_local_into(&a);
    e.switch_workspace(WorkspaceSel::Account(a.clone()), false)
        .await;
    let ws = e.workspace().expect("signed in");
    assert_eq!(ws.remote_name(X), Some("Office"));
    assert!(!e.prefs().clipboard_sync);
    assert!(e.config().account(&a).unwrap().token_ref.is_some());

    // Another account starts empty and never sees ann's data.
    let b = e.upsert_account("https://s.example", "bob");
    e.store_token(&b, "token-b".into(), false).await.unwrap();
    e.switch_workspace(WorkspaceSel::Account(b.clone()), true)
        .await;
    assert!(e.workspace().unwrap().remote_names.is_empty());
    assert!(e.prefs().clipboard_sync, "bob has the defaults");
    e.set_remote_name(Y, "Bob's");
    assert!(e.set_pref("clipboardSync", false));

    // Signed out: nothing is reachable, connecting is refused.
    e.forget_token(&b);
    e.switch_workspace(WorkspaceSel::Locked(b.clone()), true)
        .await;
    assert!(e.workspace().is_none());
    assert!(e.set_remote_name(X, "nope").is_empty());
    assert_eq!(
        e.connect(X, me.platform.surface.clone()),
        Err(ConnectError::SignedOut)
    );
    // Its settings still apply while it is signed out (a host keeps the
    // clipboard off when that was chosen).
    assert!(!e.prefs().clipboard_sync);

    // ann's token comes back from the secret store; her data is intact.
    assert_eq!(e.account_token(&a).await.as_deref(), Some("token-a"));
    e.switch_workspace(WorkspaceSel::Account(a.clone()), true)
        .await;
    assert_eq!(e.workspace().unwrap().remote_name(X), Some("Office"));
    assert_eq!(e.workspace().unwrap().remote_name(Y), None);

    // "Add": local names join the account; the account's own win.
    e.switch_workspace(WorkspaceSel::Local, true).await;
    assert!(
        e.workspace().unwrap().remote_names.is_empty(),
        "adopted on sign-up"
    );
    e.set_remote_name(Y, "Laptop");
    assert_eq!(e.local_data(), Some((0, 1)));
    e.merge_local_into(&a);
    e.switch_workspace(WorkspaceSel::Account(a.clone()), false)
        .await;
    let ws = e.workspace().unwrap();
    assert_eq!(ws.remote_name(X), Some("Office"));
    assert_eq!(ws.remote_name(Y), Some("Laptop"));
    assert_eq!(e.local_data(), None);

    // A sync answer from before a switch is not applied.
    let epoch = e.workspace_epoch();
    let doc = e.sync_doc(&a, true, true).unwrap();
    e.switch_workspace(WorkspaceSel::Account(a.clone()), false)
        .await;
    assert!(!e.apply_sync(&a, epoch, &doc, true, true));
    assert!(e.apply_sync(&a, e.workspace_epoch(), &doc, true, true));

    // Removing an account deletes its workspace.
    e.switch_workspace(WorkspaceSel::Local, true).await;
    e.remove_account(&b);
    assert!(e.config().account(&b).is_none());
    assert!(!e.config().workspaces.contains_key(&b));
}

/// Switching accounts ends a viewer session (D6); hosting is the computer's
/// and goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_accounts_ends_the_viewer_session() {
    use engine::WorkspaceSel;
    let relay = start_relay().await;
    let host = start_peer("ws-host", &relay);
    let viewer = start_peer("ws-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    eventually("connected", t, || {
        matches!(viewer.engine.role(), Role::Viewer { .. })
            && matches!(host.engine.role(), Role::Host { .. })
    })
    .await;
    let a = viewer.engine.upsert_account("https://s.example", "ann");
    viewer
        .engine
        .store_token(&a, "t".into(), false)
        .await
        .unwrap();
    viewer
        .engine
        .switch_workspace(WorkspaceSel::Account(a), true)
        .await;
    eventually("the viewer session ended", t, || {
        matches!(viewer.engine.role(), Role::Idle)
    })
    .await;
    // The host side saw it end too (end-session reached it).
    eventually("the host session ended", t, || {
        matches!(host.engine.role(), Role::Idle)
    })
    .await;
}

/// plans/08 P2: one viewer engine holds sessions with two hosts at once, each
/// on its own surface. Starting the second does not disturb the first; a
/// second connect to a connected host is refused and does not disturb it
/// either; a link death in one ends only that one while the other keeps
/// presenting; events are tagged with their own session; each ends once.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_viewer_holds_two_sessions_and_ends_them_separately() {
    use engine::ConnectError;
    let relay = start_relay().await;
    let host_a = start_peer("multi-host-a", &relay);
    let host_b = start_peer("multi-host-b", &relay);
    let viewer = start_peer("multi-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("all registered", t, || {
        host_a.engine.server_connected()
            && host_b.engine.server_connected()
            && viewer.engine.server_connected()
    })
    .await;
    let surface_a = viewer.platform.surface.clone();
    let surface_b = platform_api::mock::MockSurface::new((640, 360));
    let presented = |s: &Arc<platform_api::mock::MockSurface>| s.state().presented.len();
    let ended = |id: SessionId| {
        viewer
            .session_log
            .lock()
            .unwrap()
            .iter()
            .any(|(s, k)| *s == id && k.starts_with("ended:"))
    };

    // A first, and live.
    let a = viewer.open(&host_a.uuid);
    eventually("A presents", t, || presented(&surface_a) >= 10).await;

    // B starts while A is on screen, in the background: only the active tab
    // is shown and presented (P3, I2); A keeps presenting.
    let a_before_b = presented(&surface_a);
    let b = viewer
        .engine
        .connect(&host_b.uuid, surface_b.clone())
        .expect("not hosting");
    assert_ne!(a, b);
    let viewer_id = viewer.uuid.clone();
    eventually(
        "B's host is streaming",
        t,
        || matches!(host_b.engine.role(), Role::Host { ref peer, .. } if *peer == viewer_id),
    )
    .await;
    // B is in the background, so host B pauses its main stream (P6). The
    // mock screen changes every frame, so a host that kept streaming would
    // keep counting units.
    eventually("host B paused", t, || {
        host_b.engine.test_video_paused() == Some(true)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await; // a frame in flight
    let paused_at = host_b
        .engine
        .test_main_units()
        .expect("host B has a session");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        host_b.engine.test_main_units(),
        Some(paused_at),
        "a background tab's host sends no video"
    );
    eventually("A kept presenting while B started", t, || {
        presented(&surface_a) > a_before_b + 30
    })
    .await;
    assert_eq!(presented(&surface_b), 0, "an inactive tab is not presented");
    assert!(!surface_b.state().visible, "an inactive tab is hidden");
    assert_eq!(viewer.engine.focused_session(), Some(a));

    // Only the active tab exchanges the clipboard (B3).
    viewer.platform.clipboard.user_copies("for A");
    eventually("A's host gets the copy", t, || {
        host_a.platform.clipboard.get_text().as_deref() == Some("for A")
    })
    .await;
    // A copy made right before switching (B's loop has not polled since)
    // is still never sent to B: the gate re-baselines when it opens.
    viewer.platform.clipboard.user_copies("also for A");
    viewer.engine.set_screen(&[b], Some(b));
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_ne!(
        host_b.platform.clipboard.get_text().as_deref(),
        Some("also for A")
    );
    viewer.engine.set_screen(&[a], Some(a));

    let switch_to = |on: SessionId| viewer.engine.set_screen(&[on], Some(on));
    // Brought to the front, a paused tab's host streams again and new
    // pictures appear promptly (P6); the tab it replaced pauses in turn.
    eventually("host B paused again", t, || {
        host_b.engine.test_video_paused() == Some(true)
    })
    .await;
    let woke = Instant::now();
    let b_units = host_b
        .engine
        .test_main_units()
        .expect("host B has a session");
    switch_to(b);
    eventually("host B streams again", t, || {
        host_b
            .engine
            .test_main_units()
            .is_some_and(|u| u > b_units + 3)
    })
    .await;
    let b_before = presented(&surface_b);
    eventually("B presents the new pictures", t, || {
        presented(&surface_b) > b_before + 3
    })
    .await;
    assert!(
        woke.elapsed() < Duration::from_secs(5),
        "B took {:?} to come back",
        woke.elapsed()
    );
    eventually("host A paused", t, || {
        host_a.engine.test_video_paused() == Some(true)
    })
    .await;
    switch_to(a);

    // Switching 20 times (10 rounds there and back): the old tab is hidden
    // and stops presenting, the new one is shown and presents (I2).
    for round in 0..10 {
        for (on, on_s, off_s) in [(b, &surface_b, &surface_a), (a, &surface_a, &surface_b)] {
            let before = presented(on_s);
            switch_to(on);
            eventually("the new tab presents", t, || {
                on_s.state().visible && presented(on_s) > before + 2
            })
            .await;
            assert!(
                !off_s.state().visible,
                "round {round}: the old tab is hidden"
            );
            let off_count = presented(off_s);
            let on_count = presented(on_s);
            eventually("the new tab keeps presenting", t, || {
                presented(on_s) > on_count + 3
            })
            .await;
            assert!(
                presented(off_s) <= off_count + 1,
                "round {round}: a hidden tab is not presented"
            );
        }
    }
    assert_eq!(viewer.engine.focused_session(), Some(a));

    // The copy made while A was active never reached B, even though B was
    // active since (the gate re-baselines when it opens); B's own copy does.
    assert_ne!(
        host_b.platform.clipboard.get_text().as_deref(),
        Some("for A")
    );
    switch_to(b);
    // Copied once B's clipboard gate has opened (it wakes at once; this
    // waits a full poll so the test does not depend on that).
    tokio::time::sleep(Duration::from_millis(500)).await;
    viewer.platform.clipboard.user_copies("for B");
    eventually("B's host gets B's copy", t, || {
        host_b.platform.clipboard.get_text().as_deref() == Some("for B")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        host_a.platform.clipboard.get_text().as_deref(),
        Some("for A"),
        "an inactive tab's host gets nothing"
    );
    switch_to(a);
    eventually("A on screen again", t, || surface_a.state().visible).await;
    assert_eq!(viewer.engine.viewer_sessions(), vec![a, b]);
    assert!(
        matches!(host_a.engine.role(), Role::Host { ref peer, .. } if *peer == viewer_id)
            && matches!(host_b.engine.role(), Role::Host { ref peer, .. } if *peer == viewer_id)
    );
    {
        let log = viewer.session_log.lock().unwrap();
        for id in [a, b] {
            assert!(
                log.iter().any(|(s, k)| *s == id && k == "negotiating"),
                "{id} has its own events: {log:?}"
            );
        }
    }

    // One session per remote: a second connect is refused, and A lives on.
    assert_eq!(
        viewer
            .engine
            .connect(&host_a.uuid, viewer.platform.surface.clone()),
        Err(ConnectError::AlreadyConnected(a))
    );
    let a_now = presented(&surface_a);
    eventually("A still presents after the refused connect", t, || {
        presented(&surface_a) > a_now + 10
    })
    .await;

    // A link death in B, while both are live, ends only B.
    viewer.engine.test_sever(b);
    eventually("B ended on its own link and was released", t, || {
        let log = viewer.session_log.lock().unwrap();
        log.iter()
            .any(|(s, k)| *s == b && k.starts_with("ended:LinkLost"))
            && log.iter().any(|(s, k)| *s == b && k == "released")
    })
    .await;
    let a_after_sever = presented(&surface_a);
    eventually("A keeps presenting after B's link died", t, || {
        presented(&surface_a) > a_after_sever + 10
    })
    .await;
    assert!(!ended(a), "A did not end");
    assert_eq!(viewer.engine.viewer_sessions(), vec![a]);
    assert!(matches!(host_a.engine.role(), Role::Host { .. }));
    assert!(!surface_b.state().visible, "B's surface is hidden");

    // Ending A, by the user.
    viewer.engine.disconnect(a);
    eventually("A released", t, || {
        let log = viewer.session_log.lock().unwrap();
        log.iter().any(|(s, k)| *s == a && k == "released")
    })
    .await;
    eventually("host A's session ended", t, || {
        matches!(host_a.engine.role(), Role::Idle)
    })
    .await;
    assert!(viewer.engine.viewer_sessions().is_empty());
    let log = viewer.session_log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|(_, k)| k.starts_with("ended:")).count(),
        2,
        "each session ended exactly once: {log:?}"
    );
    assert!(
        log.iter().any(|(s, k)| *s == a && k == "ended:UserClosed"),
        "{log:?}"
    );
}

/// plans/09 W1: several sessions on screen at once, one focused. Two of three
/// hosts stream and present; the third is paused. Only the focused session
/// exchanges the clipboard, also when the other on-screen one presents.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_sessions_on_screen_one_focused() {
    let relay = start_relay().await;
    let hosts = [
        start_peer("screen-host-a", &relay),
        start_peer("screen-host-b", &relay),
        start_peer("screen-host-c", &relay),
    ];
    let viewer = start_peer("screen-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("all registered", t, || {
        hosts.iter().all(|h| h.engine.server_connected()) && viewer.engine.server_connected()
    })
    .await;
    let surfaces: Vec<_> = (0..3)
        .map(|_| platform_api::mock::MockSurface::new((640, 360)))
        .collect();
    let presented = |s: &Arc<platform_api::mock::MockSurface>| s.state().presented.len();
    let mut ids = Vec::new();
    for (h, s) in hosts.iter().zip(&surfaces) {
        ids.push(
            viewer
                .engine
                .connect(&h.uuid, s.clone())
                .expect("not hosting"),
        );
    }
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    viewer.engine.set_screen(&[a, b], Some(b));
    for (i, h) in hosts.iter().enumerate().take(2) {
        eventually("on screen: presenting", t, || presented(&surfaces[i]) > 10).await;
        assert_eq!(h.engine.test_video_paused(), Some(false));
    }
    eventually("off screen: its host paused", t, || {
        hosts[2].engine.test_video_paused() == Some(true)
    })
    .await;
    assert_eq!(
        presented(&surfaces[2]),
        0,
        "an off-screen session is not presented"
    );
    assert_eq!(viewer.engine.focused_session(), Some(b));

    // The clipboard goes to the focused session only.
    tokio::time::sleep(Duration::from_millis(500)).await;
    viewer.platform.clipboard.user_copies("for B");
    eventually("B's host gets it", t, || {
        hosts[1].platform.clipboard.get_text().as_deref() == Some("for B")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_ne!(
        hosts[0].platform.clipboard.get_text().as_deref(),
        Some("for B")
    );

    // Focus moves to A; both stay on screen and presenting.
    viewer.engine.set_screen(&[a, b], Some(a));
    tokio::time::sleep(Duration::from_millis(500)).await;
    viewer.platform.clipboard.user_copies("for A");
    eventually("A's host gets it", t, || {
        hosts[0].platform.clipboard.get_text().as_deref() == Some("for A")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_ne!(
        hosts[1].platform.clipboard.get_text().as_deref(),
        Some("for A"),
        "no longer focused: B gets nothing"
    );
    let (pa, pb) = (presented(&surfaces[0]), presented(&surfaces[1]));
    eventually("both keep presenting", t, || {
        presented(&surfaces[0]) > pa + 5 && presented(&surfaces[1]) > pb + 5
    })
    .await;

    // C comes on screen, A goes off: A pauses, C resumes.
    viewer.engine.set_screen(&[b, c], Some(c));
    eventually("A paused", t, || {
        hosts[0].engine.test_video_paused() == Some(true)
    })
    .await;
    eventually("C presents", t, || presented(&surfaces[2]) > 3).await;
    viewer.engine.end_all_and_wait(Duration::from_secs(3)).await;
}

/// The session-log entry for "the engine let go of `surface`" (plans/09 W2).
fn released_entry(surface: &Arc<platform_api::mock::MockSurface>) -> String {
    let s: Arc<dyn platform_api::VideoSurface> = surface.clone();
    format!("surface-released:{:?}", engine::SurfaceTag::of(&s))
}

/// How often session `id` of `p` logged `what`.
fn logged(p: &Peer, id: SessionId, what: &str) -> usize {
    p.session_log
        .lock()
        .unwrap()
        .iter()
        .filter(|(i, k)| *i == id && k == what)
        .count()
}

/// plans/09 W2: a live session moves between two surfaces twenty times with
/// no reconnect — the same session and one host session throughout. Each time
/// pictures reach the new surface, nothing is presented on the old one after
/// it is let go, and a key held on the old one is released on the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_session_moves_between_surfaces() {
    let relay = start_relay().await;
    let host = start_peer("move-host", &relay);
    let viewer = start_peer("move-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    let a = platform_api::mock::MockSurface::new((640, 360));
    let b = platform_api::mock::MockSurface::new((640, 360));
    let id = viewer
        .engine
        .connect(&host.uuid, a.clone())
        .expect("not hosting");
    viewer.engine.set_screen(&[id], Some(id));
    eventually("presenting on the first surface", t, || {
        a.state().presented.len() > 5
    })
    .await;
    let key_ups = |p: &Peer| {
        injected(p)
            .iter()
            .filter(|m| matches!(m, InputMsg::KeyUp { code } if code == "KeyQ"))
            .count()
    };
    let (mut from, mut to) = (a, b);
    for round in 0..20u32 {
        assert!(
            from.user_input(VideoInput::Key {
                code: "KeyQ",
                down: true
            }),
            "round {round}: the current surface takes input"
        );
        let ups = key_ups(&host);
        let focus_before = to.state().focus_requests;
        let entry = released_entry(&from);
        let before = logged(&viewer, id, &entry);
        viewer
            .engine
            .move_session(id, to.clone(), engine::WindowId(1 + round % 2))
            .expect("move accepted");
        eventually("the old surface let go", t, || {
            logged(&viewer, id, &entry) > before
        })
        .await;
        let left = from.state().presented.len();
        let shown = to.state().presented.len();
        eventually(
            "pictures on the new surface",
            Duration::from_secs(5),
            || to.state().presented.len() > shown && to.state().visible,
        )
        .await;
        eventually("the key held on the old surface released", t, || {
            key_ups(&host) > ups
        })
        .await;
        {
            let old = from.state();
            assert_eq!(
                old.presented.len(),
                left,
                "round {round}: drawn after release"
            );
            assert!(!old.visible, "round {round}: the old surface is hidden");
            assert!(
                !old.has_input_sink,
                "round {round}: the old surface takes no input"
            );
            assert!(
                !old.cursor_control,
                "round {round}: the OS cursor is back there"
            );
        }
        assert!(
            to.state().cursor_control,
            "round {round}: control moved along"
        );
        eventually("the focused session's keyboard moved along", t, || {
            to.state().focus_requests > focus_before
        })
        .await;
        std::mem::swap(&mut from, &mut to);
    }
    assert_eq!(viewer.engine.viewer_sessions(), vec![id]);
    let negotiations = |p: &Peer| {
        p.session_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, k)| k == "negotiating")
            .count()
    };
    assert_eq!(negotiations(&host), 1, "one host session: no reconnect");
    assert_eq!(negotiations(&viewer), 1);
    viewer.engine.end_all_and_wait(Duration::from_secs(3)).await;
}

/// A session whose video never got a presenter (its media thread failed at
/// once) still moves: its input and the surface go along at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_video_still_moves() {
    let relay = start_relay().await;
    let host = start_peer("move-novideo-host", &relay);
    let viewer = start_peer("move-novideo-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    let broken = platform_api::mock::MockSurface::new((640, 360));
    broken.state().refuse_presenter = true;
    let id = viewer
        .engine
        .connect(&host.uuid, broken.clone())
        .expect("not hosting");
    viewer.engine.set_screen(&[id], Some(id));
    eventually("told it cannot display", t, || {
        viewer
            .toasts
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.starts_with("Cannot display the remote screen"))
    })
    .await;
    eventually("input bound", t, || broken.state().has_input_sink).await;
    let next = platform_api::mock::MockSurface::new((640, 360));
    viewer
        .engine
        .move_session(id, next.clone(), engine::WindowId(1))
        .expect("move accepted");
    eventually("the old surface let go", t, || {
        logged(&viewer, id, &released_entry(&broken)) == 1
    })
    .await;
    assert!(!broken.state().has_input_sink);
    assert!(next.state().has_input_sink, "input moved along");
    assert!(next.user_input(VideoInput::Key {
        code: "KeyW",
        down: true
    }));
    eventually("keys from the new surface reach the host", t, || {
        injected(&host)
            .iter()
            .any(|m| matches!(m, InputMsg::KeyDown { code } if code == "KeyW"))
    })
    .await;
    viewer.engine.end_all_and_wait(Duration::from_secs(3)).await;
}

/// A move whose new surface cannot get a presenter leaves the session where
/// it was, still presenting, and lets go of the surface it was moving to.
/// A move before the session is live only swaps: it goes live there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_move_stays_and_an_early_move_goes_live_there() {
    let relay = start_relay().await;
    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let host = start_peer_full(
        "move-held-host",
        &relay,
        |_| {},
        Arc::new(HeldApproval {
            asked: asked.clone(),
            release: release.clone(),
        }),
    );
    let viewer = start_peer("move-early-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    let first = platform_api::mock::MockSurface::new((640, 360));
    let early = platform_api::mock::MockSurface::new((640, 360));
    let id = viewer
        .engine
        .connect(&host.uuid, first.clone())
        .expect("not hosting");
    viewer.engine.set_screen(&[id], Some(id));
    eventually("the host is asking", t, || {
        asked.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    // Still requesting: the move is a swap, released at once.
    viewer
        .engine
        .move_session(id, early.clone(), engine::WindowId(4))
        .expect("an early move");
    eventually("the first surface let go", t, || {
        logged(&viewer, id, &released_entry(&first)) == 1
    })
    .await;
    assert_eq!(
        viewer
            .engine
            .move_session(id, early.clone(), engine::WindowId(4)),
        Err(engine::MoveError::SurfaceInUse)
    );
    release.add_permits(1);
    eventually("live on the surface it moved to", t, || {
        early.state().presented.len() > 5
    })
    .await;
    assert!(
        first.state().presented.is_empty(),
        "never drawn on the first"
    );

    let broken = platform_api::mock::MockSurface::new((640, 360));
    broken.state().refuse_presenter = true;
    viewer
        .engine
        .move_session(id, broken.clone(), engine::WindowId(5))
        .expect("accepted; it fails on the media thread");
    eventually("the broken surface let go", t, || {
        logged(&viewer, id, &released_entry(&broken)) == 1
    })
    .await;
    let n = early.state().presented.len();
    eventually("still presenting where it was", t, || {
        early.state().presented.len() > n
    })
    .await;
    assert!(early.state().has_input_sink, "input stays with it");
    assert!(viewer
        .toasts
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.starts_with("Could not move the remote screen")));
    viewer.engine.end_all_and_wait(Duration::from_secs(3)).await;
}

/// Ending a session while its start is still building it (a slow ICE gather)
/// waits for the start to finish, then ends it for real: its threads are
/// joined before `Released`, its surface is hidden, its sinks are gone and the
/// OS cursor is restored — nothing it built outlives the end (P2 review M2).
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ending_a_session_during_its_start_leaves_nothing_behind() {
    let relay = start_relay().await;
    let host = start_peer("slow-start-host", &relay);
    let viewer = start_peer("slow-start-viewer", &relay);
    let t = Duration::from_secs(20);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.engine.test_delay_viewer_start(1500);
    let id = viewer.open(&host.uuid);
    eventually("accepted, the start under way", t, || {
        viewer
            .session_log
            .lock()
            .unwrap()
            .iter()
            .any(|(s, k)| *s == id && k == "negotiating")
    })
    .await;
    // The offer arrives within the delay; end while the start is building.
    tokio::time::sleep(Duration::from_millis(400)).await;
    viewer.engine.disconnect(id);
    eventually("ended and released", t, || {
        let log = viewer.session_log.lock().unwrap();
        let kinds: Vec<&str> = log
            .iter()
            .filter(|(s, _)| *s == id)
            .map(|(_, k)| k.as_str())
            .collect();
        kinds.contains(&"ended:UserClosed") && kinds.last() == Some(&"released")
    })
    .await;
    {
        let s = viewer.platform.surface.state();
        assert!(!s.visible, "the surface is hidden");
        assert!(!s.cursor_control, "the OS cursor is restored");
    }
    assert!(viewer.engine.viewer_sessions().is_empty());
    eventually("the host's session ended too", t, || {
        matches!(host.engine.role(), Role::Idle)
    })
    .await;

    // The host's ended session must be released too (its threads joined)
    // before it takes a new one: until then it answers "busy" (registry
    // `busy_for`). Its role goes Idle earlier, so wait for the release.
    host.engine.end_all_and_wait(Duration::from_secs(5)).await;

    // Waiting for every session to be released, as Quit does, returns with
    // nothing left running.
    viewer.engine.test_delay_viewer_start(0);
    viewer
        .engine
        .connect(&host.uuid, viewer.platform.surface.clone())
        .expect("released, so free again");
    eventually("connected again", t, || {
        matches!(viewer.engine.role(), Role::Viewer { .. })
    })
    .await;
    viewer.engine.end_all_and_wait(Duration::from_secs(3)).await;
    assert!(viewer.engine.viewer_sessions().is_empty());
    assert!(
        viewer
            .engine
            .connect(&host.uuid, viewer.platform.surface.clone())
            .is_ok(),
        "nothing is still ending"
    );
}

/// The host-screen reports a viewer received, in order (plans/10 U1).
fn host_screens(p: &Peer) -> Vec<String> {
    p.session_log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.starts_with("hostscreen:"))
        .map(|(_, e)| e.clone())
        .collect()
}

const BLOCKED: &str = "hostscreen:blocked::unsupported";
const NORMAL: &str = "hostscreen:normal::";

/// plans/10 U1, flows 10a §5.1/§5.5: the host's screen becomes one it cannot
/// capture (its lock screen) mid-session. The viewer hears `blocked` once —
/// however many times the host retries — hides the picture, and shows
/// nothing from before the lock afterwards (K21); after the local unlock the
/// host says `normal` by itself and the picture comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locked_host_screen_is_explained_and_comes_back_by_itself() {
    const LOCK_AT: u64 = 20;
    let relay = start_relay().await;
    let host = start_peer_with("lock-host", &relay, |p| {
        p.capture.outage = Some(platform_api::mock::MockOutage {
            at: LOCK_AT,
            // ~1.5 s of a lock screen at the loop's 250 ms retry.
            failed_reinits: 6,
            refused: Some(platform_api::BlockReason::NotSupported),
        });
    });
    let viewer = start_peer("lock-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    let surface = viewer.platform.surface.clone();
    eventually("the picture before the lock", t, || {
        !surface.state().presented.is_empty()
    })
    .await;
    eventually("the host says it is locked", t, || {
        host_screens(&viewer).iter().any(|e| e == BLOCKED)
    })
    .await;
    // Everything presented up to now predates the lock.
    let before = surface.state().presented.len();
    eventually("the picture is hidden", t, || !surface.state().visible).await;

    eventually("the host says it is back", t, || {
        host_screens(&viewer).last().map(String::as_str) == Some(NORMAL)
    })
    .await;
    eventually("the picture is back", t, || {
        let s = surface.state();
        s.visible && s.presented.len() > before
    })
    .await;
    // Let the repaint that follows the re-show and a few frames happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let s = surface.state();
        let after: Vec<Option<u64>> = s.presented[before..].iter().map(|p| p.stamp).collect();
        assert!(
            after.iter().all(|st| st.is_some_and(|n| n >= LOCK_AT)),
            "a frame from before the lock was shown again (K21): {after:?}"
        );
    }
    // The first state on the channel, the lock once, and the way back.
    assert_eq!(host_screens(&viewer), [NORMAL, BLOCKED, NORMAL]);
    assert!(
        host_screens(&host).is_empty(),
        "the host's own UI is not told"
    );
}

/// plans/10 U1, flow 10a §5.3 without the agent: connecting to a host that
/// is already on a screen it cannot capture. No frame and no decoder output
/// exist yet, so the message is the only thing the viewer can show, and it
/// arrives (the acceptance asks for 2 s; the queue delivers it as soon as
/// the control channel opens). The picture follows once the screen is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_to_an_already_locked_host_explains_the_missing_picture() {
    let relay = start_relay().await;
    // Five refusals: 100 + 200 + 400 + 800 + 1600 ms of `open_capture`'s
    // back-off, so the channel is open well before the screen is back.
    let host = start_peer_with("locked-host", &relay, |p| {
        p.capture_refusals
            .store(5, std::sync::atomic::Ordering::SeqCst);
    });
    let viewer = start_peer("locked-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    let surface = viewer.platform.surface.clone();
    let asked = Instant::now();
    eventually("the host says it is locked", t, || {
        host_screens(&viewer).iter().any(|e| e == BLOCKED)
    })
    .await;
    let waited = asked.elapsed();
    assert!(
        surface.state().presented.is_empty(),
        "nothing was captured yet"
    );
    eventually("the black surface is hidden", t, || {
        !surface.state().visible
    })
    .await;
    assert!(
        waited < Duration::from_secs(3),
        "the message took {waited:?} (connect + handshake included)"
    );
    eventually("the picture once the screen is back", t, || {
        let s = surface.state();
        s.visible && !s.presented.is_empty()
    })
    .await;
    assert_eq!(host_screens(&viewer), [BLOCKED, NORMAL]);
}

/// K21 against the network: the control channel and video are not ordered
/// against each other, so frames from before the lock can still be on their
/// way when the viewer has already heard both `blocked` and `normal` (seen in
/// CI on a slow machine, where they came a second late). They must not be
/// shown. The test hook hands the last frames from before the lock to the
/// media loop once more after the lock is over, as late arrivals would come;
/// the host's screen stays still, so nothing else arrives.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_from_before_the_lock_that_arrives_after_it_is_not_shown() {
    const LOCK_AT: u64 = 20;
    let relay = start_relay().await;
    let host = start_peer_with("late-host", &relay, |p| {
        p.capture.static_after = Some(LOCK_AT);
        p.capture.outage = Some(platform_api::mock::MockOutage {
            at: LOCK_AT,
            failed_reinits: 4,
            refused: Some(platform_api::BlockReason::NotSupported),
        });
    });
    let viewer = start_peer("late-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    let id = viewer.open(&host.uuid);
    let surface = viewer.platform.surface.clone();
    eventually("the host says it is locked", t, || {
        host_screens(&viewer).iter().any(|e| e == BLOCKED)
    })
    .await;
    eventually("the host says it is back", t, || {
        host_screens(&viewer).last().map(String::as_str) == Some(NORMAL)
    })
    .await;
    eventually("the video is shown again", t, || surface.state().visible).await;
    let before = surface.state().presented.len();
    assert!(before > 0, "the picture before the lock was shown");
    viewer.engine.test_replay_viewer_video(id);
    eventually("the late frames were handed on", t, || {
        viewer.engine.test_viewer_video_replayed(id) > 0
    })
    .await;
    // Time for the media loop to take them, and for the re-show's repaint.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = surface.state();
    let after: Vec<Option<u64>> = s.presented[before..].iter().map(|p| p.stamp).collect();
    assert!(
        after.iter().all(|st| st.is_some_and(|n| n >= LOCK_AT)),
        "a frame from before the lock was shown after it (K21): {after:?}"
    );
}

/// K21 at its sharpest: after the unlock the host's screen does not change,
/// so no new frame arrives and the re-shown video has only what it held. It
/// must show nothing rather than the picture from before the lock (which
/// could be a different user's desktop, or a document closed meanwhile).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_from_before_the_lock_is_never_shown_again() {
    const LOCK_AT: u64 = 20;
    let relay = start_relay().await;
    let host = start_peer_with("still-host", &relay, |p| {
        // Still after the lock: an idle desktop sends nothing new.
        p.capture.static_after = Some(LOCK_AT);
        p.capture.outage = Some(platform_api::mock::MockOutage {
            at: LOCK_AT,
            failed_reinits: 4,
            refused: Some(platform_api::BlockReason::NotSupported),
        });
    });
    let viewer = start_peer("still-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    viewer.open(&host.uuid);
    let surface = viewer.platform.surface.clone();
    eventually("the host says it is locked", t, || {
        host_screens(&viewer).iter().any(|e| e == BLOCKED)
    })
    .await;
    let before = surface.state().presented.len();
    assert!(before > 0, "the picture before the lock was shown");
    eventually("the host says it is back", t, || {
        host_screens(&viewer).last().map(String::as_str) == Some(NORMAL)
    })
    .await;
    eventually("the video is shown again", t, || surface.state().visible).await;
    // The re-show's repaint (D12) and cursor moves (D11) run every 16 ms.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = surface.state();
    let after: Vec<Option<u64>> = s.presented[before..].iter().map(|p| p.stamp).collect();
    assert!(
        after.iter().all(|st| st.is_some_and(|n| n >= LOCK_AT)),
        "a frame from before the lock was repainted (K21): {after:?}"
    );
}

/// K13 (review decision): the screen state belongs to the session. With the
/// main stream paused (its tab in the background) and a pop-out window open,
/// the pop-out's capture speaks for the host: its screen blocks, the viewer
/// hears `blocked` and the pop-out is hidden — no pre-lock picture stays up —
/// and the unlock brings `normal` and the pop-out back.
#[cfg(feature = "test-hooks")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_popout_speaks_for_the_host_while_the_main_stream_is_paused() {
    let relay = start_relay().await;
    let second = platform_api::DisplayInfo {
        id: "mock-1".into(),
        name: "Second".into(),
        x: 640,
        y: 0,
        width: 800,
        height: 600,
        scale: 1.0,
        primary: false,
    };
    let host = start_peer_with("paused-host", &relay, |p| {
        p.extra_displays.push(second.clone());
        // Only the pop-out's capture loses its screen: the paused main
        // stream would not see it before it acquires again.
        p.outage_on = Some("mock-1".into());
        p.capture.outage = Some(platform_api::mock::MockOutage {
            // ~3 s into the pop-out's stream: time to pause the main one.
            at: 90,
            failed_reinits: 6,
            refused: Some(platform_api::BlockReason::NotSupported),
        });
    });
    let viewer = start_peer("paused-viewer", &relay);
    let t = Duration::from_secs(15);
    eventually("both registered", t, || {
        host.engine.server_connected() && viewer.engine.server_connected()
    })
    .await;
    let id = viewer.open(&host.uuid);
    eventually("main window presenting", t, || {
        viewer.last_displays().is_some_and(|d| d.2)
            && !viewer.platform.surface.state().presented.is_empty()
    })
    .await;
    viewer.engine.popout_display(id, "mock-1".into());
    eventually("pop-out announced", t, || {
        *viewer.popouts.lock().unwrap() == vec![(1, "mock-1".to_string())]
    })
    .await;
    let pop = platform_api::mock::MockSurface::new((800, 600));
    assert!(viewer.engine.attach_popout(id, 1, pop.clone()).is_some());
    eventually("the pop-out presents", t, || {
        let s = pop.state();
        s.visible && !s.presented.is_empty()
    })
    .await;
    // The tab goes to the background: the host pauses the main stream.
    viewer.engine.set_screen(&[], None);
    eventually("main stream paused", t, || {
        host.engine.test_video_paused() == Some(true)
    })
    .await;
    assert_eq!(host_screens(&viewer), [NORMAL], "nothing blocked yet");

    eventually("the pop-out's screen blocks", t, || {
        host_screens(&viewer).iter().any(|e| e == BLOCKED)
    })
    .await;
    eventually("the pop-out is hidden", t, || !pop.state().visible).await;
    eventually("the host says it is back", t, || {
        host_screens(&viewer).last().map(String::as_str) == Some(NORMAL)
    })
    .await;
    eventually("the pop-out is shown again", t, || pop.state().visible).await;
    assert_eq!(host_screens(&viewer), [NORMAL, BLOCKED, NORMAL]);
}
