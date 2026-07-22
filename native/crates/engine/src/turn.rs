//! Minimal TURN client (RFC 5766) over the session UDP socket — the relay
//! fallback that makes cross-network sessions work when STUN hole-punching
//! can't (symmetric/carrier-grade NATs). One allocation per session:
//!
//!   * `allocate()` runs during candidate gathering (blocking, same pattern as
//!     the STUN query): Allocate → 401 (realm+nonce) → authenticated Allocate →
//!     XOR-RELAYED-ADDRESS. That address is added to the `Rtc` as a relayed
//!     candidate, so it rides the normal ICE machinery.
//!   * The transport driver routes: str0m `Transmit`s whose source is the
//!     relayed address are wrapped in Send Indications to the TURN server;
//!     Data Indications from the server are unwrapped and fed to str0m as if
//!     they arrived on the relayed address.
//!   * `ensure_permission()` installs/refreshes per-peer-IP permissions (the
//!     server drops relayed traffic without them), `tick()` refreshes the
//!     allocation before its lifetime expires.
//!
//! Only UDP transport, only Send/Data indications (no channel binding — an
//! optimization, not a requirement). Auth is long-term credentials:
//! key = MD5(username ":" realm ":" password), MESSAGE-INTEGRITY = HMAC-SHA1.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

const MAGIC: u32 = 0x2112_A442;

// Message types (method | class bits already combined).
const ALLOCATE_REQ: u16 = 0x0003;
const ALLOCATE_OK: u16 = 0x0103;
const ALLOCATE_ERR: u16 = 0x0113;
const REFRESH_REQ: u16 = 0x0004;
const REFRESH_OK: u16 = 0x0104;
const REFRESH_ERR: u16 = 0x0114;
const CREATE_PERM_REQ: u16 = 0x0008;
const CREATE_PERM_OK: u16 = 0x0108;
const CREATE_PERM_ERR: u16 = 0x0118;
const SEND_INDICATION: u16 = 0x0016;
const DATA_INDICATION: u16 = 0x0017;

// Attributes.
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;

/// Re-permission interval — RFC permissions live 5 min; refresh at 4.
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);

/// One TURN server allocation bound to the session socket.
pub struct TurnAllocation {
    pub server: SocketAddr,
    pub relayed: SocketAddr,
    username: String,
    realm: String,
    nonce: Vec<u8>,
    /// MD5(username:realm:password) — the long-term credential key.
    key: [u8; 16],
    password: String,
    lifetime: Duration,
    last_refresh: Instant,
    /// Peer IPs we've (re-)sent CreatePermission for, and when.
    permissions: HashMap<IpAddr, Instant>,
}

impl TurnAllocation {
    /// Perform the blocking Allocate handshake on `socket` (must still be in
    /// blocking mode with a short read timeout — called during gathering).
    pub fn allocate(
        socket: &UdpSocket,
        server: SocketAddr,
        username: &str,
        password: &str,
    ) -> Option<TurnAllocation> {
        // 1) Unauthenticated Allocate — expect 401 carrying realm + nonce.
        let tid = new_tid();
        let mut msg = MsgBuilder::new(ALLOCATE_REQ, &tid);
        msg.attr_requested_transport_udp();
        socket.send_to(&msg.finish(), server).ok()?;

        let (mtype, attrs) = recv_from_server(socket, server, &tid)?;
        let (realm, nonce) = match mtype {
            ALLOCATE_ERR => {
                let realm = attrs.get_string(ATTR_REALM)?;
                let nonce = attrs.get_bytes(ATTR_NONCE)?;
                (realm, nonce)
            }
            // Some servers allow unauthenticated allocation (rare).
            ALLOCATE_OK => {
                let relayed = attrs.get_xor_addr(ATTR_XOR_RELAYED_ADDRESS, &tid)?;
                let lifetime = attrs.get_lifetime().unwrap_or(600);
                return Some(Self::finish_alloc(
                    server,
                    relayed,
                    username,
                    "",
                    &[],
                    password,
                    lifetime,
                ));
            }
            _ => return None,
        };

        // 2) Authenticated Allocate.
        let key = long_term_key(username, &realm, password);
        let tid2 = new_tid();
        let mut msg = MsgBuilder::new(ALLOCATE_REQ, &tid2);
        msg.attr_requested_transport_udp();
        msg.attr_str(ATTR_USERNAME, username);
        msg.attr_str(ATTR_REALM, &realm);
        msg.attr_bytes(ATTR_NONCE, &nonce);
        socket
            .send_to(&msg.finish_with_integrity(&key), server)
            .ok()?;

        let (mtype, attrs) = recv_from_server(socket, server, &tid2)?;
        if mtype != ALLOCATE_OK {
            tracing::warn!(
                "TURN allocate rejected by {server} (type {mtype:#06x}, error {:?})",
                attrs.get_error_code()
            );
            return None;
        }
        let relayed = attrs.get_xor_addr(ATTR_XOR_RELAYED_ADDRESS, &tid2)?;
        let lifetime = attrs.get_lifetime().unwrap_or(600);
        tracing::info!(
            "TURN allocation on {server}: relayed address {relayed} (lifetime {lifetime}s)"
        );
        Some(Self::finish_alloc(
            server, relayed, username, &realm, &nonce, password, lifetime,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_alloc(
        server: SocketAddr,
        relayed: SocketAddr,
        username: &str,
        realm: &str,
        nonce: &[u8],
        password: &str,
        lifetime_secs: u32,
    ) -> TurnAllocation {
        TurnAllocation {
            server,
            relayed,
            username: username.to_string(),
            realm: realm.to_string(),
            nonce: nonce.to_vec(),
            key: long_term_key(username, realm, password),
            password: password.to_string(),
            lifetime: Duration::from_secs(lifetime_secs.max(60) as u64),
            last_refresh: Instant::now(),
            permissions: HashMap::new(),
        }
    }

    /// Relay one datagram to `peer` (a Send Indication — no auth needed).
    /// Installs a permission for the peer's IP first when missing/stale.
    pub fn send_via_relay(&mut self, socket: &UdpSocket, peer: SocketAddr, data: &[u8]) {
        self.ensure_permission(socket, peer);
        let tid = new_tid();
        let mut msg = MsgBuilder::new(SEND_INDICATION, &tid);
        msg.attr_xor_addr(ATTR_XOR_PEER_ADDRESS, peer, &tid);
        msg.attr_bytes(ATTR_DATA, data);
        let _ = socket.send_to(&msg.finish(), self.server);
    }

    /// Send CreatePermission for the peer's IP if we haven't recently. Fire and
    /// forget: the response is consumed by [`Self::handle_server_packet`], and
    /// ICE retransmits cover the one-RTT window before it lands.
    fn ensure_permission(&mut self, socket: &UdpSocket, peer: SocketAddr) {
        let fresh = self
            .permissions
            .get(&peer.ip())
            .is_some_and(|t| t.elapsed() < PERMISSION_REFRESH);
        if fresh {
            return;
        }
        self.permissions.insert(peer.ip(), Instant::now());
        let tid = new_tid();
        let mut msg = MsgBuilder::new(CREATE_PERM_REQ, &tid);
        msg.attr_xor_addr(ATTR_XOR_PEER_ADDRESS, peer, &tid);
        msg.attr_str(ATTR_USERNAME, &self.username);
        msg.attr_str(ATTR_REALM, &self.realm);
        msg.attr_bytes(ATTR_NONCE, &self.nonce);
        let _ = socket.send_to(&msg.finish_with_integrity(&self.key), self.server);
        tracing::debug!("TURN: permission requested for {}", peer.ip());
    }

    /// Refresh the allocation when half its lifetime has passed.
    pub fn tick(&mut self, socket: &UdpSocket) {
        if self.last_refresh.elapsed() < self.lifetime / 2 {
            return;
        }
        self.last_refresh = Instant::now();
        let tid = new_tid();
        let mut msg = MsgBuilder::new(REFRESH_REQ, &tid);
        let mut lt = [0u8; 4];
        lt.copy_from_slice(&600u32.to_be_bytes());
        msg.attr_bytes(ATTR_LIFETIME, &lt);
        msg.attr_str(ATTR_USERNAME, &self.username);
        msg.attr_str(ATTR_REALM, &self.realm);
        msg.attr_bytes(ATTR_NONCE, &self.nonce);
        let _ = socket.send_to(&msg.finish_with_integrity(&self.key), self.server);
        tracing::debug!("TURN: allocation refresh sent");
    }

    /// Process a packet that arrived from the TURN server. Returns
    /// `Some((peer, payload))` for Data Indications (relayed traffic); consumes
    /// control responses (refresh/permission, stale-nonce updates) internally.
    pub fn handle_server_packet(&mut self, buf: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        let (mtype, tid, attrs) = parse_message(buf)?;
        match mtype {
            DATA_INDICATION => {
                let peer = attrs.get_xor_addr(ATTR_XOR_PEER_ADDRESS, &tid)?;
                let data = attrs.get_bytes(ATTR_DATA)?;
                Some((peer, data))
            }
            CREATE_PERM_OK | REFRESH_OK => None,
            CREATE_PERM_ERR | REFRESH_ERR | ALLOCATE_ERR => {
                // 438 stale nonce: adopt the new nonce; the next permission or
                // refresh retry authenticates with it. Force both soon.
                if let Some(code) = attrs.get_error_code() {
                    if code == 438 {
                        if let Some(nonce) = attrs.get_bytes(ATTR_NONCE) {
                            self.nonce = nonce;
                            if let Some(realm) = attrs.get_string(ATTR_REALM) {
                                self.key = long_term_key(&self.username, &realm, &self.password);
                                self.realm = realm;
                            }
                            self.permissions.clear();
                            self.last_refresh = Instant::now() - self.lifetime;
                            tracing::debug!("TURN: stale nonce — refreshed credentials");
                        }
                    } else {
                        tracing::warn!("TURN server error {code} (type {mtype:#06x})");
                    }
                }
                None
            }
            _ => None,
        }
    }
}

/// key = MD5(username ":" realm ":" password) — RFC 5389 long-term credentials.
fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(username.as_bytes());
    h.update(b":");
    h.update(realm.as_bytes());
    h.update(b":");
    h.update(password.as_bytes());
    h.finalize().into()
}

fn new_tid() -> [u8; 12] {
    use rand::RngCore;
    let mut tid = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tid);
    tid
}

/// Wait (bounded retries) for a response from `server` with transaction `tid`,
/// skipping unrelated packets (mirrors the STUN query pattern).
fn recv_from_server(
    socket: &UdpSocket,
    server: SocketAddr,
    tid: &[u8; 12],
) -> Option<(u16, Attrs)> {
    let mut buf = [0u8; 1500];
    for _ in 0..5 {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            return None; // timeout
        };
        if from != server {
            continue;
        }
        if let Some((mtype, rtid, attrs)) = parse_message(&buf[..n]) {
            if rtid == *tid {
                return Some((mtype, attrs));
            }
        }
    }
    None
}

// ---- STUN/TURN message building ---------------------------------------------

struct MsgBuilder {
    buf: Vec<u8>,
}

impl MsgBuilder {
    fn new(mtype: u16, tid: &[u8; 12]) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&mtype.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes()); // length patched later
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(tid);
        Self { buf }
    }

    fn attr_bytes(&mut self, typ: u16, val: &[u8]) {
        self.buf.extend_from_slice(&typ.to_be_bytes());
        self.buf
            .extend_from_slice(&(val.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(val);
        // Pad to 4.
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    fn attr_str(&mut self, typ: u16, val: &str) {
        self.attr_bytes(typ, val.as_bytes());
    }

    fn attr_requested_transport_udp(&mut self) {
        // Protocol 17 (UDP) in the top byte.
        self.attr_bytes(ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
    }

    fn attr_xor_addr(&mut self, typ: u16, addr: SocketAddr, tid: &[u8; 12]) {
        let magic = MAGIC.to_be_bytes();
        let mut val = Vec::with_capacity(20);
        val.push(0);
        let xport = addr.port() ^ 0x2112;
        match addr.ip() {
            IpAddr::V4(v4) => {
                val.push(0x01);
                val.extend_from_slice(&xport.to_be_bytes());
                for (i, o) in v4.octets().iter().enumerate() {
                    val.push(o ^ magic[i]);
                }
            }
            IpAddr::V6(v6) => {
                val.push(0x02);
                val.extend_from_slice(&xport.to_be_bytes());
                let o = v6.octets();
                for i in 0..4 {
                    val.push(o[i] ^ magic[i]);
                }
                for i in 0..12 {
                    val.push(o[4 + i] ^ tid[i]);
                }
            }
        }
        self.attr_bytes(typ, &val);
    }

    fn patch_len(&mut self, extra: usize) {
        let len = (self.buf.len() - 20 + extra) as u16;
        self.buf[2..4].copy_from_slice(&len.to_be_bytes());
    }

    fn finish(mut self) -> Vec<u8> {
        self.patch_len(0);
        self.buf
    }

    /// Append MESSAGE-INTEGRITY: HMAC-SHA1 over the message with the header
    /// length pre-adjusted to include the (not yet appended) MI attribute.
    fn finish_with_integrity(mut self, key: &[u8; 16]) -> Vec<u8> {
        self.patch_len(24); // 4-byte attr header + 20-byte HMAC
        let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(&self.buf);
        let tag = mac.finalize().into_bytes();
        self.buf
            .extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        self.buf.extend_from_slice(&20u16.to_be_bytes());
        self.buf.extend_from_slice(&tag);
        self.buf
    }
}

// ---- STUN/TURN message parsing ------------------------------------------------

/// Parsed attribute list (raw type → value bytes; duplicates keep the first).
struct Attrs {
    list: Vec<(u16, Vec<u8>)>,
}

impl Attrs {
    fn get_bytes(&self, typ: u16) -> Option<Vec<u8>> {
        self.list
            .iter()
            .find(|(t, _)| *t == typ)
            .map(|(_, v)| v.clone())
    }

    fn get_string(&self, typ: u16) -> Option<String> {
        String::from_utf8(self.get_bytes(typ)?).ok()
    }

    fn get_lifetime(&self) -> Option<u32> {
        let v = self.get_bytes(ATTR_LIFETIME)?;
        if v.len() >= 4 {
            Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
        } else {
            None
        }
    }

    fn get_error_code(&self) -> Option<u32> {
        let v = self.get_bytes(ATTR_ERROR_CODE)?;
        if v.len() >= 4 {
            Some((v[2] as u32) * 100 + (v[3] as u32))
        } else {
            None
        }
    }

    fn get_xor_addr(&self, typ: u16, tid: &[u8; 12]) -> Option<SocketAddr> {
        let val = self.get_bytes(typ)?;
        if val.len() < 8 {
            return None;
        }
        let magic = MAGIC.to_be_bytes();
        let port = u16::from_be_bytes([val[2], val[3]]) ^ 0x2112;
        match val[1] {
            0x01 if val.len() >= 8 => {
                let mut a = [val[4], val[5], val[6], val[7]];
                for i in 0..4 {
                    a[i] ^= magic[i];
                }
                Some(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(a[0], a[1], a[2], a[3])),
                    port,
                ))
            }
            0x02 if val.len() >= 20 => {
                let mut a = [0u8; 16];
                a.copy_from_slice(&val[4..20]);
                for i in 0..4 {
                    a[i] ^= magic[i];
                }
                for i in 0..12 {
                    a[4 + i] ^= tid[i];
                }
                Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), port))
            }
            _ => None,
        }
    }
}

/// Parse a STUN/TURN message: `(type, transaction id, attributes)`.
fn parse_message(buf: &[u8]) -> Option<(u16, [u8; 12], Attrs)> {
    if buf.len() < 20 {
        return None;
    }
    // Top two bits zero for all STUN messages (filters SRTP/DTLS/etc.).
    if buf[0] & 0xC0 != 0 {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC {
        return None;
    }
    let mtype = u16::from_be_bytes([buf[0], buf[1]]);
    let mlen = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (20 + mlen).min(buf.len());
    let mut tid = [0u8; 12];
    tid.copy_from_slice(&buf[8..20]);

    let mut list = Vec::new();
    let mut i = 20;
    while i + 4 <= end {
        let typ = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        let vstart = i + 4;
        let vend = vstart + len;
        if vend > end {
            break;
        }
        list.push((typ, buf[vstart..vend].to_vec()));
        i = vend + ((4 - (len % 4)) % 4);
    }
    Some((mtype, tid, Attrs { list }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_addr_roundtrip_v4() {
        let tid = new_tid();
        let addr: SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let mut b = MsgBuilder::new(SEND_INDICATION, &tid);
        b.attr_xor_addr(ATTR_XOR_PEER_ADDRESS, addr, &tid);
        let msg = b.finish();
        let (mtype, rtid, attrs) = parse_message(&msg).unwrap();
        assert_eq!(mtype, SEND_INDICATION);
        assert_eq!(rtid, tid);
        assert_eq!(attrs.get_xor_addr(ATTR_XOR_PEER_ADDRESS, &tid), Some(addr));
    }

    #[test]
    fn data_indication_roundtrip() {
        let tid = new_tid();
        let peer: SocketAddr = "198.51.100.9:4242".parse().unwrap();
        let payload = vec![1u8, 2, 3, 4, 5];
        let mut b = MsgBuilder::new(DATA_INDICATION, &tid);
        b.attr_xor_addr(ATTR_XOR_PEER_ADDRESS, peer, &tid);
        b.attr_bytes(ATTR_DATA, &payload);
        let msg = b.finish();

        // A dummy allocation to exercise handle_server_packet.
        let mut alloc = TurnAllocation::finish_alloc(
            "192.0.2.1:3478".parse().unwrap(),
            "192.0.2.1:49152".parse().unwrap(),
            "user",
            "realm",
            b"nonce",
            "pass",
            600,
        );
        let (got_peer, got_data) = alloc.handle_server_packet(&msg).unwrap();
        assert_eq!(got_peer, peer);
        assert_eq!(got_data, payload);
    }

    #[test]
    fn integrity_appends_24_bytes_and_patches_length() {
        let tid = new_tid();
        let key = long_term_key("u", "r", "p");
        let mut b = MsgBuilder::new(ALLOCATE_REQ, &tid);
        b.attr_requested_transport_udp();
        let before = 20 + 8; // header + requested-transport attr
        let msg = b.finish_with_integrity(&key);
        assert_eq!(msg.len(), before + 24);
        let mlen = u16::from_be_bytes([msg[2], msg[3]]) as usize;
        assert_eq!(20 + mlen, msg.len());
    }

    #[test]
    fn error_code_parses() {
        let tid = new_tid();
        let mut b = MsgBuilder::new(ALLOCATE_ERR, &tid);
        // class 4, number 38 => 438.
        b.attr_bytes(ATTR_ERROR_CODE, &[0, 0, 4, 38]);
        let (_, _, attrs) = parse_message(&b.finish()).unwrap();
        assert_eq!(attrs.get_error_code(), Some(438));
    }
}
