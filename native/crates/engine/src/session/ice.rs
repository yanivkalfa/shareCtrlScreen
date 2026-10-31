//! ICE candidate gathering: host candidates, STUN server-reflexive, TURN relay.

use crate::Engine;

/// Bind the session UDP socket and register its candidates on `rtc` **before**
/// the SDP offer/answer is generated, so str0m embeds them and the peer learns
/// how to reach us. Gathers three candidate tiers (§6):
///   1. host (LAN direct path),
///   2. server-reflexive via STUN (public `ip:port` — friendly-NAT traversal),
///   3. relayed via TURN (works behind symmetric/carrier-grade NATs where
///      hole-punching fails; lowest ICE priority, so it's only used when the
///      direct paths lose).
pub(super) fn bind_and_gather(
    rtc: &mut str0m::Rtc,
    ice: &IceConfig,
) -> Option<(std::net::UdpSocket, Option<crate::turn::TurnAllocation>)> {
    // Bind to the *primary local IP*, not 0.0.0.0. str0m correlates a received
    // packet to a local ICE candidate by the destination address we report on
    // `Input::Receive` — which is `socket.local_addr()`. A wildcard bind makes
    // that `0.0.0.0:port`, matching none of our real-IP host candidates, so
    // connectivity checks never validate and ICE hangs at "Checking" (even on the
    // same LAN). Binding to the real IP makes `local_addr()` a routable address.
    let socket = bind_session_socket(ice.loopback_only)?;
    let bound = socket.local_addr().ok()?;
    tracing::info!("bound UDP socket at {bound} (local ICE base)");

    // Host candidate = exactly the bound address. It is always a concrete IP
    // (never the wildcard), so it is the address str0m sees packets arrive on.
    let host = vec![bound];
    for addr in &host {
        if let Ok(cand) = str0m::Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(cand);
        }
    }

    // STUN discovery for the public address (done while the socket is still
    // blocking, with a short read timeout), then switch to non-blocking for the
    // transport loop.
    if ice.loopback_only {
        let _ = socket.set_nonblocking(true);
        return Some((socket, None));
    }
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

/// Bind the session UDP socket to a concrete local IP — never the wildcard.
///
/// str0m matches a received packet to a local candidate by the address we report
/// as its destination (`socket.local_addr()`), so a `0.0.0.0` bind leaves
/// connectivity checks unmatched and ICE stuck at "Checking". Preference order:
/// the source IP of the default route; the first non-loopback IPv4 interface
/// (offline LAN, no gateway); loopback (a machine with no network at all, where
/// only same-machine sessions can work anyway).
fn bind_session_socket(loopback_only: bool) -> Option<std::net::UdpSocket> {
    let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
    let candidates: Vec<std::net::IpAddr> = if loopback_only {
        vec![loopback]
    } else {
        primary_local_ip()
            .into_iter()
            .chain(first_interface_ipv4())
            .chain(std::iter::once(loopback))
            .collect()
    };
    candidates
        .into_iter()
        .find_map(|ip| std::net::UdpSocket::bind(std::net::SocketAddr::new(ip, 0)).ok())
}

/// The first non-loopback, non-link-local IPv4 interface address.
fn first_interface_ipv4() -> Option<std::net::IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .map(|i| i.ip())
        .find(|ip| match ip {
            std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
            std::net::IpAddr::V6(_) => false,
        })
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

/// One usable TURN server: `host:port` + long-term credentials.
pub(super) struct TurnServer {
    hostport: String,
    username: String,
    credential: String,
}

/// ICE servers resolved from config (contract §5 `iceServers`) + public
/// fallbacks.
pub(super) struct IceConfig {
    /// Bind to 127.0.0.1 and skip STUN/TURN (same-machine sessions: the
    /// loopback test harness and diagnostics).
    pub(super) loopback_only: bool,
    stun_urls: Vec<String>,
    turn_servers: Vec<TurnServer>,
}

/// Parse ICE servers from (a) the config's `iceServers` (contract §5) and (b) the
/// TURN relay credentials the signaling server minted (Cloudflare TURN, cached on
/// the engine). STUN gets public fallbacks appended; TURN entries carry
/// `username`/`credential`. No dead free-relay default — cross-network relay comes
/// from the account's own Cloudflare TURN key via the server.
pub(super) fn ice_config_from(engine: &Engine) -> IceConfig {
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
        loopback_only: engine.ice_policy() == crate::IcePolicy::LoopbackOnly,
        stun_urls,
        turn_servers,
    }
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
