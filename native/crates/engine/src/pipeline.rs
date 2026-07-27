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

/// The user's clipboard-sync setting, mirrored here so the permission flip can
/// re-apply it without reaching for the config lock from the session path.
static CLIPBOARD_SETTING: AtomicBool = AtomicBool::new(true);

/// Whether a received file is pasted into the window it was dropped on. Off
/// leaves it on the clipboard for the user to paste wherever they choose.
static PASTE_DROPPED: AtomicBool = AtomicBool::new(true);

/// Host: the viewer's reported display size (`width<<32 | height`, 0 = unknown).
/// The capture/encode loop scales to this so the viewer presents 1:1 instead of
/// resampling every frame. One session at a time, so a global is fine.
static VIEW_SIZE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// Whether clipboard sync is live. Follows the control permission (and the
    /// user setting), so revoking control also stops clipboard exchange.
    clip_on: Arc<AtomicBool>,
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
    /// Viewer: reassembled AUs (+ keyframe flag) out to the decode/render thread.
    video_tx: Option<Sender<(Vec<u8>, bool)>>,
    /// Host: injection gate (Some ⇒ this side injects remote input).
    inject: Option<Arc<AtomicBool>>,
    /// Host: serialized cursor updates to send on the cursor channel.
    cursor_rx: Option<Receiver<Vec<u8>>>,
    /// Viewer: encoded [`protocol::BulkFrame`]s for an outgoing file transfer.
    /// Drained with backpressure so a large file can't monopolise the link.
    bulk_rx: Option<Receiver<Vec<u8>>>,
    /// Clipboard text received from the peer, handed to the clipboard thread
    /// (which owns the OS clipboard and the echo guard).
    clip_tx: Sender<String>,
    /// Host: the §6 data-channel ids created on the Rtc in `begin_host`.
    channels: Option<transport::Channels>,
    /// Host: the video RTP media track's mid (from `add_media`). Viewer learns
    /// its own from `Event::MediaAdded`, so this is `None` there.
    video_mid: Option<str0m::media::Mid>,
    /// Host: set to make the encoder emit an IDR (on video-channel open, and on a
    /// viewer `KeyframeRequest`). Frames sent before the channel opened are lost
    /// on the wire, so the first *deliverable* frame must restart the decoder.
    force_key: Option<Arc<AtomicBool>>,
    /// Host: the encoder's CBR target — BWE clamped to [MIN_BITRATE, TARGET].
    /// Floored on purpose: below MIN a 1080p desktop can't stay legible.
    bitrate: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Host: the RAW measured link capacity from BWE, unfloored. The source pacer
    /// drains against THIS, not `bitrate` — if the link genuinely carries less
    /// than the quality floor we must send fewer FRAMES, not pretend the capacity
    /// exists and sit in permanent congestion.
    link: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Notify the UI + reset the role when the transport dies unexpectedly (the
    /// driver thread has no `&Engine`). This is the fix for "you get disconnected
    /// but the controller thinks it's still alive with no notification."
    ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    role: Arc<Mutex<crate::Role>>,
    /// The bound UDP socket (candidates were gathered from it before the SDP was
    /// generated, so the peer receives them embedded in the offer/answer).
    socket: std::net::UdpSocket,
    /// TURN allocation when one was obtained: transmits sourced from the relayed
    /// address are wrapped for the relay; Data Indications are unwrapped.
    turn: Option<crate::turn::TurnAllocation>,
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
/// how to reach us. Gathers three candidate tiers (§6):
///   1. host (LAN direct path),
///   2. server-reflexive via STUN (public `ip:port` — friendly-NAT traversal),
///   3. relayed via TURN (works behind symmetric/carrier-grade NATs where
///      hole-punching fails; lowest ICE priority, so it's only used when the
///      direct paths lose).
fn bind_and_gather(
    rtc: &mut str0m::Rtc,
    ice: &IceConfig,
) -> Option<(std::net::UdpSocket, Option<crate::turn::TurnAllocation>)> {
    // Bind to the *primary local IP*, not 0.0.0.0. str0m correlates a received
    // packet to a local ICE candidate by the destination address we report on
    // `Input::Receive` — which is `socket.local_addr()`. A wildcard bind makes
    // that `0.0.0.0:port`, matching none of our real-IP host candidates, so
    // connectivity checks never validate and ICE hangs at "Checking" (even on the
    // same LAN). Binding to the real IP makes `local_addr()` a routable address.
    let socket = bind_primary_socket()?;
    let bound = socket.local_addr().ok()?;
    let port = bound.port();
    tracing::info!("bound UDP socket at {bound} (local ICE base)");

    // Host candidate = the actual bound address (real IP). Fall back to
    // enumerating interfaces only if we somehow ended up on the wildcard.
    let host: Vec<std::net::SocketAddr> = if bound.ip().is_unspecified() {
        local_host_candidates(port)
    } else {
        vec![bound]
    };
    for addr in &host {
        if let Ok(cand) = str0m::Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(cand);
        }
    }

    // STUN discovery for the public address (done while the socket is still
    // blocking, with a short read timeout), then switch to non-blocking for the
    // transport loop.
    if let Some(srflx) = gather_srflx(&socket, &ice.stun_urls, &host) {
        tracing::info!("STUN srflx candidate: {srflx}");
        // Base = a local host candidate matching the srflx family.
        if let Some(base) = host.iter().find(|a| a.is_ipv4() == srflx.is_ipv4()) {
            match str0m::Candidate::server_reflexive(srflx, *base, "udp") {
                Ok(cand) => {
                    rtc.add_local_candidate(cand);
                }
                Err(e) => tracing::warn!("srflx candidate rejected: {e}"),
            }
        }
    } else {
        tracing::warn!("no STUN srflx candidate — relying on TURN for cross-network");
    }

    // TURN allocation: the guaranteed cross-network path. First server that
    // allocates wins; failure just means we fall back to direct-only.
    let mut turn_alloc = None;
    for t in &ice.turn_servers {
        // Resolve to an address of the SAME family as our bound socket. Cloudflare
        // TURN has both A and AAAA records; picking an IPv6 address for an
        // IPv4-bound socket makes every send fail and the allocation time out
        // (exactly what stranded the host — its DNS returned the IPv6 address).
        let candidates = resolve_all(&t.hostport, bound.is_ipv4());
        if candidates.is_empty() {
            tracing::warn!(
                "TURN server {} did not resolve to a usable address",
                t.hostport
            );
            continue;
        }
        let mut alloc_opt = None;
        for server in candidates {
            if let Some(alloc) =
                crate::turn::TurnAllocation::allocate(&socket, server, &t.username, &t.credential)
            {
                alloc_opt = Some(alloc);
                break;
            }
            tracing::warn!("TURN allocation failed on {server}");
        }
        if let Some(alloc) = alloc_opt {
            // Local base = our bound socket addr (str0m sets the relayed
            // candidate's transmit `source` to the relayed address, which is
            // how the driver routes it through the TURN server).
            match str0m::Candidate::relayed(alloc.relayed, bound, "udp") {
                Ok(cand) => {
                    rtc.add_local_candidate(cand);
                    turn_alloc = Some(alloc);
                }
                Err(e) => tracing::warn!("relayed candidate rejected: {e}"),
            }
        }
        if turn_alloc.is_some() {
            break;
        }
    }
    if turn_alloc.is_none() && !ice.turn_servers.is_empty() {
        tracing::warn!("no TURN allocation — strict-NAT cross-network may fail");
    }

    let _ = socket.set_nonblocking(true);
    Some((socket, turn_alloc))
}

/// Resolve `host:port` to socket addresses matching the bound socket's family
/// (`want_ipv4`), so a UDP send can actually reach them. Same-family addresses
/// first; if none match, falls back to whatever resolved (best-effort).
fn resolve_all(hostport: &str, want_ipv4: bool) -> Vec<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let Ok(addrs) = hostport.to_socket_addrs() else {
        return Vec::new();
    };
    let all: Vec<std::net::SocketAddr> = addrs.collect();
    let matching: Vec<std::net::SocketAddr> = all
        .iter()
        .copied()
        .filter(|a| a.is_ipv4() == want_ipv4)
        .collect();
    if matching.is_empty() {
        all
    } else {
        matching
    }
}

/// Bind a UDP socket to the primary local IP (the source address the OS would use
/// to reach the internet), so `local_addr()` is a routable IP that matches our
/// advertised host candidate. Falls back to a wildcard bind if that can't be
/// determined (rare; multi-homed correlation may then suffer).
fn bind_primary_socket() -> Option<std::net::UdpSocket> {
    if let Some(ip) = primary_local_ip() {
        if let Ok(s) = std::net::UdpSocket::bind(std::net::SocketAddr::new(ip, 0)) {
            return Some(s);
        }
    }
    std::net::UdpSocket::bind("0.0.0.0:0").ok()
}

/// The primary outbound local IPv4: connect a throwaway UDP socket to a public
/// address (this sends **no** packets — it only makes the OS pick the source IP
/// of the default route) and read back its local address.
fn primary_local_ip() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    if ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
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

/// Bitrate policy (§6). H.264 needs real headroom to keep TEXT sharp — the
/// Electron build this rewrite replaced ran at 8 Mbps and looked right; the 3
/// Mbps "safe baseline" tried here was a large part of why the native build
/// looked like mush. The floor is deliberately high for the same reason: below
/// ~2.5 Mbps a 1080p desktop cannot stay legible, so when the link can't carry
/// that we spend FRAMES instead of sharpness (see the source pacer).
const TARGET_BITRATE: u32 = 8_000_000;
const MIN_BITRATE: u32 = 2_500_000;
/// What we ask the BWE to probe toward — just above TARGET so it can confirm
/// the link sustains it, without flooding a cellular uplink with padding.
const DESIRED_BITRATE: u32 = 9_000_000;

/// Host encode frame-rate cap (30fps). NOTE: `EncoderConfig::fps_num` MUST match
/// this — CBR budgets bits per DECLARED frame, so a mismatch silently halves
/// both per-frame quality and the bitrate actually used.
const MAX_FPS: u32 = 30;
const MAX_FPS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1000 / MAX_FPS as u64);
/// Never pace the source below this — under a truly bad link we want a slow but
/// still-updating picture, not a slideshow that looks frozen.
const MIN_FPS: u32 = 4;

/// One usable TURN server: `host:port` + long-term credentials.
struct TurnServer {
    hostport: String,
    username: String,
    credential: String,
}

/// ICE servers resolved from config (contract §5 `iceServers`) + public
/// fallbacks.
struct IceConfig {
    stun_urls: Vec<String>,
    turn_servers: Vec<TurnServer>,
}

/// Parse ICE servers from (a) the config's `iceServers` (contract §5) and (b) the
/// TURN relay credentials the signaling server minted (Cloudflare TURN, cached on
/// the engine). STUN gets public fallbacks appended; TURN entries carry
/// `username`/`credential`. No dead free-relay default — cross-network relay comes
/// from the account's own Cloudflare TURN key via the server.
fn ice_config_from(engine: &Engine) -> IceConfig {
    let mut servers = engine.config().ice_servers;
    // Relay credentials fetched over signaling (turn:/turns: with creds).
    servers.extend(engine.turn_servers());
    let mut stun_urls: Vec<String> = Vec::new();
    let mut turn_servers: Vec<TurnServer> = Vec::new();

    for s in servers {
        let url = s.urls.clone();
        if url.starts_with("stun:") || url.starts_with("stuns:") {
            stun_urls.push(url);
        } else if let Some(rest) = url.strip_prefix("turn:") {
            // Only UDP transport is supported; "?transport=tcp" entries skip.
            if url.contains("transport=tcp") {
                continue;
            }
            let hostport = rest.split(['?', '&']).next().unwrap_or(rest).to_string();
            let hostport = if hostport.contains(':') {
                hostport
            } else {
                format!("{hostport}:3478")
            };
            turn_servers.push(TurnServer {
                hostport,
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            });
        }
    }

    // STUN fallbacks (dedup) so one slow/blocked server doesn't cost the srflx.
    for fallback in [
        "stun:stun.l.google.com:19302",
        "stun:stun1.l.google.com:19302",
        "stun:stun.cloudflare.com:3478",
    ] {
        if !stun_urls.iter().any(|u| u == fallback) {
            stun_urls.push(fallback.to_string());
        }
    }

    if turn_servers.is_empty() {
        tracing::info!(
            "no TURN relay configured — direct paths only (set up a TURN key for cross-network)"
        );
    } else {
        tracing::info!("{} TURN relay endpoint(s) available", turn_servers.len());
    }

    IceConfig {
        stun_urls,
        turn_servers,
    }
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
                      // Start at the native screen size; the viewer reports its own once it renders.
    VIEW_SIZE.store(0, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Build the offerer Rtc, bind the UDP socket, and register our host
    // candidates BEFORE generating the offer so str0m embeds them in the SDP
    // (the peer learns how to reach us). Then relay the offer through the
    // Cloudflare signaling (opaque `signal.data`, §6).
    //
    // Enable BWE (Google Congestion Control): str0m measures the link and emits
    // `EgressBitrateEstimate`, which we feed straight to the encoder's CBR target
    // (§6) — the real congestion control that made the browser build smooth.
    // Offer exactly the one negotiated video codec so the media track's payload
    // type is unambiguous when we resolve it to write frames.
    let mut rtc = {
        let builder = str0m::RtcConfig::new()
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(TARGET_BITRATE as u64)))
            .clear_codecs();
        let builder = match negotiated_codec() {
            codec::Codec::Hevc => builder.enable_h265(true),
            codec::Codec::Av1 => builder.enable_av1(true),
            codec::Codec::H264 => builder.enable_h264(true),
        };
        builder.build(std::time::Instant::now())
    };
    let ice = ice_config_from(engine);
    let (socket, turn_alloc) = match bind_and_gather(&mut rtc, &ice) {
        Some(s) => s,
        None => {
            tracing::error!("host: failed to bind UDP socket");
            return;
        }
    };
    // Create the two §6 data channels (ctl reliable, cursor unreliable-latest)
    // exactly once via the direct API. Video is NOT a data channel — it is added
    // as an RTP media track below.
    let channels = {
        let [c, cur, blk] = transport::channel_configs();
        let mut dapi = rtc.direct_api();
        let ctl = dapi.create_data_channel(c);
        let cursor = dapi.create_data_channel(cur);
        let bulk = dapi.create_data_channel(blk);
        transport::Channels { ctl, cursor, bulk }
    };

    let mut api = rtc.sdp_api();
    // The video RTP media track (host → viewer). str0m negotiates it in the offer;
    // the mid is writable once the answer lands (see `Transport::send_video`).
    let video_mid = api.add_media(
        str0m::media::MediaKind::Video,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    // One throwaway SDP-negotiated channel forces the m=application section into
    // the offer (the SCTP association the direct channels ride on). Its label is
    // never used for media.
    api.add_channel("init".to_string());
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
    let bitrate = Arc::new(std::sync::atomic::AtomicU32::new(TARGET_BITRATE));
    let link = Arc::new(std::sync::atomic::AtomicU32::new(TARGET_BITRATE));
    let force_key = Arc::new(AtomicBool::new(false));

    // Clipboard sync mirrors text both ways. Gated on the SAME live permission
    // as input: a view-only peer must not be able to read this machine's
    // clipboard (which can hold a password that was never on screen) or write to
    // it. Flipping to view-only mid-session stops it immediately.
    CLIPBOARD_SETTING.store(engine.config().clipboard_sync, Ordering::SeqCst);
    PASTE_DROPPED.store(engine.config().paste_dropped_files, Ordering::SeqCst);
    let clip_on = Arc::new(AtomicBool::new(
        engine.config().clipboard_sync && permission == Permission::Control,
    ));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    let clip_ctl = ctl_tx.clone();
    let clip_stop = stop.clone();
    let clip_gate = clip_on.clone();
    let cb = std::thread::spawn(move || {
        clipboard_loop(clip_ctl, clip_rx, clip_gate, clip_stop);
    });

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
        bulk_rx: None, // host does not push files
        clip_tx: clip_tx.clone(),

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
    };
    let t = std::thread::spawn(move || transport_driver(driver));

    // Host capture→encode thread (own D3D11 device shared capture↔encode).
    let stop_m = stop.clone();
    let bitrate_m = bitrate.clone();
    let link_m = link.clone();
    let m = std::thread::spawn(move || {
        if let Err(e) = host_media_loop(frame_tx, cursor_tx, bitrate_m, link_m, force_key, stop_m) {
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
        clip_on,
        threads: vec![t, m, cb],
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
    let (video_tx, video_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();

    let mut rtc = str0m::Rtc::new(std::time::Instant::now());
    // Bind + gather host/srflx/relay candidates before accepting the offer, so
    // the answer str0m generates carries them back to the host (§6 + NAT
    // traversal).
    let ice = ice_config_from(engine);
    let (socket, turn_alloc) = match bind_and_gather(&mut rtc, &ice) {
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

    // Viewer file push: dropping files on the video window queues them here, and
    // this thread streams each one as bulk frames. Reading/chunking on its own
    // thread keeps a slow disk off the transport loop.
    let (bulk_tx, bulk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (drop_tx, drop_rx) = std::sync::mpsc::channel::<render::window::FileDrop>();
    render::window::set_file_drop_sink(drop_tx);
    let stop_f = stop.clone();
    let ui_f = engine.ui_sender();
    let f = std::thread::spawn(move || {
        file_send_loop(drop_rx, bulk_tx, ui_f, stop_f);
    });

    // Viewer input capture (§7): the video window's wndproc pushes VideoInput to
    // us; we translate to protocol InputMsg and relay on the ctl channel. Gated
    // by the sink being installed, so we only capture during control sessions —
    // and the host also enforces its own permission (defence in depth).
    let (input_tx, input_rx) = std::sync::mpsc::channel::<render::window::VideoInput>();
    render::window::set_input_sink(input_tx);

    // Clipboard sync (viewer side). The host applies its own permission gate; on
    // this side the user's setting is what governs.
    CLIPBOARD_SETTING.store(engine.config().clipboard_sync, Ordering::SeqCst);
    let clip_on = Arc::new(AtomicBool::new(engine.config().clipboard_sync));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    let clip_ctl = ctl_tx.clone();
    let clip_stop = stop.clone();
    let clip_gate = clip_on.clone();
    let cb = std::thread::spawn(move || {
        clipboard_loop(clip_ctl, clip_rx, clip_gate, clip_stop);
    });
    let ctl_for_input = ctl_tx.clone();
    let stop_i = stop.clone();
    let i = std::thread::spawn(move || {
        use render::window::VideoInput;
        // Mouse-move coalescing: gaming mice emit up to 1000 moves/s; relayed
        // over a reliable ordered channel they queue AHEAD of clicks/keys and
        // make input feel drunk on slow links. Cap moves to ~100/s, always
        // sending the LATEST position (never a stale one). Clicks/keys/wheel
        // are never delayed by more than the coalescing drain itself.
        const MOVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
        let mut last_move_sent = std::time::Instant::now() - MOVE_INTERVAL;
        while !stop_i.load(Ordering::SeqCst) {
            match input_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(mut ev) => {
                    let mut follow: Option<VideoInput> = None;
                    if matches!(ev, VideoInput::Move { .. }) {
                        // Pace: wait out the interval, then drain the backlog so
                        // we forward the newest position, not a stale one.
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
                        last_move_sent = std::time::Instant::now();
                    }
                    if let Some(msg) = translate_input(ev) {
                        let _ = ctl_for_input.send(serde_json::to_vec(&msg).unwrap_or_default());
                    }
                    if let Some(f) = follow {
                        if let Some(msg) = translate_input(f) {
                            let _ =
                                ctl_for_input.send(serde_json::to_vec(&msg).unwrap_or_default());
                        }
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
        bulk_rx: Some(bulk_rx),
        clip_tx: clip_tx.clone(),
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
    };
    let t = std::thread::spawn(move || transport_driver(driver));

    // Viewer decode→render thread. It holds a ctl sender so it can ask the host
    // for a fresh keyframe when frames arrive but none decode (lost keyframe).
    let stop_r = stop.clone();
    let ctl_for_kf = ctl_tx.clone();
    let r = std::thread::spawn(move || {
        if let Err(e) = viewer_media_loop(video_rx, ctl_for_kf, stop_r) {
            tracing::warn!("viewer media loop ended: {e}");
        }
    });

    let mut threads = vec![t, r, i, f, cb];

    // Shortcut capture (§8a): grab OS-reserved combos (Alt+Tab, Win) via
    // WH_KEYBOARD_LL while the session window is foreground and forward them to
    // the host. NOTE: whether the host actually ACTS on injected Alt+Tab depends
    // on the host process being elevated or UIAccess-signed — Windows blocks
    // shell-shortcut injection from a plain medium-integrity app (documented).
    // The hook is installed regardless (the stuck-key guards make it safe); it
    // just becomes effective once the host runs elevated/installed. `has_uiaccess`
    // is logged so the session log states plainly whether it can work.
    {
        tracing::info!(
            "shortcut capture on (uiaccess={}); remote Alt+Tab needs the host elevated or signed",
            elevation::process_has_uiaccess()
        );
        input::keyhook::set_focus_root(RENDER_HWND.load(Ordering::SeqCst));
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
        clip_on,
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

/// Viewer: temporarily hide/show the native video surface so web UI overlays
/// (the settings modal) are visible during a session — the video child HWND
/// sits above the WebView2 and would otherwise cover them. The stream keeps
/// running; only presentation is hidden.
pub fn set_video_visible(visible: bool) {
    let hwnd_raw = RENDER_HWND.load(Ordering::SeqCst);
    if hwnd_raw == 0 {
        return;
    }
    if visible {
        render::window::show(hwnd_raw);
    } else {
        render::window::hide(hwnd_raw);
    }
}

/// Host: flip the injection gate when the live permission changes (§4.2). When
/// control is revoked the transport thread releases any held input.
pub fn set_control(allow: bool) {
    if let Some(s) = SESSION.lock().as_ref() {
        s.control.store(allow, Ordering::SeqCst);
        // Clipboard exchange is part of "control" — revoking it must stop the
        // peer reading this machine's clipboard too, not just its input.
        s.clip_on.store(
            allow && CLIPBOARD_SETTING.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
    }
}

/// Tear the session down: signal all threads to stop and join them.
pub fn teardown(_engine: &Engine) {
    // Stop capturing viewer input before threads join (idempotent).
    render::window::clear_input_sink();
    render::window::clear_file_drop_sink();
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
        bulk_rx,
        clip_tx,
        channels,
        video_mid,
        force_key,
        bitrate,
        link: link_bw,
        ui,
        role,
        socket,
        mut turn,
        stop,
    } = d;
    use transport::{Inbound, Transport};

    // Liveness: report the disconnect to the UI and reset the role exactly once,
    // however the loop exits (ICE dead, consent lost, inactivity, error). Without
    // this the controller sits on a dead session with no notification.
    let notify_dead = {
        let ui = ui.clone();
        let role = role.clone();
        move |reason: &str| {
            let was_active = !matches!(&*role.lock(), crate::Role::Idle);
            if was_active {
                *role.lock() = crate::Role::Idle;
                // Take the native video surface down FIRST. It sits on top of the
                // web UI, so leaving it up means the user keeps staring at the
                // last decoded frame while the app thinks it is idle — the
                // "it just froze instead of disconnecting" report. Input capture
                // goes with it so a dead session can't still grab keystrokes.
                set_video_visible(false);
                render::window::clear_input_sink();
                render::window::clear_file_drop_sink();
                let _ = ui.send(crate::UiEvent::Toast(format!("Disconnected: {reason}")));
                let _ = ui.send(crate::UiEvent::RoleChanged(crate::Role::Idle));
            }
            was_active
        }
    };

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
    // Host: adopt the data-channel ids + video media-track mid created in
    // `begin_host`, and tell the BWE how high to probe (up to our max bitrate).
    if let Some(ch) = channels {
        tp.set_channels(ch);
    }
    if let Some(mid) = video_mid {
        tp.set_video_mid(mid);
    }
    let host_side = frame_rx.is_some();
    if host_side {
        tp.set_desired_bitrate(DESIRED_BITRATE);
    }

    // ctl messages are BUFFERED until the ctl channel opens (they're control
    // state like the initial Perm — losing them desyncs the session). Video is a
    // media track: `send_video` no-ops until it's negotiated + connected, and a
    // keyframe is forced on connect so the first delivered frame is decodable.
    let mut ctl_open = false;
    let mut cursor_open = false;
    let mut bulk_open = false;
    // Host: the file transfer currently being received, if any, and where on
    // this screen the sender aimed it.
    let mut incoming: Option<crate::transfer::Incoming> = None;
    let mut incoming_drop_at: Option<(f64, f64)> = None;
    let mut ctl_backlog: Vec<Vec<u8>> = Vec::new();

    // Liveness watchdog: if NOTHING arrives from the peer for this long, the
    // connection is dead even if ICE hasn't declared it yet. str0m's ICE consent
    // checks also emit Disconnected within a few seconds; this is the backstop.
    /// Backstop only. It must sit ABOVE the ping/pong grace below, or it would
    /// kill the session before the user ever sees the countdown — the app-level
    /// keepalive owns that decision because it can explain itself to the user.
    const DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(20);
    let mut last_rx = std::time::Instant::now();
    let mut connected = false; // becomes true on the first Connected event

    // Application-level ping/pong. UDP liveness (`last_rx`) counts STUN/TURN
    // keepalives too, so it can look healthy while the session itself is wedged.
    // Both sides ping; both sides enforce the grace period, so a dead link tears
    // BOTH ends down rather than leaving one staring at a frozen picture.
    const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    /// How long the peer may stay silent before the session is given up on.
    const PONG_GRACE: std::time::Duration = std::time::Duration::from_secs(15);
    /// Silence beyond this is reported to the user (with a countdown) while we
    /// keep waiting. Long enough not to fire on ordinary jitter, short enough
    /// that a frozen picture is explained rather than left a mystery.
    const TROUBLE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
    let mut last_ping_sent = std::time::Instant::now();
    let mut last_pong = std::time::Instant::now();
    let mut ping_seq: u32 = 0;
    let mut link_trouble = false;
    let mut last_trouble_tick = std::time::Instant::now();

    // Viewer: NEVER hand the decoder a delta whose reference chain is broken.
    // str0m NACK-repairs most loss; when a gap survives its reorder window it
    // delivers the next frame with `contiguous=false`. From that instant every
    // delta is undecodable garbage (the "random black/smeared pixels") — drop
    // them all and request a keyframe (rate-limited) until an IDR arrives.
    let mut drop_till_key = false;
    let mut last_pli = std::time::Instant::now() - std::time::Duration::from_secs(5);

    // Periodic pipeline stats (both sides, every 5s) so field tests yield DATA:
    // host logs frames/IDRs sent + applied bitrate + last BWE estimate; viewer
    // logs frames received + gaps + frames dropped while awaiting an IDR.
    let mut last_stats = std::time::Instant::now();
    let mut stat_sent: u64 = 0;
    let mut stat_idr: u64 = 0;
    let mut stat_gaps: u64 = 0;
    let mut stat_dropped: u64 = 0;
    let mut stat_pli: u64 = 0;
    let mut last_est: u32 = 0;

    let mut buf = [0u8; 2048];
    let mut video_count: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        // Liveness: once connected, no traffic for DEAD_AFTER ⇒ declare it dead.
        if connected && last_rx.elapsed() > DEAD_AFTER {
            tracing::warn!("transport: no data for {DEAD_AFTER:?} — declaring dead");
            notify_dead("no response from the peer");
            break;
        }
        // Application-level keepalive: ping on a timer, and give up if the peer
        // has not answered within the grace period. Best-effort Bye first so the
        // other end tears down promptly instead of waiting out its own timer.
        if connected && ctl_open {
            if last_ping_sent.elapsed() >= PING_EVERY {
                last_ping_sent = std::time::Instant::now();
                ping_seq = ping_seq.wrapping_add(1);
                let _ = tp.send_ctl(&serialize(&protocol::ControlMsg::Ping { seq: ping_seq }));
            }
            let silent = last_pong.elapsed();
            if silent > PONG_GRACE {
                tracing::warn!("transport: no pong for {PONG_GRACE:?} — declaring dead");
                let _ = tp.send_ctl(&serialize(&protocol::ControlMsg::Bye));
                notify_dead("the peer stopped responding");
                break;
            } else if silent >= TROUBLE_AFTER {
                if !link_trouble {
                    link_trouble = true;
                    tracing::warn!("transport: peer quiet — {PONG_GRACE:?} grace started");
                    // Stop presenting a stale frame while we wait. Without this
                    // the viewer sees a frozen picture that is indistinguishable
                    // from a working session, which is the whole complaint.
                    if !host_side {
                        set_video_visible(false);
                    }
                }
                // Re-emit ~1/s so the UI can count down.
                if last_trouble_tick.elapsed() >= std::time::Duration::from_millis(900) {
                    last_trouble_tick = std::time::Instant::now();
                    let left = PONG_GRACE.saturating_sub(silent).as_secs() as u32;
                    let _ = ui.send(crate::UiEvent::LinkTrouble { secs_left: left });
                }
            } else if link_trouble {
                // Answered again within the grace period — carry on.
                link_trouble = false;
                tracing::info!("transport: peer responded again — session continues");
                if !host_side {
                    set_video_visible(true);
                    // The stream may have advanced without us; get a clean start.
                    let _ = tp.send_ctl(&serialize(&protocol::ControlMsg::KeyframeRequest));
                }
                let _ = ui.send(crate::UiEvent::LinkRestored);
            }
        }
        // Field-test telemetry: one INFO line per 5s per side.
        if connected && last_stats.elapsed() >= std::time::Duration::from_secs(5) {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = std::time::Instant::now();
            if host_side {
                tracing::info!(
                    "video/tx: {:.1} fps, {} idr, {} pli in, applied {} kbps, bwe {} kbps",
                    stat_sent as f64 / secs,
                    stat_idr,
                    stat_pli,
                    bitrate
                        .as_ref()
                        .map(|b| b.load(Ordering::SeqCst) / 1000)
                        .unwrap_or(0),
                    last_est / 1000,
                );
            } else {
                tracing::info!(
                    "video/rx: {:.1} fps, {} gap(s), {} dropped awaiting idr",
                    stat_sent as f64 / secs, // viewer: frames received this window
                    stat_gaps,
                    stat_dropped,
                );
            }
            stat_sent = 0;
            stat_idr = 0;
            stat_gaps = 0;
            stat_dropped = 0;
            stat_pli = 0;
        }
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
            if ctl_open {
                let _ = tp.send_ctl(&bytes);
            } else {
                ctl_backlog.push(bytes);
            }
        }
        if let Some(frame_rx) = &frame_rx {
            while let Ok((au, keyframe)) = frame_rx.try_recv() {
                // Write onto the RTP media track. str0m packetizes, paces, and
                // NACK-repairs; it no-ops until the track is connected. The encoder
                // bitrate is steered by the BWE back-off (Inbound::BweEstimate
                // below), not a hand-rolled send-queue heuristic.
                stat_sent += 1;
                if keyframe {
                    stat_idr += 1;
                }
                let _ = tp.send_video(&au);
            }
        }
        // Host: cursor position updates on the cursor channel (§5a/§7). Stale
        // pre-open positions are worthless — drop, don't buffer.
        if let Some(cursor_rx) = &cursor_rx {
            while let Ok(bytes) = cursor_rx.try_recv() {
                if cursor_open {
                    let _ = tp.send_cursor(&bytes);
                }
            }
        }
        // Viewer: outbound file bytes, WITH BACKPRESSURE. Queueing a whole file
        // at once would hand SCTP megabytes to push as fast as it can, starving
        // the video stream for the entire transfer; stopping while the channel
        // already holds a backlog keeps the session usable while files move.
        if let Some(bulk_rx) = &bulk_rx {
            if bulk_open {
                const BULK_QUEUE_LIMIT: usize = 256 * 1024;
                while tp.bulk_buffered() < BULK_QUEUE_LIMIT {
                    match bulk_rx.try_recv() {
                        Ok(frame) => {
                            let _ = tp.send_bulk(&frame);
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // 2.5) TURN housekeeping: refresh the allocation before it expires.
        if let Some(alloc) = turn.as_mut() {
            alloc.tick(&socket);
        }

        // 3) Drive str0m: emit transmits, handle timeouts, surface events.
        match tp.poll_output() {
            Ok(str0m::Output::Transmit(t)) => {
                // Transmits sourced from the relayed address go via the TURN
                // server (Send Indication); everything else is direct UDP.
                let via_relay = turn.as_ref().is_some_and(|alloc| t.source == alloc.relayed);
                if via_relay {
                    if let Some(alloc) = turn.as_mut() {
                        alloc.send_via_relay(&socket, t.destination, &t.contents);
                    }
                } else {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
            }
            Ok(str0m::Output::Timeout(_)) => {
                // 4) Drain ALL pending UDP into str0m (bounded per cycle), then
                // advance time if there was nothing. One-packet-per-poll-cycle
                // couldn't keep up at video rates (~300 pkts/s + NACK + TWCC
                // feedback), which distorted the BWE's arrival timing and delayed
                // loss repair. The bound keeps transmits flowing under flood.
                let mut received = 0u32;
                while received < 64 {
                    match socket.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            received += 1;
                            // Liveness: any datagram from the peer proves the link.
                            last_rx = std::time::Instant::now();
                            // Packets from the TURN server: unwrap Data Indications
                            // (relayed peer traffic, reported as arriving on the
                            // relayed address) and consume control responses.
                            let is_turn_server =
                                turn.as_ref().is_some_and(|alloc| from == alloc.server);
                            if is_turn_server {
                                let alloc = turn.as_mut().expect("checked above");
                                if let Some((peer, data)) = alloc.handle_server_packet(&buf[..n]) {
                                    let recv = str0m::net::Receive::new(
                                        str0m::net::Protocol::Udp,
                                        peer,
                                        alloc.relayed,
                                        &data,
                                    );
                                    if let Ok(recv) = recv {
                                        let _ = tp.handle_input(str0m::Input::Receive(
                                            std::time::Instant::now(),
                                            recv,
                                        ));
                                    }
                                }
                            } else if let Ok(local) = socket.local_addr() {
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
                        Err(_) => break, // WouldBlock — nothing pending
                    }
                }
                if received == 0 {
                    let _ = tp.handle_input(str0m::Input::Timeout(std::time::Instant::now()));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            Ok(str0m::Output::Event(ev)) => {
                if let Some(inbound) = tp.on_event(ev) {
                    match inbound {
                        Inbound::Connected => {
                            connected = true;
                            last_rx = std::time::Instant::now();
                            last_pong = std::time::Instant::now(); // start the grace period here
                                                                   // Host: the viewer's decoder has no reference yet, and
                                                                   // anything written before connect was dropped — force an
                                                                   // IDR so the first *delivered* frame is decodable.
                            if let Some(fk) = &force_key {
                                fk.store(true, Ordering::SeqCst);
                            }
                            tracing::info!(
                                "transport connected ({})",
                                if inject.is_some() { "host" } else { "viewer" }
                            );
                        }
                        Inbound::ChannelOpen(_, label) => {
                            tracing::info!("data channel open: {label}");
                            match label.as_str() {
                                "ctl" => {
                                    ctl_open = true;
                                    // Ping/pong only starts now — don't count the
                                    // channel-setup time against the grace period.
                                    last_pong = std::time::Instant::now();
                                    for bytes in ctl_backlog.drain(..) {
                                        let _ = tp.send_ctl(&bytes);
                                    }
                                }
                                "cursor" => cursor_open = true,
                                "bulk" => bulk_open = true,
                                _ => {} // "init" — SDP bootstrap channel, unused
                            }
                        }
                        Inbound::Video {
                            data,
                            keyframe,
                            contiguous,
                        } => {
                            video_count += 1;
                            stat_sent += 1;
                            if video_count <= 5 {
                                tracing::info!(
                                    "viewer: received video frame #{video_count} ({} bytes, keyframe={keyframe})",
                                    data.len(),
                                );
                            }
                            // CORRECTNESS GATE: a non-contiguous frame means loss
                            // survived str0m's NACK + reorder window, so every delta
                            // from here is undecodable garbage (black/smeared
                            // blocks). Drop them ALL until an IDR arrives; keep
                            // asking for one (rate-limited) on both paths — native
                            // RTCP PLI and the reliable ctl channel.
                            if !contiguous && !keyframe && !drop_till_key {
                                stat_gaps += 1;
                                drop_till_key = true;
                            }
                            if drop_till_key {
                                if keyframe {
                                    drop_till_key = false; // clean restart point
                                } else {
                                    stat_dropped += 1;
                                    if last_pli.elapsed() >= std::time::Duration::from_millis(500) {
                                        tp.request_keyframe();
                                        let _ = tp.send_ctl(&serialize(
                                            &protocol::ControlMsg::KeyframeRequest,
                                        ));
                                        last_pli = std::time::Instant::now();
                                    }
                                    continue; // NEVER feed a broken delta
                                }
                            }
                            if let Some(tx) = &video_tx {
                                let _ = tx.send((data, keyframe));
                            }
                        }
                        Inbound::KeyframeRequest => {
                            // Host: the peer (or str0m on its behalf) asked for a
                            // keyframe over RTCP. Counted, because this path was
                            // invisible in the field logs while the ctl-channel
                            // requests were counted — leaving most IDRs
                            // unattributed. The host rate-limits what it actually
                            // emits (see `host_media_loop`).
                            stat_pli += 1;
                            if let Some(fk) = &force_key {
                                fk.store(true, Ordering::SeqCst);
                            }
                        }
                        Inbound::BweEstimate(bps) => {
                            last_est = bps;
                            // The pacer needs the RAW capacity to decide how many
                            // frames the link can actually carry.
                            if let Some(lk) = &link_bw {
                                lk.store(bps.max(100_000), Ordering::SeqCst);
                            }
                            // The encoder's target is the estimate FLOORED at
                            // MIN_BITRATE: quality never drops below legible, even
                            // when that means the link can't take every frame.
                            // 15% hysteresis stops estimate jitter from becoming
                            // visible quality flicker.
                            if let Some(br) = &bitrate {
                                let next = bps.clamp(MIN_BITRATE, TARGET_BITRATE);
                                let cur = br.load(Ordering::SeqCst);
                                if next.abs_diff(cur) * 100 > cur * 15 {
                                    tracing::info!(
                                        "bwe: link {} kbps → encoder {} kbps",
                                        bps / 1000,
                                        next / 1000
                                    );
                                    br.store(next, Ordering::SeqCst);
                                }
                            }
                        }
                        Inbound::Ctl(bytes) => {
                            // Control messages both sides understand, handled once
                            // up front. Anything that isn't one falls through to
                            // the host's input-injection path below.
                            match serde_json::from_slice::<protocol::ControlMsg>(&bytes) {
                                // Liveness: answer probes, and count any reply as
                                // proof the peer is alive.
                                Ok(protocol::ControlMsg::Ping { seq }) => {
                                    last_pong = std::time::Instant::now();
                                    let _ = tp
                                        .send_ctl(&serialize(&protocol::ControlMsg::Pong { seq }));
                                    continue;
                                }
                                Ok(protocol::ControlMsg::Pong { .. }) => {
                                    last_pong = std::time::Instant::now();
                                    continue;
                                }
                                // A viewer keyframe request restarts the stream
                                // (decoder never started / lost the keyframe). NOT
                                // gated on control — view-only viewers need it too.
                                Ok(protocol::ControlMsg::KeyframeRequest) if host_side => {
                                    tracing::info!("viewer requested keyframe");
                                    if let Some(fk) = &force_key {
                                        fk.store(true, Ordering::SeqCst);
                                    }
                                    continue;
                                }
                                // The viewer told us its display size — encode to
                                // exactly that so it presents 1:1 (no resampling
                                // blur) and we don't spend bits on unseen pixels.
                                Ok(protocol::ControlMsg::ViewSize { width, height })
                                    if host_side =>
                                {
                                    VIEW_SIZE.store(
                                        ((width as u64) << 32) | height as u64,
                                        Ordering::SeqCst,
                                    );
                                    continue;
                                }
                                // Clipboard text from the peer. On the host this
                                // is gated on the live control permission, same
                                // as input and file receipt.
                                Ok(protocol::ControlMsg::Clipboard { text }) => {
                                    let allowed = inject
                                        .as_ref()
                                        .map(|gate| gate.load(Ordering::SeqCst))
                                        .unwrap_or(true); // viewer: no gate
                                    if allowed && text.len() <= crate::clipboard::MAX_TEXT_BYTES {
                                        let _ = clip_tx.send(text);
                                    }
                                    continue;
                                }
                                // A clean goodbye from either end.
                                Ok(protocol::ControlMsg::Bye) => {
                                    notify_dead("the other side ended the session");
                                    break;
                                }
                                _ => {}
                            }
                            // Host: anything else on ctl is remote input — inject
                            // it, gated on the current control permission (§4.1).
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
                        }
                        Inbound::Bulk(bytes) => {
                            // Host: an inbound file. Writing to this machine's
                            // disk is at least as privileged as injecting input,
                            // so it needs the SAME live control permission — a
                            // view-only peer cannot drop files here.
                            let allowed = inject
                                .as_ref()
                                .is_some_and(|gate| gate.load(Ordering::SeqCst));
                            if !allowed {
                                if incoming.take().is_some() {
                                    tracing::warn!("file transfer aborted: control revoked");
                                }
                                continue;
                            }
                            let Some(frame) = protocol::BulkFrame::decode(&bytes) else {
                                continue; // malformed — ignore, never trust it
                            };
                            match frame {
                                protocol::BulkFrame::Begin(meta) => {
                                    // A new Begin supersedes any transfer in
                                    // flight; drop that partial file first.
                                    if let Some(prev) = incoming.take() {
                                        prev.abort();
                                    }
                                    let dir = crate::transfer::receive_dir();
                                    incoming_drop_at = meta.drop_at;
                                    match crate::transfer::Incoming::begin(&meta, &dir) {
                                        Ok(inc) => {
                                            tracing::info!(
                                                "receiving {} ({} bytes)",
                                                meta.name,
                                                meta.size
                                            );
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
                                                "Receiving {}…",
                                                meta.name
                                            )));
                                            incoming = Some(inc);
                                        }
                                        Err(e) => {
                                            tracing::warn!("refused file {}: {e}", meta.name);
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
                                                "Refused file: {e}"
                                            )));
                                        }
                                    }
                                }
                                protocol::BulkFrame::Chunk(data) => {
                                    if let Some(inc) = incoming.as_mut() {
                                        if let Err(e) = inc.write(&data) {
                                            tracing::warn!("file transfer failed: {e}");
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
                                                "Transfer failed: {e}"
                                            )));
                                            if let Some(bad) = incoming.take() {
                                                bad.abort();
                                            }
                                        }
                                    }
                                }
                                protocol::BulkFrame::End => {
                                    if let Some(inc) = incoming.take() {
                                        match inc.finish() {
                                            Ok(path) => {
                                                tracing::info!("received file {path:?}");
                                                let name = path
                                                    .file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy()
                                                    .into_owned();
                                                let pasted = deliver_received_file(
                                                    &path,
                                                    incoming_drop_at.take(),
                                                    injector.as_mut(),
                                                );
                                                let _ = ui.send(crate::UiEvent::Toast(if pasted {
                                                    format!("Pasted {name}")
                                                } else {
                                                    format!(
                                                        "{name} is on the clipboard \
                                                         (also saved to Downloads\\ShareCtrlScreen)"
                                                    )
                                                }));
                                            }
                                            Err(e) => {
                                                let _ = ui.send(crate::UiEvent::Toast(format!(
                                                    "Transfer failed: {e}"
                                                )));
                                            }
                                        }
                                    }
                                }
                                protocol::BulkFrame::Abort(reason) => {
                                    if let Some(inc) = incoming.take() {
                                        inc.abort();
                                    }
                                    tracing::warn!("sender aborted transfer: {reason}");
                                    let _ = ui.send(crate::UiEvent::Toast(
                                        "Transfer cancelled by sender".into(),
                                    ));
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
                            notify_dead("the connection dropped");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("transport poll error: {e}");
                notify_dead("transport error");
                break;
            }
        }
    }

    // Local teardown (the user hit Disconnect): say goodbye on the DATA channel
    // as well. `Engine::end_session` announces over signaling, which is useless
    // when the signaling socket is itself the thing that broke — the peer would
    // then sit out the full grace period instead of dropping immediately.
    if stop.load(Ordering::SeqCst) && ctl_open && connected {
        let _ = tp.send_ctl(&serialize(&protocol::ControlMsg::Bye));
        // The loop is over, so nothing else will flush str0m's queue — pump it
        // briefly by hand, bounded so teardown can't hang on a dead socket.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            match tp.poll_output() {
                Ok(str0m::Output::Transmit(t)) => {
                    let via_relay = turn.as_ref().is_some_and(|a| t.source == a.relayed);
                    if via_relay {
                        if let Some(alloc) = turn.as_mut() {
                            alloc.send_via_relay(&socket, t.destination, &t.contents);
                        }
                    } else {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                }
                // Nothing queued right now — the Bye is on the wire.
                _ => break,
            }
        }
    }

    // If the transport died (role still active), also stop the session's other
    // threads so nothing keeps running behind a dead connection.
    if !matches!(&*role.lock(), crate::Role::Idle) {
        stop.store(true, Ordering::SeqCst);
    }
    // Host: whatever ends the session (disconnect, teardown, transport error),
    // never leave remotely-injected keys/buttons held down on this machine.
    if let Some(inj) = injector.as_mut() {
        inj.release_all();
    }
}

/// The largest `src`-aspect size that fits inside `want`, never upscaling past
/// `src`, rounded to even dimensions (H.264 chroma is subsampled 2x2 and MFTs
/// reject odd sizes). Preserving the source aspect matters: the viewer letterboxes
/// to the same ratio, so anything else would be displayed with bars AND rescaled.
fn fit_within(want: (u32, u32), src: (u32, u32)) -> (u32, u32) {
    if want.0 == 0 || want.1 == 0 || src.0 == 0 || src.1 == 0 {
        return src;
    }
    // Scale by the tighter of the two axes, capped at 1.0 (no upscaling — it
    // would cost bitrate without adding any real detail).
    let sx = want.0 as f64 / src.0 as f64;
    let sy = want.1 as f64 / src.1 as f64;
    let s = sx.min(sy).min(1.0);
    let w = ((src.0 as f64 * s).round() as u32) & !1;
    let h = ((src.1 as f64 * s).round() as u32) & !1;
    (w.max(2), h.max(2))
}

// ---- File push (viewer → host) ----------------------------------------------

/// Read each dropped file and emit it as bulk frames. One file at a time: the
/// bulk channel is ordered and a transfer has no id, so overlapping them would
/// interleave two files into one.
fn file_send_loop(
    drops: Receiver<render::window::FileDrop>,
    out: Sender<Vec<u8>>,
    ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    stop: Arc<AtomicBool>,
) {
    use protocol::transfer::{BulkFrame, FileMeta, CHUNK_BYTES, MAX_FILE_BYTES};
    use std::io::Read;

    while !stop.load(Ordering::SeqCst) {
        let Ok((paths, drop_x, drop_y)) = drops.recv_timeout(std::time::Duration::from_millis(200))
        else {
            continue;
        };
        for path in paths {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(meta) = std::fs::metadata(&path) else {
                let _ = ui.send(crate::UiEvent::Toast(format!("Cannot read {name}")));
                continue;
            };
            if meta.is_dir() {
                // Folders would need a manifest and per-entry paths — exactly the
                // shape that invites traversal bugs. Files only, for now.
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "{name} is a folder — send files instead"
                )));
                continue;
            }
            let size = meta.len();
            if size > MAX_FILE_BYTES {
                let _ = ui.send(crate::UiEvent::Toast(format!("{name} is too large")));
                continue;
            }
            let Ok(mut file) = std::fs::File::open(&path) else {
                let _ = ui.send(crate::UiEvent::Toast(format!("Cannot open {name}")));
                continue;
            };

            let _ = ui.send(crate::UiEvent::Toast(format!("Sending {name}…")));
            let _ = out.send(
                BulkFrame::Begin(FileMeta {
                    name: name.clone(),
                    size,
                    // Where it was dropped, so the host can paste it into the
                    // window under that point rather than just filing it away.
                    drop_at: Some((drop_x, drop_y)),
                })
                .encode(),
            );

            let mut buf = vec![0u8; CHUNK_BYTES];
            let mut sent: u64 = 0;
            let mut failed = false;
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if out
                            .send(BulkFrame::Chunk(buf[..n].to_vec()).encode())
                            .is_err()
                        {
                            failed = true; // session ended under us
                            break;
                        }
                        sent += n as u64;
                    }
                    Err(e) => {
                        let _ = out.send(BulkFrame::Abort(format!("read error: {e}")).encode());
                        let _ = ui.send(crate::UiEvent::Toast(format!("Failed sending {name}")));
                        failed = true;
                        break;
                    }
                }
                if stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            if failed {
                continue;
            }
            if sent != size {
                // The file changed while we read it — tell the peer rather than
                // let it write a file that doesn't match what was announced.
                let _ = out.send(BulkFrame::Abort("file changed while sending".into()).encode());
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "{name} changed while sending"
                )));
                continue;
            }
            let _ = out.send(BulkFrame::End.encode());
        }
    }
}

/// Make a just-received file behave like one dropped locally.
///
/// Windows gives no way to synthesize a real OLE drop into another process, so
/// this does the next best thing, which every app that accepts a pasted file
/// treats identically: put the file on the clipboard as a `CF_HDROP` file-drop
/// list (exactly what Explorer writes when you copy a file), focus the window
/// the sender aimed at, and press Ctrl+V.
///
/// Focus is set WITHOUT clicking: a real drop doesn't click either, and a
/// synthetic click could press whatever button sits under the drop point.
/// Returns whether the paste was actually attempted; the file is on the
/// clipboard (and on disk) either way.
fn deliver_received_file(
    path: &std::path::Path,
    drop_at: Option<(f64, f64)>,
    injector: Option<&mut input::Injector>,
) -> bool {
    if crate::clipboard::set_files(std::slice::from_ref(&path.to_path_buf())).is_none() {
        tracing::warn!("could not put {path:?} on the clipboard");
        return false;
    }
    if !PASTE_DROPPED.load(Ordering::SeqCst) {
        return false;
    }
    let (Some((nx, ny)), Some(inj)) = (drop_at, injector) else {
        return false; // saved + on the clipboard; the user pastes it themselves
    };
    if !input::focus_window_at(nx, ny) {
        tracing::info!("no window under the drop point — file left on the clipboard");
        return false;
    }
    // Give the focused window a moment to become foreground before typing.
    std::thread::sleep(std::time::Duration::from_millis(120));
    inj.key("ControlLeft", true);
    inj.key("KeyV", true);
    inj.key("KeyV", false);
    inj.key("ControlLeft", false);
    tracing::info!("pasted {path:?} at ({nx:.3}, {ny:.3})");
    true
}

// ---- Clipboard sync (both directions) ---------------------------------------

/// Mirror clipboard TEXT between the two machines. Polls the OS change counter
/// (cheap, no window needed) and sends on change. Applying a value received from
/// the peer bumps that same counter, so the resulting sequence number is
/// remembered and skipped — otherwise each side would echo the other forever.
fn clipboard_loop(
    ctl_tx: Sender<Vec<u8>>,
    inbound: Receiver<String>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut last_seq = crate::clipboard::sequence();
    let mut last_text: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        // Apply anything the peer sent first, so our own poll below sees the
        // resulting sequence number and treats it as ours.
        while let Ok(text) = inbound.try_recv() {
            if !enabled.load(Ordering::SeqCst) {
                continue;
            }
            if last_text.as_deref() == Some(text.as_str()) {
                continue; // already have it — don't touch the clipboard at all
            }
            if let Some(seq) = crate::clipboard::set_text(&text) {
                last_seq = seq;
                last_text = Some(text);
            }
        }

        if enabled.load(Ordering::SeqCst) {
            let seq = crate::clipboard::sequence();
            if seq != last_seq {
                last_seq = seq;
                if let Some(text) = crate::clipboard::get_text() {
                    let changed = last_text.as_deref() != Some(text.as_str());
                    if changed && text.len() <= crate::clipboard::MAX_TEXT_BYTES {
                        last_text = Some(text.clone());
                        let _ = ctl_tx.send(serialize(&ControlMsg::Clipboard { text }));
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

// ---- Host capture → encode --------------------------------------------------

fn host_media_loop(
    frame_tx: Sender<(Vec<u8>, bool)>,
    cursor_tx: Sender<Vec<u8>>,
    bitrate: Arc<std::sync::atomic::AtomicU32>,
    link: Arc<std::sync::atomic::AtomicU32>,
    force_key: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use codec::{Encoder, EncoderConfig};

    let mut dup = capture::Duplicator::new(0, 0)?;
    let codec = negotiated_codec();
    // Encode at the screen's REAL size. Assuming 1920x1080 made the GPU
    // converter rescale every frame on any other monitor (soft picture, wrong
    // aspect) — the viewer adapts to whatever size actually arrives.
    let (cap_w, cap_h) = dup.dimensions();
    let mut cfg = EncoderConfig {
        codec,
        bitrate_bps: bitrate.load(Ordering::SeqCst),
        // Even dimensions: H.264 chroma is subsampled 2x2 and MFTs reject odd sizes.
        width: (cap_w & !1).max(2),
        height: (cap_h & !1).max(2),
        fps_num: MAX_FPS,
        fps_den: 1,
    };
    // Encoder shares the capture device (§5c zero-copy).
    let mut encoder = Encoder::new(dup.device(), cfg)?;
    tracing::info!(
        "host: capture+encoder ready ({}x{} @{}fps, {}, {} kbps)",
        cfg.width,
        cfg.height,
        MAX_FPS,
        codec.as_caps_str(),
        cfg.bitrate_bps / 1000,
    );
    // The first emitted frame must be an IDR so the viewer can start decoding.
    encoder.force_keyframe();
    let mut applied_bitrate = cfg.bitrate_bps;
    let mut encoder_size = (cfg.width, cfg.height);
    // A viewer size the encoder refused — don't retry it every frame.
    let mut rejected_size: Option<(u32, u32)> = None;
    let mut sent: u64 = 0;
    let mut last_frame_at = std::time::Instant::now();

    // Source pacing = the "readable but slower" trade, made explicit. The encoder
    // now has a hard QUALITY floor (codec::MAX_QP), so when the screen needs more
    // bits than the link can carry it OVERSHOOTS the bitrate instead of blurring.
    // This token bucket absorbs that: we track the bytes actually produced and,
    // when we're ahead of what the link can drain, we skip capturing the next
    // frame rather than let the encoder degrade. Sharpness is preserved; frame
    // rate is what gives. (Bucket is capped at ~1s of credit so an idle screen
    // can still burst a full keyframe out immediately when something changes.)
    let mut debt_bytes: i64 = 0;
    let mut last_debt_at = std::time::Instant::now();

    // Keyframe rate limiting + attribution (see the emit site below). The first
    // keyframe must not be delayed, so start the clock a full gap in the past.
    const MIN_KEYFRAME_GAP: std::time::Duration = std::time::Duration::from_millis(1000);
    let mut last_keyframe_at = std::time::Instant::now() - MIN_KEYFRAME_GAP;
    let mut requested_idr: u32 = 0; // we asked for it (connect / PLI / resize)
    let mut encoder_idr: u32 = 0; // Media Foundation decided on its own
    let mut deferred_keys: u32 = 0; // requests the rate limiter held back
    let mut last_key_log = std::time::Instant::now();

    while !stop.load(Ordering::SeqCst) {
        // Frame-rate cap: never encode faster than MAX_FPS_INTERVAL. The pacing
        // sleep only bites when we're running FASTER than the cap (a busy screen);
        // a slow software encoder or an idle screen sets the real rate.
        let since = last_frame_at.elapsed();
        if since < MAX_FPS_INTERVAL {
            std::thread::sleep(MAX_FPS_INTERVAL - since);
        }
        last_frame_at = std::time::Instant::now();

        // §6 adaptive bitrate: feed the current BWE target to the encoder.
        let target = bitrate.load(Ordering::SeqCst);
        if target != applied_bitrate {
            let _ = encoder.set_bitrate(target);
            applied_bitrate = target;
            cfg.bitrate_bps = target;
        }

        // Encode at the size the viewer actually displays (never upscaling past
        // our own screen), so it presents 1:1 with no resampling. The GPU
        // converter already scales, so this costs nothing extra — and encoding
        // fewer pixels puts more bits into each one. Rebuild only on a material
        // change; the viewer debounces, and every rebuild costs a keyframe.
        let want = match VIEW_SIZE.load(Ordering::SeqCst) {
            0 => (cap_w, cap_h),
            v => {
                let (vw, vh) = ((v >> 32) as u32, v as u32);
                fit_within((vw, vh), (cap_w, cap_h))
            }
        };
        if want != encoder_size && want.0 >= 2 && want.1 >= 2 && rejected_size != Some(want) {
            tracing::info!(
                "host: re-encoding at {}x{} (was {}x{})",
                want.0,
                want.1,
                encoder_size.0,
                encoder_size.1
            );
            cfg.width = want.0;
            cfg.height = want.1;
            match Encoder::new(dup.device(), cfg) {
                Ok(e) => {
                    encoder = e;
                    encoder.force_keyframe(); // new SPS/PPS — restart the decoder
                    applied_bitrate = cfg.bitrate_bps;
                    encoder_size = want;
                    rejected_size = None;
                }
                Err(e) => {
                    // Keep streaming at the working size; remember the bad one so
                    // we don't rebuild-storm on every frame.
                    tracing::warn!("host: encoder rebuild at {want:?} failed ({e}) — keeping size");
                    cfg.width = encoder_size.0;
                    cfg.height = encoder_size.1;
                    rejected_size = Some(want);
                }
            }
        }

        // Where keyframes actually come from — the open question the field logs
        // could not answer, because a request-side counter can't see the ones
        // Media Foundation inserts by itself.
        if last_key_log.elapsed() >= std::time::Duration::from_secs(5) {
            last_key_log = std::time::Instant::now();
            if requested_idr + encoder_idr + deferred_keys > 0 {
                tracing::info!(
                    "keyframes/5s: {requested_idr} requested, {encoder_idr} encoder-initiated, {deferred_keys} deferred"
                );
            }
            requested_idr = 0;
            encoder_idr = 0;
            deferred_keys = 0;
        }

        // Drain the bucket by what the LINK (not the encoder target) could have
        // carried since the last pass. These differ on a bad link: the encoder is
        // floored at MIN_BITRATE to stay legible, so if real capacity is below
        // that, the difference has to come out of the frame rate.
        let link_bps = link.load(Ordering::SeqCst).max(100_000);
        let elapsed = last_debt_at.elapsed();
        last_debt_at = std::time::Instant::now();
        let drained = (link_bps as f64 / 8.0 * elapsed.as_secs_f64()) as i64;
        debt_bytes = (debt_bytes - drained).max(0);
        // More than MIN_FPS's worth of unsent credit? Skip this capture rather
        // than pile more onto a link that hasn't drained the last frame yet.
        let max_debt = (link_bps as i64 / 8) / MIN_FPS as i64;
        if debt_bytes > max_debt && !force_key.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
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

                let has_change = !frame.pointer_only
                    && (!frame.dirty_rects.is_empty() || !frame.move_rects.is_empty());

                // KEYFRAMES ON DEMAND ONLY (infinite GOP — the AnyDesk model).
                // No periodic IDR: at CBR every IDR costs ~10x a delta, so the
                // encoder crushes its quality and deltas spend the next second
                // sharpening — a visible blur PULSE every interval ("randomly
                // blurry, then not"). No scene-change IDR either: a full-screen
                // change is just a big delta the encoder intra-codes anyway.
                // Corruption can't linger because the viewer NEVER decodes across
                // a loss gap (it drops deltas and requests an IDR — see the
                // transport driver), so on-demand recovery covers everything:
                // connect, RTCP PLI, ctl KeyframeRequest, capture AccessLost.
                // RATE-LIMITED. Field logs showed 3-5 IDRs every 5 seconds on
                // idle content, and each one is a burst big enough to spike the
                // relay's queuing delay — which congestion control reads as
                // congestion, collapsing the estimate from ~5 Mbps to ~150 kbps.
                // The pacer then starves the viewer, the starved viewer asks for
                // a keyframe, and the loop feeds itself. Honouring at most one
                // request per MIN_KEYFRAME_GAP breaks it at the narrowest point.
                // A request is never dropped, only deferred: `force_key` stays
                // set and fires as soon as the gap has elapsed.
                let want_key = force_key.load(Ordering::SeqCst);
                let key_now = want_key && last_keyframe_at.elapsed() >= MIN_KEYFRAME_GAP;
                if want_key && !key_now {
                    deferred_keys += 1;
                }
                if !has_change && !key_now {
                    dup.release();
                    continue;
                }
                if key_now {
                    encoder.force_keyframe();
                    force_key.store(false, Ordering::SeqCst);
                    last_keyframe_at = std::time::Instant::now();
                }
                // BGRA→NV12 + encode happen inside the encoder path (§5b).
                match encoder.encode(&frame.texture) {
                    Ok(units) => {
                        for u in units {
                            sent += 1;
                            // Attribute every IDR. An IDR we did NOT ask for is
                            // the encoder's own doing (Media Foundation runs its
                            // own scene-change detection and honours GOPSize), and
                            // that is the one keyframe source we cannot see from
                            // the request side — so count it separately.
                            if u.keyframe {
                                if key_now {
                                    requested_idr += 1;
                                } else {
                                    encoder_idr += 1;
                                }
                            }
                            // Charge the pacing bucket with what we actually
                            // produced — with a QP floor this can exceed the CBR
                            // target, and paying for it in frames is the point.
                            debt_bytes += u.data.len() as i64;
                            if sent == 1 || sent % 120 == 0 {
                                tracing::info!(
                                    "host: encoded+sent AU #{sent} ({} bytes, keyframe={}, bitrate={}kbps)",
                                    u.data.len(),
                                    u.keyframe,
                                    applied_bitrate / 1000
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
    video_rx: Receiver<(Vec<u8>, bool)>,
    ctl_tx: Sender<Vec<u8>>,
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
    let mut render_errors: u64 = 0;
    let mut skipped: u64 = 0;
    // Keyframe watchdog: if AUs arrive but the decoder produces nothing, the
    // keyframe was lost (unreliable channel) — ask the host for a fresh one,
    // rate-limited to ~1/s so a slow link isn't flooded.
    let mut undecoded_streak: u32 = 0;
    let mut last_kf_req = std::time::Instant::now();
    // Catch-up state: after locally dumping a delta backlog, deltas are useless
    // (their reference frames were skipped) until the requested IDR arrives.
    let mut awaiting_keyframe = false;
    // Tell the host what size we're actually displaying at, so it encodes to that
    // and we can present 1:1 (no resampling blur). Debounced: a drag-resize would
    // otherwise rebuild the host's encoder on every intermediate size.
    let mut reported_size: (u32, u32) = (0, 0);
    let mut size_settled_at = std::time::Instant::now();
    let mut pending_size: (u32, u32) = (0, 0);
    while !stop.load(Ordering::SeqCst) {
        // Track window resizes (and hover-reveal offsets) — no-op when unchanged.
        render::window::fit(hwnd_raw);
        {
            let now_size = render::window::client_size(hwnd_raw);
            if now_size != pending_size {
                pending_size = now_size; // still moving — restart the debounce
                size_settled_at = std::time::Instant::now();
            } else if now_size != reported_size
                && now_size.0 >= 320
                && now_size.1 >= 240
                && size_settled_at.elapsed() >= std::time::Duration::from_millis(600)
            {
                reported_size = now_size;
                tracing::info!("viewer: requesting {}x{} from host", now_size.0, now_size.1);
                let _ = ctl_tx.send(serialize(&protocol::ControlMsg::ViewSize {
                    width: now_size.0,
                    height: now_size.1,
                }));
            }
        }
        match video_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(first) => {
                // Batch-drain the queue, then CATCH UP instead of falling behind
                // (the AnyDesk property): software decode at ~20-30ms/frame can
                // never out-decode a backlog, so latency would compound forever.
                let mut pending: Vec<(Vec<u8>, bool)> = vec![first];
                while let Ok(next) = video_rx.try_recv() {
                    pending.push(next);
                }

                if let Some(k) = pending.iter().rposition(|(_, kf)| *kf) {
                    // A keyframe is queued: decoding from the NEWEST one is
                    // always valid — everything older is pure latency. Skip it.
                    if k > 0 {
                        skipped += k as u64;
                        tracing::debug!("viewer: catch-up — skipped {k} stale frame(s)");
                    }
                    pending.drain(..k);
                    awaiting_keyframe = false;
                } else if awaiting_keyframe {
                    // Deltas can't decode until the requested IDR arrives.
                    skipped += pending.len() as u64;
                    if last_kf_req.elapsed() >= std::time::Duration::from_secs(1) {
                        let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                        last_kf_req = std::time::Instant::now();
                    }
                    continue;
                } else if pending.len() > 6 {
                    // Hopeless delta backlog (>~200ms behind, no keyframe in
                    // sight): dump it and resync via a fresh IDR.
                    skipped += pending.len() as u64;
                    awaiting_keyframe = true;
                    tracing::info!(
                        "viewer: {} frame(s) behind — dumped backlog, requesting keyframe",
                        pending.len()
                    );
                    let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                    last_kf_req = std::time::Instant::now();
                    continue;
                }

                // Decode the batch; PRESENT only the newest decoded frame (the
                // earlier ones only exist to carry decoder references forward).
                let count = pending.len();
                for (i, (au, keyframe)) in pending.into_iter().enumerate() {
                    // 100-ns MF units at ~60 fps — decoders (esp. the software
                    // AV1 MFT) can stall on nonsense timestamps like 1,2,3….
                    ts += 166_667;
                    match decoder.decode(&au, keyframe, ts) {
                        Ok(Some(frame)) => {
                            undecoded_streak = 0;
                            rendered += 1;
                            if rendered == 1 || rendered % 120 == 0 {
                                tracing::info!(
                                    "viewer: decoded frame #{rendered} (skipped {skipped} total)"
                                );
                            }
                            if i + 1 < count {
                                continue; // reference-only; present the newest
                            }
                            // Draw the out-of-band cursor sprite on top (§7).
                            let cursor = match *CURSOR.lock() {
                                Some((x, y, true)) => Some((x, y)),
                                _ => None,
                            };
                            // A failing render is NOT silent: this was exactly the
                            // place a black screen hid (decode fine, render dead).
                            if let Err(e) =
                                renderer.render_frame(&frame.texture, frame.array_index, cursor)
                            {
                                render_errors += 1;
                                if render_errors <= 10 || render_errors % 120 == 0 {
                                    tracing::warn!("render error #{render_errors}: {e}");
                                }
                            }
                        }
                        Ok(None) => {
                            undecoded_streak += 1;
                            if undecoded_streak >= 5
                                && last_kf_req.elapsed() >= std::time::Duration::from_secs(1)
                            {
                                tracing::info!("viewer: no decodable frames — requesting keyframe");
                                let _ =
                                    ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                                last_kf_req = std::time::Instant::now();
                            }
                        }
                        Err(e) => tracing::warn!("decode error: {e}"),
                    }
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
    use super::{fit_within, parse_stun_mapped_address};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn fit_within_never_upscales() {
        // A viewer window bigger than the screen must not inflate the encode.
        assert_eq!(fit_within((3840, 2160), (1920, 1080)), (1920, 1080));
    }

    #[test]
    fn fit_within_preserves_aspect_and_evenness() {
        // Maximized 1080p viewer: some height lost to chrome. The result keeps
        // 16:9 (the viewer letterboxes to that) and stays even for 4:2:0 chroma.
        let (w, h) = fit_within((1920, 1040), (1920, 1080));
        assert_eq!((w, h), (1848, 1040));
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn fit_within_handles_degenerate_input() {
        // Unknown/zero sizes fall back to the source rather than producing 0x0.
        assert_eq!(fit_within((0, 0), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_within((1280, 720), (0, 0)), (0, 0));
    }

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
