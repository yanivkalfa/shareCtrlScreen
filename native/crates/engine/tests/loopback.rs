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
    start_peer_on(name, relay, "mock")
}

/// A peer whose mock platform reports `os` as its [`PlatformInfo::name`].
///
/// [`PlatformInfo::name`]: platform_api::PlatformInfo::name
fn start_peer_on(name: &str, relay: &str, os: &'static str) -> Peer {
    let root = temp_root(name);
    let mut platform = MockPlatform::new(&root);
    platform.os_name = os;
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
    viewer.engine.connect_to(host.uuid.clone());
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
    viewer
        .engine
        .update_config(|c| c.translate_modifiers = false);
    press(&viewer, &host, &["-MetaLeft"], &["-ControlLeft"]).await;
    press(
        &viewer,
        &host,
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
        &["+MetaLeft", "+KeyC", "-KeyC", "-MetaLeft"],
    )
    .await;
    // And back on: translated again, and nothing is left held on the host.
    viewer
        .engine
        .update_config(|c| c.translate_modifiers = true);
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
    viewer.engine.end_session();
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
    viewer.engine.end_session();
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
        viewer.engine.end_session();
        eventually("both idle", Duration::from_secs(15), || {
            host.engine.role() == Role::Idle && viewer.engine.role() == Role::Idle
        })
        .await;
    }
}
