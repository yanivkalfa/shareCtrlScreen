//! Windows media + transport pipeline (Plan 04 §2 architecture, §5–§7). Wires
//! `capture`→`codec`→`transport` on the host and `transport`→`codec`→`render`
//! on the viewer, plus `input` injection. str0m owns the sans-IO WebRTC loop on
//! a dedicated thread; the COM-bound media stages each run on their own thread
//! with their own D3D11 device (COM interfaces are not `Send`), communicating
//! over byte channels.
//!
//! This module is the integration surface for the §12 latency smoke-test
//! (`capture → encode → transport → decode → render`), the single go/no-go for
//! the whole native-rewrite premise; the fine timing of the str0m↔UDP driver and
//! the encoder event pump are validated on target hardware there.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use protocol::{ControlMsg, Permission, SignalData};

use crate::Engine;

/// The viewer's native video **child** HWND (Option A, §7), created under the
/// Tauri window by [`create_video_window`].
static RENDER_HWND: AtomicIsize = AtomicIsize::new(0);

/// The single active session (one at a time, contract §1).
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Latest out-of-band cursor position the viewer received (normalized, visible),
/// drawn by the render loop as a client-side sprite (§5a/§7).
static CURSOR: Mutex<Option<(f64, f64, bool)>> = Mutex::new(None);

/// The app calls this once, passing the Tauri main-window HWND. We create a
/// native D3D11 child window under it (§7 Option A) and remember its handle; the
/// swapchain is created on the child, never on the WebView2 window itself.
pub fn create_video_window(parent_hwnd: isize) {
    let parent = windows::Win32::Foundation::HWND(parent_hwnd as *mut _);
    match render::VideoWindow::create(parent) {
        Ok(w) => {
            RENDER_HWND.store(w.hwnd_raw(), Ordering::SeqCst);
            tracing::info!("native video child window created");
            // The window persists until the parent is destroyed; the struct can
            // drop (it has no Drop that destroys the HWND).
        }
        Err(e) => tracing::error!("failed to create video window: {e}"),
    }
}

struct Session {
    stop: Arc<AtomicBool>,
    /// Feed inbound answer/ICE from signaling into the transport thread.
    signal_tx: Sender<SignalData>,
    /// Feed outbound control (perm/bye/input) into the transport thread.
    ctl_tx: Sender<Vec<u8>>,
    /// Host side: whether injecting remote input is currently allowed (the live
    /// `control` permission). Flipping to `false` releases any held keys/buttons.
    control: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// Bundle of the role-specific channels/flags handed to [`transport_driver`].
struct Driver {
    rtc: str0m::Rtc,
    pending: Option<str0m::change::SdpPendingOffer>,
    signal_rx: Receiver<SignalData>,
    ctl_rx: Receiver<Vec<u8>>,
    /// Host: encoded AUs to send on the video channel.
    frame_rx: Option<Receiver<(Vec<u8>, bool)>>,
    /// Viewer: reassembled AUs out to the decode/render thread.
    video_tx: Option<Sender<Vec<u8>>>,
    /// Host: injection gate (Some ⇒ this side injects remote input).
    inject: Option<Arc<AtomicBool>>,
    /// Host: serialized cursor updates to send on the cursor channel.
    cursor_rx: Option<Receiver<Vec<u8>>>,
    /// The bound UDP socket (candidates were gathered from it before the SDP was
    /// generated, so the peer receives them embedded in the offer/answer).
    socket: std::net::UdpSocket,
    stop: Arc<AtomicBool>,
}

/// Real host ICE candidates: one per non-loopback local interface address, all
/// on the bound `port` (the socket listens on `0.0.0.0`, so any interface's
/// `ip:port` reaches it). This replaces advertising the useless wildcard
/// `0.0.0.0:port`, which no peer could route to. Link-local IPv6 (`fe80::`) is
/// skipped (needs a scope id str0m's plain `Candidate::host` can't carry).
fn local_host_candidates(port: u16) -> Vec<std::net::SocketAddr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            let ip = iface.ip();
            if let std::net::IpAddr::V6(v6) = ip {
                if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                    continue; // link-local
                }
            }
            out.push(std::net::SocketAddr::new(ip, port));
        }
    }
    // Offline same-machine testing: fall back to loopback so ICE still forms.
    if out.is_empty() {
        out.push(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    }
    out
}

/// Bind the session UDP socket and register its candidates on `rtc` **before**
/// the SDP offer/answer is generated, so str0m embeds them and the peer learns
/// how to reach us. Gathers host candidates (LAN direct path, §6) AND a
/// server-reflexive (srflx) candidate via STUN (the machine's public `ip:port`),
/// which is what lets two machines on *different* networks traverse typical NATs.
/// Symmetric/strict NATs still need TURN (not yet wired). `stun_urls` come from
/// the config's `iceServers`.
fn bind_and_gather(rtc: &mut str0m::Rtc, stun_urls: &[String]) -> Option<std::net::UdpSocket> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    let port = socket.local_addr().ok()?.port();

    let host = local_host_candidates(port);
    for addr in &host {
        if let Ok(cand) = str0m::Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(cand);
        }
    }

    // STUN discovery for the public address (done while the socket is still
    // blocking, with a short read timeout), then switch to non-blocking for the
    // transport loop.
    if let Some(srflx) = gather_srflx(&socket, stun_urls, &host) {
        tracing::info!("STUN srflx candidate: {srflx}");
        // Base = a local IPv4 host candidate matching the srflx family.
        if let Some(base) = host.iter().find(|a| a.is_ipv4() == srflx.is_ipv4()) {
            match str0m::Candidate::server_reflexive(srflx, *base, "udp") {
                Ok(cand) => {
                    rtc.add_local_candidate(cand);
                }
                Err(e) => tracing::warn!("srflx candidate rejected: {e}"),
            }
        }
    } else {
        tracing::warn!("no STUN srflx candidate — cross-network may fail (need TURN)");
    }

    let _ = socket.set_nonblocking(true);
    Some(socket)
}

/// Query the configured STUN servers for this socket's public `ip:port`.
fn gather_srflx(
    socket: &std::net::UdpSocket,
    stun_urls: &[String],
    _host: &[std::net::SocketAddr],
) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(800)));
    for url in stun_urls {
        // "stun:host:port" (or "stun:host:port?transport=udp").
        let hostport = url
            .strip_prefix("stun:")
            .or_else(|| url.strip_prefix("stuns:"))
            .unwrap_or(url);
        let hostport = hostport.split(['?', '&']).next().unwrap_or(hostport);
        let Ok(addrs) = hostport.to_socket_addrs() else {
            continue;
        };
        for server in addrs {
            if let Some(mapped) = stun_query(socket, server) {
                return Some(mapped);
            }
        }
    }
    None
}

/// Send one STUN Binding Request and parse the mapped address from the reply.
fn stun_query(
    socket: &std::net::UdpSocket,
    server: std::net::SocketAddr,
) -> Option<std::net::SocketAddr> {
    use rand::RngCore;
    // Hand-build a minimal RFC 5389 Binding Request (no attributes): a public
    // STUN server replies with our XOR-MAPPED-ADDRESS.
    let mut tid = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tid);
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
    req[2..4].copy_from_slice(&0u16.to_be_bytes()); // length 0
    req[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes()); // magic cookie
    req[8..20].copy_from_slice(&tid);

    socket.send_to(&req, server).ok()?;
    let mut buf = [0u8; 512];
    // Try a couple of reads (unrelated packets may arrive first). Parse the
    // XOR-MAPPED-ADDRESS by hand — str0m's StunMessage::parse is built for ICE
    // connectivity checks and rejects a bare RFC 5389 Binding Success Response
    // (no MESSAGE-INTEGRITY/FINGERPRINT), which is all a public STUN server sends.
    for _ in 0..3 {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            return None; // read timeout / error — give up on this server
        };
        if from != server {
            continue; // not the STUN reply we're waiting for
        }
        if let Some(mapped) = parse_stun_mapped_address(&buf[..n], &tid) {
            return Some(mapped);
        }
    }
    None
}

/// Parse `XOR-MAPPED-ADDRESS` (preferred) or `MAPPED-ADDRESS` from a STUN Binding
/// Success Response (RFC 5389 §15.1–15.2). Returns the reflexive `ip:port`.
fn parse_stun_mapped_address(buf: &[u8], tid: &[u8; 12]) -> Option<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    const MAGIC: u32 = 0x2112_A442;
    if buf.len() < 20 {
        return None;
    }
    // Binding Success Response = 0x0101.
    if u16::from_be_bytes([buf[0], buf[1]]) != 0x0101 {
        return None;
    }
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (20 + msg_len).min(buf.len());
    let magic = MAGIC.to_be_bytes();

    let mut i = 20;
    let mut plain: Option<SocketAddr> = None;
    while i + 4 <= end {
        let atype = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let alen = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        let vstart = i + 4;
        let vend = vstart + alen;
        if vend > end {
            break;
        }
        // XOR-MAPPED-ADDRESS (0x0020) or MAPPED-ADDRESS (0x0001).
        if (atype == 0x0020 || atype == 0x0001) && alen >= 4 {
            let val = &buf[vstart..vend];
            let xored = atype == 0x0020;
            let family = val[1];
            let port = u16::from_be_bytes([val[2], val[3]]) ^ if xored { 0x2112 } else { 0 };
            let ip = match family {
                0x01 if val.len() >= 8 => {
                    let mut a = [val[4], val[5], val[6], val[7]];
                    if xored {
                        for k in 0..4 {
                            a[k] ^= magic[k];
                        }
                    }
                    Some(IpAddr::V4(Ipv4Addr::new(a[0], a[1], a[2], a[3])))
                }
                0x02 if val.len() >= 20 => {
                    let mut a = [0u8; 16];
                    a.copy_from_slice(&val[4..20]);
                    if xored {
                        for k in 0..4 {
                            a[k] ^= magic[k];
                        }
                        for k in 0..12 {
                            a[4 + k] ^= tid[k];
                        }
                    }
                    Some(IpAddr::V6(Ipv6Addr::from(a)))
                }
                _ => None,
            };
            if let Some(ip) = ip {
                let sa = SocketAddr::new(ip, port);
                if xored {
                    return Some(sa); // XOR form is authoritative
                }
                plain = Some(sa); // keep as fallback, prefer XOR if it appears
            }
        }
        // Advance past the value + 4-byte padding.
        i = vend + ((4 - (alen % 4)) % 4);
    }
    plain
}

const DEFAULT_BITRATE: u32 = 8_000_000;

/// STUN server URLs from the config's `iceServers` (contract §5). Falls back to
/// Google's public STUN if none are usable.
fn stun_urls_from(engine: &Engine) -> Vec<String> {
    let mut urls: Vec<String> = engine
        .config()
        .ice_servers
        .into_iter()
        .map(|s| s.urls)
        .filter(|u| u.starts_with("stun:") || u.starts_with("stuns:"))
        .collect();
    // Always append public fallbacks (dedup) so a single slow/blocked server
    // doesn't lose us the srflx candidate cross-network.
    for fallback in [
        "stun:stun.l.google.com:19302",
        "stun:stun1.l.google.com:19302",
        "stun:stun.cloudflare.com:3478",
    ] {
        if !urls.iter().any(|u| u == fallback) {
            urls.push(fallback.to_string());
        }
    }
    urls
}

/// The negotiated codec for the current/next session (§3), set by the host when
/// it accepts a viewer whose `caps` it has seen. 0=H264, 1=HEVC, 2=AV1.
static NEGOTIATED_CODEC: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn codec_to_u8(c: codec::Codec) -> u8 {
    match c {
        codec::Codec::H264 => 0,
        codec::Codec::Hevc => 1,
        codec::Codec::Av1 => 2,
    }
}

fn negotiated_codec() -> codec::Codec {
    match NEGOTIATED_CODEC.load(Ordering::SeqCst) {
        1 => codec::Codec::Hevc,
        2 => codec::Codec::Av1,
        _ => codec::Codec::H264,
    }
}

/// Host: pick the best codec both ends support from the viewer's advertised
/// decode list intersected with what this host can hardware-encode (§3). Falls
/// back to H.264. Returns the chosen codec's caps string. Call before
/// [`begin_host`].
pub fn set_negotiated_codec_from_caps(viewer_decode: &[String]) -> String {
    let viewer: Vec<codec::Codec> = viewer_decode
        .iter()
        .filter_map(|s| codec::Codec::from_caps_str(s))
        .collect();
    let chosen = codec::Codec::negotiate(&codec::encode::host_encodable(), &viewer);
    NEGOTIATED_CODEC.store(codec_to_u8(chosen), Ordering::SeqCst);
    tracing::info!("negotiated codec: {}", chosen.as_caps_str());
    chosen.as_caps_str().to_string()
}

/// Viewer: the codecs this machine can actually hardware-decode, as caps strings
/// (§3). Advertised in the connect-request so the host never negotiates a codec
/// this viewer cannot decode — the exact failure that black-screens a session
/// (host encodes AV1, viewer has no AV1 decoder, viewer media loop dies).
pub fn viewer_decode_caps() -> Vec<String> {
    codec::decode::viewer_decodable()
        .iter()
        .map(|c| c.as_caps_str().to_string())
        .collect()
}

/// Viewer: record the codec the host said it will stream, so the decoder uses
/// the matching codec (§3). Called before [`begin_viewer`].
pub fn set_codec_from_str(s: &str) {
    let c = codec::Codec::from_caps_str(s).unwrap_or(codec::Codec::H264);
    NEGOTIATED_CODEC.store(codec_to_u8(c), Ordering::SeqCst);
}

/// Host role: build the offer, create channels, send the offer over signaling,
/// and start the capture→encode→transport pipeline.
pub fn begin_host(engine: &Engine, peer: String, permission: Permission) {
    teardown(engine); // ensure clean slate
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Build the offerer Rtc, bind the UDP socket, and register our host
    // candidates BEFORE generating the offer so str0m embeds them in the SDP
    // (the peer learns how to reach us). Then relay the offer through the
    // Cloudflare signaling (opaque `signal.data`, §6).
    let mut rtc = str0m::Rtc::new(std::time::Instant::now());
    let stun_urls = stun_urls_from(engine);
    let socket = match bind_and_gather(&mut rtc, &stun_urls) {
        Some(s) => s,
        None => {
            tracing::error!("host: failed to bind UDP socket");
            return;
        }
    };
    let mut api = rtc.sdp_api();
    api.add_channel("video".to_string());
    api.add_channel("ctl".to_string());
    api.add_channel("cursor".to_string());
    let pending = match api.apply() {
        Some((offer, pending)) => {
            let _ = engine.signaling().send(protocol::SignalMsg::Signal {
                to: Some(peer.clone()),
                from: None,
                data: SignalData::Offer {
                    sdp: offer.to_sdp_string(),
                },
            });
            Some(pending)
        }
        None => None,
    };

    let control = Arc::new(AtomicBool::new(permission == Permission::Control));
    let bitrate = Arc::new(std::sync::atomic::AtomicU32::new(DEFAULT_BITRATE));

    // Transport driver thread (owns the Rtc + UDP socket).
    let driver = Driver {
        rtc,
        pending,
        signal_rx,
        ctl_rx,
        frame_rx: Some(frame_rx),
        video_tx: None, // host does not render video
        inject: Some(control.clone()),
        cursor_rx: Some(cursor_rx),
        socket,
        stop: stop.clone(),
    };
    let t = std::thread::spawn(move || transport_driver(driver));

    // Host capture→encode thread (own D3D11 device shared capture↔encode).
    let stop_m = stop.clone();
    let bitrate_m = bitrate.clone();
    let m = std::thread::spawn(move || {
        if let Err(e) = host_media_loop(frame_tx, cursor_tx, bitrate_m, stop_m) {
            tracing::warn!("host media loop ended: {e}");
        }
    });

    // Send the initial permission once the ctl channel is up (§4.2).
    let _ = ctl_tx.send(serialize(&ControlMsg::Perm { value: permission }));

    *SESSION.lock() = Some(Session {
        stop,
        signal_tx,
        ctl_tx,
        control,
        threads: vec![t, m],
    });
    let _ = peer;
}

/// Viewer role: accept the host's offer when it arrives (see [`on_signal`]) and
/// start the transport→decode→render pipeline. Called after `connect-response`
/// accepted; the actual offer is handled in [`on_signal`].
pub fn begin_viewer(_engine: &Engine, peer: String, _permission: Permission) {
    tracing::info!("viewer session with {peer}; awaiting offer");
    // The viewer's transport/media threads are started on the first offer in
    // `on_signal` (it needs the offer to build the answerer Rtc).
}

/// Route an inbound WebRTC payload (offer/answer/ICE) to the transport thread,
/// or bootstrap the viewer's answerer Rtc on the first offer.
pub fn on_signal(engine: &Engine, peer: &str, data: SignalData) {
    // If we already have a session, forward answer/ICE to its transport thread.
    if let Some(s) = SESSION.lock().as_ref() {
        let _ = s.signal_tx.send(data);
        return;
    }
    // No session yet + an offer ⇒ we are the viewer; build the answerer.
    if let SignalData::Offer { sdp } = data {
        start_viewer_transport(engine, peer.to_string(), sdp);
    }
}

fn start_viewer_transport(engine: &Engine, peer: String, offer_sdp: String) {
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (video_tx, video_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let mut rtc = str0m::Rtc::new(std::time::Instant::now());
    // Bind + gather host/srflx candidates before accepting the offer, so the
    // answer str0m generates carries them back to the host (§6 + NAT traversal).
    let stun_urls = stun_urls_from(engine);
    let socket = match bind_and_gather(&mut rtc, &stun_urls) {
        Some(s) => s,
        None => {
            tracing::error!("viewer: failed to bind UDP socket");
            return;
        }
    };
    if let Ok(offer) = str0m::change::SdpOffer::from_sdp_string(&offer_sdp) {
        match rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => {
                let _ = engine.signaling().send(protocol::SignalMsg::Signal {
                    to: Some(peer.clone()),
                    from: None,
                    data: SignalData::Answer {
                        sdp: answer.to_sdp_string(),
                    },
                });
            }
            Err(e) => tracing::warn!("accept_offer failed: {e}"),
        }
    }

    // Viewer input capture (§7): the video window's wndproc pushes VideoInput to
    // us; we translate to protocol InputMsg and relay on the ctl channel. Gated
    // by the sink being installed, so we only capture during control sessions —
    // and the host also enforces its own permission (defence in depth).
    let (input_tx, input_rx) = std::sync::mpsc::channel::<render::window::VideoInput>();
    render::window::set_input_sink(input_tx);
    let ctl_for_input = ctl_tx.clone();
    let stop_i = stop.clone();
    let i = std::thread::spawn(move || {
        while !stop_i.load(Ordering::SeqCst) {
            match input_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(ev) => {
                    if let Some(msg) = translate_input(ev) {
                        let _ = ctl_for_input.send(serde_json::to_vec(&msg).unwrap_or_default());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
    });

    // Transport thread (viewer: routes reassembled video to the render thread).
    let driver = Driver {
        rtc,
        pending: None,
        signal_rx,
        ctl_rx,
        frame_rx: None,
        video_tx: Some(video_tx),
        inject: None, // viewer never injects
        cursor_rx: None,
        socket,
        stop: stop.clone(),
    };
    let t = std::thread::spawn(move || transport_driver(driver));

    // Viewer decode→render thread.
    let stop_r = stop.clone();
    let r = std::thread::spawn(move || {
        if let Err(e) = viewer_media_loop(video_rx, stop_r) {
            tracing::warn!("viewer media loop ended: {e}");
        }
    });

    let mut threads = vec![t, r, i];

    // Optional shortcut capture (§8a, experimental, off by default): grab
    // Alt+Tab / Win locally via WH_KEYBOARD_LL and forward them to the host.
    if engine.config().capture_shortcuts {
        let ctl = ctl_tx.clone();
        let stop_k = stop.clone();
        let k = std::thread::spawn(move || {
            let installed = input::keyhook::install(Box::new(move |code: &str, down: bool| {
                let msg = if down {
                    protocol::InputMsg::KeyDown {
                        code: code.to_string(),
                    }
                } else {
                    protocol::InputMsg::KeyUp {
                        code: code.to_string(),
                    }
                };
                let _ = ctl.send(serde_json::to_vec(&msg).unwrap_or_default());
            }));
            if installed {
                input::keyhook::message_pump(&stop_k);
                input::keyhook::uninstall();
            }
        });
        threads.push(k);
    }

    *SESSION.lock() = Some(Session {
        stop,
        signal_tx,
        ctl_tx,
        control: Arc::new(AtomicBool::new(false)),
        threads,
    });
}

/// Translate a captured [`render::window::VideoInput`] into a protocol input
/// message (§7/§8a — scancode → DOM `KeyboardEvent.code`).
fn translate_input(ev: render::window::VideoInput) -> Option<protocol::InputMsg> {
    use protocol::{Button, InputMsg};
    Some(match ev {
        render::window::VideoInput::Move { nx, ny } => InputMsg::Move { x: nx, y: ny },
        render::window::VideoInput::Button {
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
        render::window::VideoInput::Wheel { dx, dy } => InputMsg::Wheel { dx, dy },
        render::window::VideoInput::Key {
            scancode,
            extended,
            down,
        } => {
            let code = input::scancode::code_for(scancode, extended)?.to_string();
            if down {
                InputMsg::KeyDown { code }
            } else {
                InputMsg::KeyUp { code }
            }
        }
    })
}

/// Send a host→viewer control message (perm change / bye) on the ctl channel.
pub fn send_ctl(_engine: &Engine, msg: &ControlMsg) {
    if let Some(s) = SESSION.lock().as_ref() {
        let _ = s.ctl_tx.send(serialize(msg));
    }
}

/// Host: flip the injection gate when the live permission changes (§4.2). When
/// control is revoked the transport thread releases any held input.
pub fn set_control(allow: bool) {
    if let Some(s) = SESSION.lock().as_ref() {
        s.control.store(allow, Ordering::SeqCst);
    }
}

/// Tear the session down: signal all threads to stop and join them.
pub fn teardown(_engine: &Engine) {
    // Stop capturing viewer input before threads join (idempotent).
    render::window::clear_input_sink();
    let session = SESSION.lock().take();
    if let Some(session) = session {
        session.stop.store(true, Ordering::SeqCst);
        for t in session.threads {
            let _ = t.join();
        }
    }
    // Hide the native video surface so the home screen is visible again (§7).
    let hwnd_raw = RENDER_HWND.load(Ordering::SeqCst);
    if hwnd_raw != 0 {
        render::window::hide(hwnd_raw);
    }
}

fn serialize(msg: &ControlMsg) -> Vec<u8> {
    serde_json::to_vec(msg).unwrap_or_default()
}

// ---- Transport driver (owns the str0m Rtc + UDP socket) ---------------------

fn transport_driver(d: Driver) {
    let Driver {
        rtc,
        mut pending,
        signal_rx,
        ctl_rx,
        frame_rx,
        video_tx,
        inject,
        cursor_rx,
        socket,
        stop,
    } = d;
    use transport::{Inbound, Transport};

    // Host side: injector for remote input, gated by the live control permission.
    let mut injector = inject.as_ref().map(|_| input::Injector::new());
    let mut was_control = true;
    // Host: follow the input desktop so injection reaches the secure desktop /
    // UAC prompt when running elevated (§8b). Re-attaches this thread on switch.
    let mut desktop = inject
        .as_ref()
        .map(|_| elevation::InputDesktopFollower::new());

    // The socket was bound and its host candidates registered before the SDP was
    // generated (see `bind_and_gather`), so the peer already has our candidates.
    let mut tp = Transport::new(rtc);
    // Host creates the three channels here (direct write side).
    if frame_rx.is_some() {
        tp.create_channels();
    }

    let mut buf = [0u8; 2048];
    let mut video_count: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        // 0) Host: if control was just revoked (view-only), release any input we
        // are holding down so the host is never left with a stuck key/button.
        if let (Some(inj), Some(gate)) = (injector.as_mut(), inject.as_ref()) {
            let now = gate.load(Ordering::SeqCst);
            if was_control && !now {
                inj.release_all();
            }
            was_control = now;
        }

        // 1) Accept inbound answer/ICE relayed from signaling.
        while let Ok(data) = signal_rx.try_recv() {
            match data {
                SignalData::Answer { sdp } => {
                    if let (Some(p), Ok(ans)) = (
                        pending.take(),
                        str0m::change::SdpAnswer::from_sdp_string(&sdp),
                    ) {
                        let _ = tp.rtc_mut().sdp_api().accept_answer(p, ans);
                    }
                }
                SignalData::Ice { candidate } => {
                    if let Ok(cand) = str0m::Candidate::from_sdp_string(&candidate.candidate) {
                        tp.rtc_mut().add_remote_candidate(cand);
                    }
                }
                SignalData::Offer { .. } => {} // handled at session start
            }
        }

        // 2) Outbound control + encoded video from the media threads.
        while let Ok(bytes) = ctl_rx.try_recv() {
            let _ = tp.send_ctl(&bytes);
        }
        if let Some(frame_rx) = &frame_rx {
            while let Ok((au, keyframe)) = frame_rx.try_recv() {
                let _ = tp.send_video(&au, keyframe);
            }
        }
        // Host: cursor position updates on the cursor channel (§5a/§7).
        if let Some(cursor_rx) = &cursor_rx {
            while let Ok(bytes) = cursor_rx.try_recv() {
                let _ = tp.send_cursor(&bytes);
            }
        }

        // 3) Drive str0m: emit transmits, handle timeouts, surface events.
        match tp.poll_output() {
            Ok(str0m::Output::Transmit(t)) => {
                let _ = socket.send_to(&t.contents, t.destination);
            }
            Ok(str0m::Output::Timeout(_)) => {
                // 4) Pull any pending UDP and feed it in; else advance time.
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if let Ok(local) = socket.local_addr() {
                            let recv = str0m::net::Receive::new(
                                str0m::net::Protocol::Udp,
                                from,
                                local,
                                &buf[..n],
                            );
                            if let Ok(recv) = recv {
                                let _ = tp.handle_input(str0m::Input::Receive(
                                    std::time::Instant::now(),
                                    recv,
                                ));
                            }
                        }
                    }
                    Err(_) => {
                        let _ = tp.handle_input(str0m::Input::Timeout(std::time::Instant::now()));
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
            Ok(str0m::Output::Event(ev)) => {
                if let Some(inbound) = tp.on_event(ev) {
                    match inbound {
                        Inbound::Connected => {
                            tracing::info!(
                                "transport connected ({})",
                                if inject.is_some() { "host" } else { "viewer" }
                            );
                        }
                        Inbound::ChannelOpen(_, label) => {
                            tracing::info!("data channel open: {label}");
                        }
                        Inbound::Video(frame) => {
                            video_count += 1;
                            if video_count == 1 || video_count % 120 == 0 {
                                tracing::info!(
                                    "viewer: received video frame #{video_count} ({} bytes)",
                                    frame.data.len()
                                );
                            }
                            if let Some(tx) = &video_tx {
                                let _ = tx.send(frame.data);
                            }
                        }
                        Inbound::Ctl(bytes) => {
                            // Host: inbound ctl is remote input — inject it,
                            // gated on the current control permission (§4.1).
                            if let (Some(inj), Some(gate)) = (injector.as_mut(), inject.as_ref()) {
                                if gate.load(Ordering::SeqCst) {
                                    // Re-attach to the current input desktop first
                                    // so injection lands on it (§8b).
                                    if let Some(d) = desktop.as_mut() {
                                        let _ = d.follow();
                                    }
                                    if let Ok(msg) =
                                        serde_json::from_slice::<protocol::InputMsg>(&bytes)
                                    {
                                        inj.dispatch(&msg);
                                    }
                                }
                            }
                            // Viewer: inbound ctl is a ControlMsg (perm/bye). A
                            // clean `bye` ends the session.
                            if inject.is_none() {
                                if let Ok(protocol::ControlMsg::Bye) =
                                    serde_json::from_slice::<protocol::ControlMsg>(&bytes)
                                {
                                    break;
                                }
                            }
                        }
                        Inbound::Cursor(bytes) => {
                            // Viewer: update the sprite position (§7).
                            if let Ok(protocol::ControlMsg::Cursor { x, y, visible, .. }) =
                                serde_json::from_slice::<protocol::ControlMsg>(&bytes)
                            {
                                *CURSOR.lock() = Some((x, y, visible));
                            }
                        }
                        Inbound::Disconnected => {
                            tracing::info!("transport disconnected");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("transport poll error: {e}");
                break;
            }
        }
    }
}

// ---- Host capture → encode --------------------------------------------------

fn host_media_loop(
    frame_tx: Sender<(Vec<u8>, bool)>,
    cursor_tx: Sender<Vec<u8>>,
    bitrate: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use codec::{Encoder, EncoderConfig};

    let mut dup = capture::Duplicator::new(0, 0)?;
    let codec = negotiated_codec();
    let mut cfg = EncoderConfig {
        codec,
        bitrate_bps: bitrate.load(Ordering::SeqCst),
        ..Default::default()
    };
    // Encoder shares the capture device (§5c zero-copy).
    let mut encoder = Encoder::new(dup.device(), cfg)?;
    tracing::info!(
        "host: capture+encoder ready ({}x{}, {})",
        cfg.width,
        cfg.height,
        codec.as_caps_str()
    );
    // The first emitted frame must be an IDR so the viewer can start decoding.
    encoder.force_keyframe();
    let mut applied_bitrate = cfg.bitrate_bps;
    let mut sent: u64 = 0;

    while !stop.load(Ordering::SeqCst) {
        // §6 adaptive bitrate: feed the current BWE target to the encoder.
        let target = bitrate.load(Ordering::SeqCst);
        if target != applied_bitrate {
            let _ = encoder.set_bitrate(target);
            applied_bitrate = target;
            cfg.bitrate_bps = target;
        }

        match dup.acquire(std::time::Duration::from_millis(16)) {
            Ok(Some(frame)) => {
                // Cursor moves travel out-of-band on the cursor channel and never
                // wake the video encoder (§5a) — the viewer draws the sprite.
                if let Some(cur) = &frame.cursor {
                    let x = (cur.position.x as f64 / cfg.width as f64).clamp(0.0, 1.0);
                    let y = (cur.position.y as f64 / cfg.height as f64).clamp(0.0, 1.0);
                    let msg = protocol::ControlMsg::Cursor {
                        x,
                        y,
                        shape: None,
                        visible: cur.visible,
                    };
                    let _ = cursor_tx.send(serde_json::to_vec(&msg).unwrap_or_default());
                }

                // §5a adaptive frame rate: a pointer-only update or a frame with
                // no changed region costs ~0 — send nothing, don't wake the
                // encoder into an idle stream.
                let has_change = !frame.pointer_only
                    && (!frame.dirty_rects.is_empty() || !frame.move_rects.is_empty());
                if !has_change {
                    dup.release();
                    continue;
                }
                // BGRA→NV12 + encode happen inside the encoder path (§5b).
                match encoder.encode(&frame.texture) {
                    Ok(units) => {
                        for u in units {
                            sent += 1;
                            if sent == 1 || sent % 120 == 0 {
                                tracing::info!(
                                    "host: encoded+sent AU #{sent} ({} bytes, keyframe={})",
                                    u.data.len(),
                                    u.keyframe
                                );
                            }
                            let _ = frame_tx.send((u.data, u.keyframe));
                        }
                    }
                    Err(e) => tracing::warn!("encode error: {e}"),
                }
                dup.release();
            }
            Ok(None) => { /* §5a idle: WAIT_TIMEOUT, send nothing */ }
            Err(capture::Error::AccessLost) => {
                let _ = dup.reinit();
                encoder.force_keyframe(); // new surface — resync the viewer
            }
            Err(e) => {
                tracing::warn!("capture: {e}");
                break;
            }
        }
    }
    Ok(())
}

// ---- Viewer decode → render -------------------------------------------------

fn viewer_media_loop(
    video_rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use codec::Decoder;

    let hwnd_raw = RENDER_HWND.load(Ordering::SeqCst);
    if hwnd_raw == 0 {
        return Err("no render target set".into());
    }
    let hwnd = windows::Win32::Foundation::HWND(hwnd_raw as *mut _);

    // Reveal the native video surface over the web chrome (§7).
    render::window::show(hwnd_raw);

    // Create a device + decoder + renderer that share it (§5c/§7). The decoder
    // uses the codec the host negotiated (§3).
    let renderer_dev = create_render_device()?;
    let mut decoder = Decoder::new(&renderer_dev, negotiated_codec(), 1920, 1080)?;
    let mut renderer = render::Renderer::new(&renderer_dev, hwnd, 1920, 1080)?;
    tracing::info!(
        "viewer: decoder+renderer ready ({})",
        negotiated_codec().as_caps_str()
    );

    let mut ts = 0i64;
    let mut rendered: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        match video_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(au) => {
                ts += 1;
                match decoder.decode(&au, ts) {
                    Ok(Some(frame)) => {
                        rendered += 1;
                        if rendered == 1 || rendered % 120 == 0 {
                            tracing::info!("viewer: rendered frame #{rendered}");
                        }
                        // Draw the out-of-band cursor sprite on top (§7).
                        let cursor = match *CURSOR.lock() {
                            Some((x, y, true)) => Some((x, y)),
                            _ => None,
                        };
                        let _ = renderer.render_frame(&frame.texture, frame.array_index, cursor);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("decode error: {e}"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Create a standalone D3D11 device for the viewer decode+render side.
fn create_render_device(
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Device, Box<dyn std::error::Error>> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_SDK_VERSION,
    };
    let mut device = None;
    // SAFETY: standard device creation for video decode + render.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.ok_or_else(|| "device creation returned null".into())
}

#[cfg(test)]
mod tests {
    use super::parse_stun_mapped_address;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn parses_xor_mapped_address_ipv4() {
        let tid = [7u8; 12];
        let ip = Ipv4Addr::new(203, 0, 113, 5);
        let port: u16 = 12345;
        let magic: u32 = 0x2112_A442;
        let mb = magic.to_be_bytes();

        let mut msg = Vec::new();
        msg.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success Response
        msg.extend_from_slice(&12u16.to_be_bytes()); // attr header(4) + value(8)
        msg.extend_from_slice(&magic.to_be_bytes());
        msg.extend_from_slice(&tid);
        msg.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0x00);
        msg.push(0x01); // IPv4 family
        msg.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
        let o = ip.octets();
        msg.extend_from_slice(&[o[0] ^ mb[0], o[1] ^ mb[1], o[2] ^ mb[2], o[3] ^ mb[3]]);

        let got = parse_stun_mapped_address(&msg, &tid).unwrap();
        assert_eq!(got, SocketAddr::new(IpAddr::V4(ip), port));
    }

    #[test]
    fn rejects_non_success_response() {
        // A request (0x0001), not a success response — must not yield an address.
        let mut msg = vec![0u8; 20];
        msg[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        assert!(parse_stun_mapped_address(&msg, &[0u8; 12]).is_none());
    }
}
