//! One remote-control session (one at a time): WebRTC setup, the session's
//! threads, and teardown. The host offers and streams; the viewer answers,
//! decodes, presents and sends input.
//!
//! Threads per session (all named `sct-*`, all stopped by one flag and joined in
//! [`teardown`]):
//! * transport — owns the str0m `Rtc` + UDP socket ([`driver`]),
//! * host media (capture → encode) or viewer media (decode → present),
//! * clipboard sync,
//! * viewer only: input translation, file push, and optionally shortcut capture.

mod clipboard;
mod driver;
mod files;
mod ice;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use platform_api::{FileDrop, KeyForwarder, Rect, VideoInput};
use protocol::{ControlMsg, InputMsg, Permission, SignalData, SignalMsg};

use crate::media::host::{host_media_loop, HostMedia};
use crate::media::viewer::{viewer_media_loop, ViewerMedia};
use crate::modifiers::{KeyTranslator, OsFamily};
use crate::{Engine, Role};

pub(crate) use crate::tuning::{DESIRED_BITRATE, MIN_BITRATE, TARGET_BITRATE};

/// State shared by one session's threads. Per session (not process-wide) so two
/// engines in one process — the loopback test — never see each other's state.
#[derive(Default)]
pub(crate) struct Shared {
    /// Host: the viewer's display size `w << 32 | h` (0 = unknown). The host
    /// encodes to it so the viewer presents 1:1.
    pub view_size: AtomicU64,
    /// Viewer: latest cursor from the host, drawn as a sprite by the presenter.
    pub cursor: Mutex<Option<ViewerPointer>>,
    /// Viewer: the host's cursor images received so far (task D9).
    pub cursor_images: Mutex<crate::media::cursor::ViewerCursors>,
}

/// The pointer as the last position message described it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ViewerPointer {
    /// Hotspot, normalized `[0,1]` over the video.
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    /// The image to draw (an id in [`Shared::cursor_images`]); `None` = the
    /// presenter's built-in arrow.
    pub shape: Option<u32>,
}

/// The live session, owned by [`Engine`].
pub(crate) struct Session {
    peer: String,
    stop: Arc<AtomicBool>,
    /// Inbound answer/ICE from signaling → transport thread.
    signal_tx: Sender<SignalData>,
    /// Outbound control (perm / input / clipboard) → transport thread.
    ctl_tx: Sender<Vec<u8>>,
    /// Host: whether injecting remote input is allowed right now (the live
    /// `control` permission). Revoking releases held keys/buttons.
    control: Arc<AtomicBool>,
    /// Whether clipboard sync is live. Follows control (and the user setting), so
    /// revoking control also stops clipboard exchange.
    clip_on: Arc<AtomicBool>,
    /// The user's clipboard-sync setting at session start.
    clipboard_setting: bool,
    threads: Vec<JoinHandle<()>>,
}

pub(crate) fn serialize(msg: &ControlMsg) -> Vec<u8> {
    serde_json::to_vec(msg).unwrap_or_default()
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("sct-{name}"))
        .spawn(f)
}

/// Spawn a session thread or abandon the session: stop and join what already
/// started, end the session with a message. (A failed spawn means the OS is out
/// of threads; panicking would abort the whole app in release builds.)
macro_rules! spawn_or_abort {
    ($engine:expr, $stop:expr, $threads:expr, $name:literal, $body:expr) => {
        match spawn($name, $body) {
            Ok(h) => $threads.push(h),
            Err(e) => {
                tracing::error!("could not start the {} thread: {e}", $name);
                $stop.store(true, Ordering::SeqCst);
                for t in $threads.drain(..) {
                    let _ = t.join();
                }
                $engine.fail_session("the session could not be started");
                return;
            }
        }
    };
}

/// The rectangle input should map onto when sharing a non-primary display.
fn target_rect_for(engine: &Engine, display: Option<&str>) -> Option<Rect> {
    let id = display?;
    let d = engine
        .platform()
        .displays()
        .into_iter()
        .find(|d| d.id == id)?;
    (!d.primary).then_some(Rect {
        x: d.x,
        y: d.y,
        w: d.width,
        h: d.height,
    })
}

// ---- host ------------------------------------------------------------------

/// Host: build the offer and data channels, start the transport, capture →
/// encode and clipboard threads, then send the offer to `peer`.
pub(crate) fn begin_host(engine: &Engine, peer: String, permission: Permission) {
    teardown(engine);
    let platform = engine.platform();
    let cfg = engine.config();
    let codec = engine.negotiated_codec();
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Offer exactly the negotiated codec, so the media track's payload type is
    // unambiguous. BWE (congestion control) steers the encoder bitrate.
    let mut rtc = {
        let builder = str0m::RtcConfig::new()
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(TARGET_BITRATE as u64)))
            .clear_codecs();
        let builder = match codec {
            platform_api::Codec::Hevc => builder.enable_h265(true),
            platform_api::Codec::Av1 => builder.enable_av1(true),
            platform_api::Codec::H264 => builder.enable_h264(true),
        };
        builder.build(Instant::now())
    };
    // Bind + gather candidates BEFORE generating the offer so it carries them.
    let ice = ice::ice_config_from(engine);
    let Some((socket, turn_alloc)) = ice::bind_and_gather(&mut rtc, &ice) else {
        tracing::error!("host: failed to bind a UDP socket");
        engine.fail_session("no network socket available");
        return;
    };
    // Data channels (ctl reliable, cursor unreliable-latest, bulk reliable);
    // video is an RTP media track.
    let channels = {
        let [c, cur, blk] = transport::channel_configs();
        let mut dapi = rtc.direct_api();
        let ctl = dapi.create_data_channel(c);
        let cursor = dapi.create_data_channel(cur);
        let bulk = dapi.create_data_channel(blk);
        transport::Channels { ctl, cursor, bulk }
    };
    let mut api = rtc.sdp_api();
    let video_mid = api.add_media(
        str0m::media::MediaKind::Video,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    // One SDP-negotiated channel forces the m=application section (the SCTP
    // association the direct channels ride on). Its label is never used.
    api.add_channel("init".to_string());
    let Some((offer, pending)) = api.apply() else {
        tracing::error!("host: str0m produced no offer");
        engine.fail_session("the connection could not be set up");
        return;
    };

    let control = Arc::new(AtomicBool::new(permission == Permission::Control));
    let bitrate = Arc::new(AtomicU32::new(TARGET_BITRATE));
    let link = Arc::new(AtomicU32::new(TARGET_BITRATE));
    let force_key = Arc::new(AtomicBool::new(false));
    // Clipboard sync is gated on the SAME live permission as input: a view-only
    // peer must not read this machine's clipboard (it can hold a password that
    // was never on screen) or write to it.
    let clip_on = Arc::new(AtomicBool::new(
        cfg.clipboard_sync && permission == Permission::Control,
    ));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    let mut threads = Vec::new();

    {
        let (platform, ctl, gate, stop_c) = (
            platform.clone(),
            ctl_tx.clone(),
            clip_on.clone(),
            stop.clone(),
        );
        spawn_or_abort!(engine, stop, threads, "clipboard", move || {
            clipboard::clipboard_loop(platform, ctl, clip_rx, gate, stop_c)
        });
    }

    let driver = driver::Driver {
        rtc,
        pending: Some(pending),
        signal_rx,
        ctl_rx,
        frame_rx: Some(frame_rx),
        video_tx: None,
        inject: Some(control.clone()),
        cursor_rx: Some(cursor_rx),
        bulk_rx: None,
        clip_tx,
        channels: Some(channels),
        video_mid: Some(video_mid),
        force_key: Some(force_key.clone()),
        bitrate: Some(bitrate.clone()),
        link: Some(link.clone()),
        ui: engine.ui_sender(),
        role: engine.role_handle(),
        socket,
        turn: turn_alloc,
        stop: stop.clone(),
        platform: platform.clone(),
        surface: None,
        shared: shared.clone(),
        downloads_dir: platform.paths().downloads_dir,
        paste_dropped: cfg.paste_dropped_files,
        target_rect: target_rect_for(engine, cfg.share_display_id.as_deref()),
    };
    spawn_or_abort!(engine, stop, threads, "transport", move || {
        driver::transport_driver(driver)
    });

    {
        let media = HostMedia {
            platform,
            display: cfg.share_display_id.clone(),
            codec,
            shared,
            ctl_tx: ctl_tx.clone(),
        };
        let stop_m = stop.clone();
        let ui = engine.ui_sender();
        spawn_or_abort!(engine, stop, threads, "host-media", move || {
            if let Err(e) =
                host_media_loop(media, frame_tx, cursor_tx, bitrate, link, force_key, stop_m)
            {
                tracing::warn!("host media loop ended: {e}");
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "Screen sharing stopped: {e}"
                )));
            }
        });
    }

    // The initial permission, delivered once the ctl channel opens (§4.2).
    let _ = ctl_tx.send(serialize(&ControlMsg::Perm { value: permission }));

    // Register the session BEFORE the offer leaves: the answer must find it.
    *engine.session.lock() = Some(Session {
        peer: peer.clone(),
        stop,
        signal_tx,
        ctl_tx,
        control,
        clip_on,
        clipboard_setting: cfg.clipboard_sync,
        threads,
    });
    let _ = engine.signaling().send(SignalMsg::Signal {
        to: Some(peer),
        from: None,
        data: SignalData::Offer {
            sdp: offer.to_sdp_string(),
        },
    });
}

// ---- viewer ------------------------------------------------------------------

/// Route an inbound WebRTC payload. An offer starts a viewer session — but only
/// from the host this engine is connected to as a viewer; anything else is
/// forwarded to the live session with that same peer.
pub(crate) fn on_signal(engine: &Engine, peer: &str, data: SignalData) {
    match data {
        SignalData::Offer { sdp } => {
            let expected = matches!(
                engine.role(),
                Role::Viewer { peer: ref p, .. } if p == peer
            );
            if !expected {
                tracing::warn!("ignoring a WebRTC offer from {peer}: not the host we connected to");
                return;
            }
            start_viewer(engine, peer.to_string(), sdp);
        }
        other => match engine.session.lock().as_ref() {
            Some(s) if s.peer == peer => {
                let _ = s.signal_tx.send(other);
            }
            Some(_) => tracing::warn!("ignoring signal from {peer}: not the session peer"),
            None => tracing::debug!("signal from {peer} with no session — dropped"),
        },
    }
}

fn start_viewer(engine: &Engine, peer: String, offer_sdp: String) {
    // An offer always means a new session (the host restarts by re-offering).
    teardown(engine);
    let Some(surface) = engine.surface() else {
        tracing::error!("viewer: no video surface on this platform");
        engine.fail_session("this computer cannot display the remote screen yet");
        return;
    };
    let platform = engine.platform();
    let cfg = engine.config();
    let codec = engine.negotiated_codec();
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (video_tx, video_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();

    let mut rtc = str0m::Rtc::new(Instant::now());
    let ice = ice::ice_config_from(engine);
    let Some((socket, turn_alloc)) = ice::bind_and_gather(&mut rtc, &ice) else {
        tracing::error!("viewer: failed to bind a UDP socket");
        engine.fail_session("no network socket available");
        return;
    };
    let answer = match str0m::change::SdpOffer::from_sdp_string(&offer_sdp)
        .map_err(|e| e.to_string())
        .and_then(|offer| rtc.sdp_api().accept_offer(offer).map_err(|e| e.to_string()))
    {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!("viewer: could not accept the host's offer: {e}");
            engine.fail_session("the connection could not be set up");
            return;
        }
    };

    let mut threads = Vec::new();

    // File push: drops on the video surface are streamed as bulk frames from
    // their own thread, so a slow disk stays off the transport loop.
    let (bulk_tx, bulk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (drop_tx, drop_rx) = std::sync::mpsc::channel::<FileDrop>();
    {
        let (ui, stop_f) = (engine.ui_sender(), stop.clone());
        spawn_or_abort!(engine, stop, threads, "file-push", move || {
            files::file_send_loop(drop_rx, bulk_tx, ui, stop_f)
        });
    }

    // Clipboard sync. The host applies its own permission gate; here the user's
    // setting governs.
    let clip_on = Arc::new(AtomicBool::new(cfg.clipboard_sync));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    {
        let (platform, ctl, gate, stop_c) = (
            platform.clone(),
            ctl_tx.clone(),
            clip_on.clone(),
            stop.clone(),
        );
        spawn_or_abort!(engine, stop, threads, "clipboard", move || {
            clipboard::clipboard_loop(platform, ctl, clip_rx, gate, stop_c)
        });
    }

    // Every forwarded key — from the surface and from the shortcut hook — goes
    // through this one translator, which rewrites modifiers for the host's OS
    // and remembers what it sent for each held key (D8).
    let keys = {
        let viewer_os = OsFamily::from_name(platform.info().name);
        let host_os = engine.peer_os();
        tracing::info!("key translation: {viewer_os:?} viewer → {host_os:?} host");
        Arc::new(Mutex::new(KeyTranslator::new(
            viewer_os,
            host_os,
            engine.translate_modifiers_handle(),
        )))
    };

    // Input from the video surface → protocol messages on ctl. The host enforces
    // its own permission (defence in depth).
    let (input_tx, input_rx) = std::sync::mpsc::channel::<VideoInput>();
    {
        let (ctl, keys, stop_i) = (ctl_tx.clone(), keys.clone(), stop.clone());
        spawn_or_abort!(engine, stop, threads, "input", move || {
            input_loop(input_rx, ctl, keys, stop_i)
        });
    }

    let driver = driver::Driver {
        rtc,
        pending: None,
        signal_rx,
        ctl_rx,
        frame_rx: None,
        video_tx: Some(video_tx),
        inject: None,
        cursor_rx: None,
        bulk_rx: Some(bulk_rx),
        clip_tx,
        channels: None,  // learned from ChannelOpen by label
        video_mid: None, // learned from Event::MediaAdded
        force_key: None,
        bitrate: None,
        link: None,
        ui: engine.ui_sender(),
        role: engine.role_handle(),
        socket,
        turn: turn_alloc,
        stop: stop.clone(),
        platform: platform.clone(),
        surface: Some(surface.clone()),
        shared: shared.clone(),
        downloads_dir: platform.paths().downloads_dir,
        paste_dropped: false,
        target_rect: None,
    };
    spawn_or_abort!(engine, stop, threads, "transport", move || {
        driver::transport_driver(driver)
    });

    {
        let media = ViewerMedia {
            platform: platform.clone(),
            surface: surface.clone(),
            codec,
            shared,
        };
        let (ctl, stop_r, ui) = (ctl_tx.clone(), stop.clone(), engine.ui_sender());
        spawn_or_abort!(engine, stop, threads, "viewer-media", move || {
            if let Err(e) = viewer_media_loop(media, video_rx, ctl, stop_r) {
                tracing::warn!("viewer media loop ended: {e}");
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "Cannot display the remote screen: {e}"
                )));
            }
        });
    }

    // OS shortcut capture (Win/Alt+Tab, Cmd+Tab, Super), when the user enabled it.
    if cfg.capture_shortcuts {
        let (platform, surface, ctl, keys, stop_k, hook_input) = (
            platform.clone(),
            surface.clone(),
            ctl_tx.clone(),
            keys.clone(),
            stop.clone(),
            input_tx.clone(),
        );
        spawn_or_abort!(engine, stop, threads, "shortcuts", move || {
            let Some(mut hook) = platform.create_shortcut_hook() else {
                tracing::info!("shortcut capture is not available on this platform");
                return;
            };
            let forward = hook_forwarder(hook_input, keys, ctl);
            if hook.install(surface.as_ref(), forward) {
                hook.run(&stop_k);
            } else {
                tracing::info!("shortcut capture could not be installed");
            }
            hook.uninstall();
        });
    }

    surface.set_drop_sink(Some(drop_tx));
    surface.set_input_sink(Some(input_tx));
    *engine.session.lock() = Some(Session {
        peer: peer.clone(),
        stop,
        signal_tx,
        ctl_tx,
        control: Arc::new(AtomicBool::new(false)),
        clip_on,
        clipboard_setting: cfg.clipboard_sync,
        threads,
    });
    let _ = engine.signaling().send(SignalMsg::Signal {
        to: Some(peer),
        from: None,
        data: SignalData::Answer {
            sdp: answer.to_sdp_string(),
        },
    });
}

/// Queue one input message for the host on the ctl channel.
fn send_input(ctl: &Sender<Vec<u8>>, msg: &InputMsg) {
    let _ = ctl.send(serde_json::to_vec(msg).unwrap_or_default());
}

/// The shortcut hook's forwarder: its keys join the surface's input channel,
/// so the translator sees hook keys and surface keys in the order they were
/// produced (M12).
///
/// WHY not lock the translator directly from the hook thread (D8's first
/// version): the surface's keys wait in `input` (behind a pointer move the
/// input thread may be pacing for up to 10 ms), so a Cmd sent by the surface
/// could reach the translator **after** the Cmd+C that the hook took a moment
/// later, and the host would get C, then Ctrl. The hook guarantees that the
/// modifier is in the channel before its own key (it forwards the modifier
/// itself unless the surface already did); one FIFO channel turns that into
/// the order the host sees. A channel send never blocks, as the hook's OS
/// callback requires.
///
/// Fallback to the translator directly: a code outside the shared keymap
/// (cannot be a `&'static str` for [`VideoInput::Key`]), or the input thread
/// already gone at session end (a release must still reach the host).
fn hook_forwarder(
    input: Sender<VideoInput>,
    keys: Arc<Mutex<KeyTranslator>>,
    ctl: Sender<Vec<u8>>,
) -> KeyForwarder {
    Box::new(move |code: &str, down: bool| {
        if let Some(code) = platform_api::keymap::from_dom(code).map(platform_api::keymap::dom_for)
        {
            if input.send(VideoInput::Key { code, down }).is_ok() {
                return;
            }
        }
        keys.lock()
            .key(code, down, &mut |msg| send_input(&ctl, &msg));
    })
}

/// Translate surface input to protocol messages, coalescing pointer moves.
/// Keys go through `keys` (modifier translation for the host's OS).
///
/// Gaming mice emit up to 1000 moves/s; relayed over the reliable ordered
/// channel they would queue AHEAD of clicks and keys and make input feel drunk on
/// slow links. Moves are capped at ~100/s and always carry the LATEST position;
/// clicks, keys and wheel are never delayed beyond the coalescing drain.
fn input_loop(
    input_rx: std::sync::mpsc::Receiver<VideoInput>,
    ctl: Sender<Vec<u8>>,
    keys: Arc<Mutex<KeyTranslator>>,
    stop: Arc<AtomicBool>,
) {
    const MOVE_INTERVAL: Duration = Duration::from_millis(10);
    let send = |ev: VideoInput| {
        if let VideoInput::Key { code, down } = ev {
            keys.lock()
                .key(code, down, &mut |msg| send_input(&ctl, &msg));
        } else if let Some(msg) = translate_input(ev) {
            send_input(&ctl, &msg);
        }
    };
    let mut last_move_sent = Instant::now() - MOVE_INTERVAL;
    while !stop.load(Ordering::SeqCst) {
        match input_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(mut ev) => {
                let mut follow: Option<VideoInput> = None;
                if matches!(ev, VideoInput::Move { .. }) {
                    let since = last_move_sent.elapsed();
                    if since < MOVE_INTERVAL {
                        std::thread::sleep(MOVE_INTERVAL - since);
                    }
                    while let Ok(next) = input_rx.try_recv() {
                        if matches!(next, VideoInput::Move { .. }) {
                            ev = next; // newer position supersedes
                        } else {
                            follow = Some(next);
                            break;
                        }
                    }
                    last_move_sent = Instant::now();
                }
                send(ev);
                if let Some(f) = follow {
                    send(f);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Keys still queued when the session stops (the surface's and the
    // shortcut hook's releases at teardown) go out anyway: a dropped key-up
    // would leave that key held on the host until it notices the disconnect.
    for ev in input_rx.try_iter() {
        if matches!(ev, VideoInput::Key { .. }) {
            send(ev);
        }
    }
}

/// Pointer and wheel input → wire message. Buttons outside 0..=2 are dropped.
/// Keys are not mapped here: they need the session's held-key state, which
/// lives in [`KeyTranslator`], so `None`.
fn translate_input(ev: VideoInput) -> Option<InputMsg> {
    use protocol::Button;
    Some(match ev {
        VideoInput::Move { nx, ny } => InputMsg::Move { x: nx, y: ny },
        VideoInput::Button {
            button,
            down,
            nx,
            ny,
        } => {
            let b = Button::try_from(button).ok()?;
            if down {
                InputMsg::ButtonDown { b, x: nx, y: ny }
            } else {
                InputMsg::ButtonUp { b, x: nx, y: ny }
            }
        }
        VideoInput::Wheel { dx, dy } => InputMsg::Wheel { dx, dy },
        VideoInput::Key { .. } => return None,
    })
}

// ---- session control -----------------------------------------------------------

/// Send a control message (perm change, keyframe request) on the ctl channel.
pub(crate) fn send_ctl(engine: &Engine, msg: &ControlMsg) {
    if let Some(s) = engine.session.lock().as_ref() {
        let _ = s.ctl_tx.send(serialize(msg));
    }
}

/// Host: flip the injection gate when the live permission changes (§4.2). The
/// transport thread releases held input when control is revoked.
pub(crate) fn set_control(engine: &Engine, allow: bool) {
    if let Some(s) = engine.session.lock().as_ref() {
        s.control.store(allow, Ordering::SeqCst);
        // Clipboard exchange is part of "control": revoking it must stop the peer
        // reading this machine's clipboard too, not just its input.
        s.clip_on
            .store(allow && s.clipboard_setting, Ordering::SeqCst);
    }
}

/// End the session: stop capturing input, stop and join every thread, hide the
/// video. Idempotent.
pub(crate) fn teardown(engine: &Engine) {
    let surface = engine.surface();
    if let Some(s) = &surface {
        // Releases held keys first (surface contract), then stops capturing.
        s.set_input_sink(None);
        s.set_drop_sink(None);
    }
    // Take the session out BEFORE joining, so the lock is not held while threads
    // that may need it (signal routing) wind down.
    let session = engine.session.lock().take();
    if let Some(session) = session {
        session.stop.store(true, Ordering::SeqCst);
        for t in session.threads {
            let _ = t.join();
        }
    }
    if let Some(s) = &surface {
        s.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_keys() -> Arc<Mutex<KeyTranslator>> {
        Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Windows,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )))
    }

    fn sent(ctl_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<InputMsg> {
        ctl_rx
            .try_iter()
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect()
    }

    #[test]
    fn input_loop_sends_keys_through_the_translator() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        for (code, down) in [("MetaLeft", true), ("KeyC", true), ("KeyC", false)] {
            tx.send(VideoInput::Key { code, down }).unwrap();
        }
        drop(tx);
        let keys = Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Mac,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )));
        input_loop(rx, ctl_tx, keys, Arc::new(AtomicBool::new(false)));
        let codes: Vec<(String, bool)> = sent(&ctl_rx)
            .into_iter()
            .map(|m| match m {
                InputMsg::KeyDown { code } => (code, true),
                InputMsg::KeyUp { code } => (code, false),
                other => panic!("not a key: {other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            [
                ("ControlLeft".to_string(), true),
                ("KeyC".to_string(), true),
                ("KeyC".to_string(), false)
            ]
        );
    }

    fn key_codes(ctl_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<(String, bool)> {
        sent(ctl_rx)
            .into_iter()
            .map(|m| match m {
                InputMsg::KeyDown { code } => (code, true),
                InputMsg::KeyUp { code } => (code, false),
                other => panic!("not a key: {other:?}"),
            })
            .collect()
    }

    fn mac_to_windows_keys() -> Arc<Mutex<KeyTranslator>> {
        Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Mac,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )))
    }

    /// M12: Cmd travels the surface's path, Cmd+C's C the hook's. Queued in
    /// that order, the translator must see them in that order even though
    /// the input thread has not run yet when the hook forwards C.
    #[test]
    fn hook_keys_queue_behind_the_surface_keys_sent_before_them() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let keys = mac_to_windows_keys();
        let forward = hook_forwarder(tx.clone(), keys.clone(), ctl_tx.clone());
        tx.send(VideoInput::Key {
            code: "MetaLeft",
            down: true,
        })
        .unwrap();
        forward("KeyC", true);
        forward("KeyC", false);
        tx.send(VideoInput::Key {
            code: "MetaLeft",
            down: false,
        })
        .unwrap();
        // Nothing reached the host yet: the hook did not jump the queue.
        assert!(key_codes(&ctl_rx).is_empty());
        drop((tx, forward));
        input_loop(rx, ctl_tx, keys, Arc::new(AtomicBool::new(false)));
        assert_eq!(
            key_codes(&ctl_rx),
            [
                ("ControlLeft".to_string(), true),
                ("KeyC".to_string(), true),
                ("KeyC".to_string(), false),
                ("ControlLeft".to_string(), false)
            ]
        );
    }

    #[test]
    fn hook_keys_still_reach_the_host_once_the_input_thread_is_gone() {
        let (tx, rx) = std::sync::mpsc::channel::<VideoInput>();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        drop(rx);
        let forward = hook_forwarder(tx, mac_to_windows_keys(), ctl_tx);
        forward("MetaLeft", false);
        // A code outside the shared keymap goes straight to the translator.
        forward("NotAKey", true);
        assert_eq!(
            key_codes(&ctl_rx),
            [
                ("ControlLeft".to_string(), false),
                ("NotAKey".to_string(), true)
            ]
        );
    }

    #[test]
    fn keys_queued_when_the_session_stops_are_still_sent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        tx.send(VideoInput::Move { nx: 0.5, ny: 0.5 }).unwrap();
        tx.send(VideoInput::Key {
            code: "KeyA",
            down: false,
        })
        .unwrap();
        // Already stopped: the loop body never runs, only the drain.
        input_loop(rx, ctl_tx, identity_keys(), Arc::new(AtomicBool::new(true)));
        assert_eq!(key_codes(&ctl_rx), [("KeyA".to_string(), false)]);
    }

    #[test]
    fn translate_input_maps_every_variant() {
        assert!(translate_input(VideoInput::Key {
            code: "KeyA",
            down: true
        })
        .is_none());
        assert!(matches!(
            translate_input(VideoInput::Button {
                button: 2,
                down: false,
                nx: 0.5,
                ny: 0.25
            }),
            Some(InputMsg::ButtonUp {
                b: protocol::Button::Right,
                ..
            })
        ));
        assert!(translate_input(VideoInput::Button {
            button: 7,
            down: true,
            nx: 0.0,
            ny: 0.0
        })
        .is_none());
        assert!(matches!(
            translate_input(VideoInput::Wheel { dx: 0, dy: -120 }),
            Some(InputMsg::Wheel { dx: 0, dy: -120 })
        ));
    }

    #[test]
    fn input_loop_coalesces_moves_but_keeps_clicks_in_order() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        for i in 0..50 {
            tx.send(VideoInput::Move {
                nx: i as f64 / 100.0,
                ny: 0.0,
            })
            .unwrap();
        }
        tx.send(VideoInput::Button {
            button: 0,
            down: true,
            nx: 0.49,
            ny: 0.0,
        })
        .unwrap();
        drop(tx);
        input_loop(rx, ctl_tx, identity_keys(), stop);
        let msgs: Vec<InputMsg> = ctl_rx
            .try_iter()
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect();
        // The 50 moves collapse into very few, and the newest position wins.
        let moves: Vec<f64> = msgs
            .iter()
            .filter_map(|m| match m {
                InputMsg::Move { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert!(moves.len() <= 2, "{moves:?}");
        assert_eq!(moves.last().copied(), Some(0.49));
        assert!(matches!(msgs.last(), Some(InputMsg::ButtonDown { .. })));
    }
}
