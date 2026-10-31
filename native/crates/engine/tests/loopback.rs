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
use engine::{Engine, IcePolicy, Role, UiEvent};
use futures_util::{SinkExt, StreamExt};
use platform_api::mock::MockPlatform;
use platform_api::{Clipboard, FileDrop, VideoInput};
use protocol::{Config, InputMsg, Permission};
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

struct Peer {
    engine: Arc<Engine>,
    platform: Arc<MockPlatform>,
    uuid: String,
    toasts: Arc<std::sync::Mutex<Vec<String>>>,
    /// Every role the engine reported, in order.
    roles: Arc<std::sync::Mutex<Vec<Role>>>,
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
    let root = temp_root(name);
    let platform = Arc::new(MockPlatform::new(&root));
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

    let (engine, mut ui_rx, mut sig_rx) = Engine::start(platform.clone(), Arc::new(AllowControl));
    engine.set_ice_policy(IcePolicy::LoopbackOnly);
    engine.attach_surface(platform.surface.clone());

    let e = engine.clone();
    tokio::spawn(async move {
        while let Some(ev) = sig_rx.recv().await {
            e.handle_signaling(ev).await;
        }
    });
    let toasts: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let roles: Arc<std::sync::Mutex<Vec<Role>>> = Arc::default();
    let (t, r) = (toasts.clone(), roles.clone());
    tokio::spawn(async move {
        while let Some(ev) = ui_rx.recv().await {
            match ev {
                UiEvent::Toast(msg) => t.lock().unwrap().push(msg),
                UiEvent::RoleChanged(role) => r.lock().unwrap().push(role),
                _ => {}
            }
        }
    });
    Peer {
        engine,
        platform,
        uuid,
        toasts,
        roles,
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

fn injected(p: &Peer) -> Vec<InputMsg> {
    p.platform.injector.lock().unwrap().events.clone()
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

// ---- the test -------------------------------------------------------------------

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
    viewer.engine.connect_to(host.uuid.clone());
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
        assert!(
            stamps.windows(2).all(|w| w[0] < w[1]),
            "frames arrive in order without repeats: {stamps:?}"
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
    viewer.engine.end_session();
    eventually("both idle", t, || {
        host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
    })
    .await;
    assert!(
        !viewer.platform.surface.state().visible,
        "surface hidden after the session"
    );
    let toasts = host.toasts.lock().unwrap().clone();
    assert!(
        !toasts.iter().any(|t| t.contains("unavailable")),
        "no capability errors on the host: {toasts:?}"
    );
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
