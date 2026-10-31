<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# turn+transport+signaling

## summary
Unit "turn+transport+signaling" covers five files: engine/src/turn.rs (557 lines), transport/src/lib.rs (344), transport/src/packet.rs (345), transport/src/fec.rs (101), signaling/src/lib.rs (220). All five were read completely. None contains a `#[cfg(windows)]`, a `windows`/`std::os` import, or any Win32 call. The only OS-facing surfaces are std::net::UdpSocket (turn.rs only), std::time::Instant (monotonic clock, several files), the tokio runtime (signaling), rand::thread_rng (turn.rs), and rustls with the aws-lc-rs provider (signaling). Everything else is pure computation, so the Windows-specific risk in this unit sits in its callers (the engine's socket loop and gathering code, which are out of scope here) and in build-time dependencies.

turn.rs is a minimal RFC 5766 TURN client over the session's own UdpSocket. allocate() (turn.rs:78-142) is blocking. It runs Allocate, expects a 401 carrying REALM and NONCE, then sends an authenticated Allocate and reads XOR-RELAYED-ADDRESS. Permissions are installed per peer IP (ensure_permission, :182-199, refreshed after PERMISSION_REFRESH=240s, :57). tick() refreshes at lifetime/2 (:202-217). handle_server_packet() (:222-254) returns Data Indications as (peer, payload) and consumes control responses. Auth is long-term credentials: key = MD5(user:realm:pass) (:258-266) and MESSAGE-INTEGRITY = HMAC-SHA1 (:375-385). Only Send/Data indications are used; there is no channel binding and no TCP/TLS transport.

transport/lib.rs wraps str0m's sans-IO Rtc (:118-131). Its module doc (:1-21) states the design: no threads, no timers, no jitter buffer; the caller owns the loop. Video is an RTP media track (send_video :191-223, which packetizes via str0m writer.write, with a 90 kHz clock derived from std Instant media_start). Control, cursor and bulk are data channels (channel_configs :56-85). Inbound events are mapped from str0m Event in on_event (:279-343). Only Disconnected maps from IceConnectionStateChange (:285-292). Viewer-side channel ids and video mid are learned from ChannelOpen (:320-329) and MediaAdded (:293-301).

packet.rs is app-level fragmentation of encoded access units into datagrams of at most 1100 bytes of payload plus a 16-byte header (:22-34, :41-57). Delta frames (recovery_count=0) use natural chunk sizes. Keyframes get ceil(n/4) (min 1) Reed-Solomon recovery shards, and every shard is padded to fec::padded_len(1100)=1152 bytes (:61-122). Reassembler (:146-227) keys partials by frame_id in a HashMap. It completes a frame when all originals arrive, or, for keyframes, when total shards >= original_count. assemble() (:235-288) reconstructs via fec::reconstruct and truncates to total_len.

fec.rs is a thin wrapper over reed-solomon-simd (:11-58). It pads shards to 64-byte multiples and is otherwise stateless.

signaling/lib.rs is an async WebSocket client (tokio + tokio-tungstenite) with a reconnecting loop (:85-111) and a per-connection loop (:115-198). It sends register first (:126-131). It runs a 25 s ping, 10 s pong timeout, and closes after 2 consecutive misses (:20-21, :133-196). It backs off through [1,2,4,8,16,30] s, with a fixed 30 s delay on duplicate registration (:22-24, :102-108). Inbound text is parsed as SignalMsg (:200-220). The engine interacts through two unbounded mpsc channels (:63, :71-72).

## public_api
- engine/src/turn.rs:60 pub struct TurnAllocation { pub server: SocketAddr, pub relayed: SocketAddr, ...private }: one allocation bound to the session socket
- engine/src/turn.rs:61 pub server: SocketAddr (TURN server address)
- engine/src/turn.rs:62 pub relayed: SocketAddr (XOR-RELAYED-ADDRESS to add as an ICE relayed candidate)
- engine/src/turn.rs:78 pub fn TurnAllocation::allocate(socket:&UdpSocket, server:SocketAddr, username:&str, password:&str) -> Option<TurnAllocation>: blocking; caller must have set a short read timeout
- engine/src/turn.rs:170 pub fn send_via_relay(&mut self, socket:&UdpSocket, peer:SocketAddr, data:&[u8]): Send Indication after ensure_permission; errors ignored
- engine/src/turn.rs:202 pub fn tick(&mut self, socket:&UdpSocket): sends Refresh when elapsed >= lifetime/2; fire-and-forget
- engine/src/turn.rs:222 pub fn handle_server_packet(&mut self, buf:&[u8]) -> Option<(SocketAddr, Vec<u8>)>: Data Indication unwrap; handles 438 stale nonce internally
- engine/src/turn.rs:57 const PERMISSION_REFRESH (private, 240 s)
- transport/src/lib.rs:23-24 pub mod fec; pub mod packet;
- transport/src/lib.rs:31-33 re-exports: pub use std::time::Instant; pub use str0m::net::{Protocol, Receive, Transmit}; pub use str0m::{Candidate, IceConnectionState}
- transport/src/lib.rs:36 pub enum Error { Rtc(RtcError), ChannelClosed(&'static str) }
- transport/src/lib.rs:47 pub struct Channels { pub ctl, pub cursor, pub bulk: ChannelId }
- transport/src/lib.rs:56 pub fn channel_configs() -> [ChannelConfig; 3] (labels ctl, cursor, bulk)
- transport/src/lib.rs:89 pub enum Inbound { Connected, Disconnected, Video{data,keyframe,contiguous}, KeyframeRequest, BweEstimate(u32), Ctl(Vec<u8>), Cursor(Vec<u8>), Bulk(Vec<u8>), ChannelOpen(ChannelId,String) }
- transport/src/lib.rs:117 pub struct Transport (owns Rtc; no Mutex; no threads)
- transport/src/lib.rs:136 pub fn new(rtc: Rtc) -> Self
- transport/src/lib.rs:151 pub fn set_channels(&mut self, Channels)
- transport/src/lib.rs:158 pub fn set_video_mid(&mut self, Mid)
- transport/src/lib.rs:162 pub fn rtc_mut(&mut self) -> &mut Rtc
- transport/src/lib.rs:167 pub fn poll_output(&mut self) -> Result<Output, Error>
- transport/src/lib.rs:172 pub fn handle_input(&mut self, Input<'_>) -> Result<(), Error>
- transport/src/lib.rs:180 pub fn set_desired_bitrate(&mut self, bps: u32)
- transport/src/lib.rs:191 pub fn send_video(&mut self, au:&[u8]) -> Result<(), Error> (Annex-B AU; silent no-op until negotiated)
- transport/src/lib.rs:228 pub fn request_keyframe(&mut self) (RTCP PLI)
- transport/src/lib.rs:237 pub fn send_ctl(&mut self, bytes:&[u8]) -> Result<(), Error>
- transport/src/lib.rs:246 pub fn send_cursor(&mut self, bytes:&[u8]) -> Result<(), Error>
- transport/src/lib.rs:255 pub fn send_bulk(&mut self, bytes:&[u8]) -> Result<(), Error>
- transport/src/lib.rs:266 pub fn bulk_buffered(&mut self) -> usize (backpressure for file sender)
- transport/src/lib.rs:279 pub fn on_event(&mut self, Event) -> Option<Inbound>
- transport/src/packet.rs:22 pub const MAX_PAYLOAD: usize = 1100
- transport/src/packet.rs:38 pub struct Fragment(pub Vec<u8>) (one wire datagram)
- transport/src/packet.rs:61 pub fn fragment(frame_id:u32, keyframe:bool, au:&[u8]) -> Vec<Fragment>
- transport/src/packet.rs:126 pub struct ReassembledFrame { pub frame_id: u32, pub keyframe: bool, pub data: Vec<u8> }
- transport/src/packet.rs:146 pub struct Reassembler (Default)
- transport/src/packet.rs:153 pub fn new() -> Self
- transport/src/packet.rs:159 pub fn push(&mut self, frag:&[u8]) -> Option<ReassembledFrame>
- transport/src/fec.rs:14 pub enum FecError { Rs(String), Unsupported }
- transport/src/fec.rs:22 pub fn padded_len(len:usize) -> usize (round up to 64)
- transport/src/fec.rs:28 pub fn encode_recovery(original:&[Vec<u8>], recovery_count:usize) -> Result<Vec<Vec<u8>>, FecError>
- transport/src/fec.rs:44 pub fn reconstruct(original_count, recovery_count, original:&[(usize,Vec<u8>)], recovery:&[(usize,Vec<u8>)]) -> Result<Vec<(usize,Vec<u8>)>, FecError>
- signaling/src/lib.rs:27 pub enum Error { Ws(tungstenite::Error), ChannelClosed }
- signaling/src/lib.rs:36 pub enum Event { Registered, RegisterError(String), Message(SignalMsg), Disconnected }
- signaling/src/lib.rs:62 pub struct SignalingClient { outbound: UnboundedSender<SignalMsg> } (not Clone)
- signaling/src/lib.rs:69 pub fn SignalingClient::spawn(server_url:String, uuid:String) -> (Self, UnboundedReceiver<Event>): calls ensure_crypto and tokio::spawn
- signaling/src/lib.rs:80 pub fn send(&self, msg: SignalMsg) -> Result<(), Error>

## platform_deps
- **std::net::UdpSocket blocking send_to / recv_from with a read timeout set by the caller (not set in this file)** @ engine/src/turn.rs:22,79,88,122-124,170-176,182-197,202-215,278-286
  - purpose: All TURN control and relay I/O runs on the same UDP socket as ICE/str0m, so the relayed 5-tuple and host candidate share one socket.
  - portable: std UdpSocket is portable. Contract: allocate() requires blocking mode plus a short SO_RCVTIMEO; recv_from_server (:277-297) treats ANY recv error as a timeout and returns None, so the function is insensitive to the error kind. On Windows, recv_from on a UDP socket can return a connection-reset error after an ICMP port-unreachable (WSAECONNRESET); this file swallows it, but the engine's main socket loop (outside this unit) must not treat recv errors as fatal. macOS and Linux do not generate this error on unconnected UDP sockets.
- **rand::thread_rng() (fill_bytes) for 12-byte STUN transaction ids** @ engine/src/turn.rs:268-273
  - purpose: Unpredictable transaction ids for matching responses.
  - portable: OS-agnostic via the rand crate. Contract: the RNG must be cryptographically seeded per OS. No change needed for macOS/Linux.
- **std::time::Instant (monotonic clock) for permission, refresh and media timing** @ engine/src/turn.rs:70,163,186,203-206,243; transport/src/lib.rs:31,130,145,213-216; transport/src/lib.rs:136-146 (media_start); str0m writer.write(now,...)
  - purpose: Permission expiry (240 s), allocation refresh (lifetime/2), stale-nonce back-dating, and RTP timestamps (90 kHz derived from micros since media_start).
  - portable: Monotonic on all three OSes. Hazard: turn.rs:243 does `Instant::now() - self.lifetime` (lifetime >= 60 s). Rust's Instant subtraction panics on overflow, and whether it can underflow depends on the platform clock epoch (boot-relative on Linux/macOS/Windows differs). Use checked_sub or a stored timestamp instead; treat as an unverified portability hazard.
- **tokio runtime: tokio::spawn, tokio::time::interval/sleep/sleep_until/Instant, tokio::select!, mpsc unbounded channels** @ signaling/src/lib.rs:17,73,109,122,133-136,143-145,150-196
  - purpose: Reconnect loop task, ping cadence, pong deadline, backoff sleeps.
  - portable: tokio is cross-platform. Contract: SignalingClient::spawn must be called inside a tokio runtime context or tokio::spawn panics (:73). Timer resolution is the tokio default on every OS.
- **rustls 0.23 CryptoProvider = aws_lc_rs, installed process-wide once** @ signaling/src/lib.rs:48-59 (ensure_crypto), :57
  - purpose: TLS for wss:// connections made by tokio-tungstenite connect_async (:122).
  - portable: Code is OS-agnostic, but the aws-lc-rs backend is a native C library built at compile time, so macOS and Linux builds need a C toolchain/cmake equivalent. Root-certificate source (webpki vs native roots) is set by Cargo features not visible in this unit; verify in the crate manifest.
- **tokio-tungstenite connect_async over TCP (and TLS for wss)** @ signaling/src/lib.rs:122
  - purpose: Connection to the Cloudflare Worker signaling endpoint.
  - portable: No explicit connect timeout exists in this file; connect time is bounded only by the OS TCP SYN retry policy, which differs between Windows, macOS and Linux. A hung connect blocks the reconnect loop with no watchdog until the OS gives up.

## portable_parts
- engine/src/turn.rs:258-266 long_term_key (MD5 credential key, pure)
- engine/src/turn.rs:268-273 new_tid (rand only)
- engine/src/turn.rs:301-386 MsgBuilder (pure STUN/TURN encoder, incl. XOR-ADDRESS for v4/v6 and MESSAGE-INTEGRITY)
- engine/src/turn.rs:391-457 Attrs getters (pure; get_xor_addr v4/v6 at :425-456)
- engine/src/turn.rs:460-491 parse_message (pure; top two bits check, magic cookie check, padding-aware attribute walk)
- engine/src/turn.rs:493-557 unit tests (pure)
- transport/src/lib.rs:46-85 Channels struct and channel_configs (pure str0m config)
- transport/src/lib.rs:89-114 Inbound enum (pure)
- transport/src/lib.rs:279-343 on_event routing (pure given str0m Event)
- transport/src/packet.rs entire file (pure byte layout and state machine; header layout at :41-57, reassembly at :159-227, assemble at :235-288)
- transport/src/fec.rs entire file (pure, deterministic wrapper; only the crate's internal SIMD backend is platform-specific)
- signaling/src/lib.rs:20-24 timing constants
- signaling/src/lib.rs:36-46 Event enum
- signaling/src/lib.rs:200-220 handle_text (pure JSON routing)
- signaling/src/lib.rs:85-111 reconnect_loop backoff logic (OS-agnostic, tokio timers)

## abstraction_points
- engine/src/turn.rs:78,170,182,202,222: all functions take concrete `&UdpSocket` and `SocketAddr`; there is no trait. A portability port can keep std UdpSocket on all three OSes unchanged. Introducing a seam would require replacing the socket type, which is not needed for mac/linux.
- turn.rs uses `Instant::now()` directly (:163,:203,:206,:243) with no injected clock, so permission and refresh timing cannot be unit-tested without real waits.
- transport/src/lib.rs:118 `rtc: Rtc` is concrete; there is no trait between Transport and str0m. Tests must construct a real Rtc.
- transport/src/lib.rs:191 send_video accepts a raw Annex-B AU slice. This is the seam where a different encoder backend (per-OS hardware encoder output) plugs in, as long as it emits Annex-B H.264.
- transport/src/packet.rs:61 fragment and :159 push are free functions/methods over byte slices: the natural seam between engine and wire format. No OS types cross it.
- signaling/src/lib.rs:36 Event and :80 send are the only public surface; the internal transport (tokio-tungstenite) is hard-wired in connect_once (:122) with no trait.
- signaling/src/lib.rs:85-111 reconnect policy (BACKOFF_SECS, duplicate 30 s) is a const slice with no injection point.

## cross_module_contracts
- TURN and the session socket: TurnAllocation::allocate must receive the SAME UdpSocket that str0m uses for host candidates. The socket must be in blocking mode with a short read timeout during candidate gathering (turn.rs:76-77). After gathering, the engine's transport driver must route Transmits whose source == relayed address through send_via_relay, and must feed Data Indications from `server` into str0m via handle_server_packet (turn.rs header lines 9-12). The engine must check that the packet source equals `server`: handle_server_packet does NOT check the source address (turn.rs:222), only recv_from_server does during allocate().
- Threading for TurnAllocation: no internal threads and no Mutex. It is used from whichever thread drives the transport loop. It owns a HashMap and Strings and is therefore Send, but all mutation (send_via_relay, tick, handle_server_packet) must be serialized by the engine's driver thread.
- Transport is sans-IO: the engine owns the loop and must repeatedly call poll_output (:167) and handle_input (:172), and must call request_keyframe (:228), set_desired_bitrate (:180), and send_video (:191). Timers are NOT in this file; the engine must schedule timeouts requested by str0m Output::Timeout.
- Host/viewer asymmetry: host calls set_channels (:151) and set_video_mid (:158) after building Rtc as offerer. Viewer learns ids from Event::ChannelOpen (:320-329) and mid from Event::MediaAdded (:293-301). The engine must handle both paths; ChannelOpen also re-sets ids on the host without harm.
- Video send precondition: send_video is a silent Ok(()) no-op until the video mid is set AND the SDP answer has produced payload_params (:198-211). The engine is documented to force a keyframe on connect (:188-190) so the first delivered frame is decodable. Caller cannot tell a dropped frame from a sent frame.
- Bulk backpressure: the file sender must poll bulk_buffered() (:266) before queuing more, per the comment at :263-265, to avoid starving the video track.
- Packet layer contract with engine: caller assigns frame_id (u32, wrapping) and supplies keyframe flag per AU. Reassembler is per-stream and must not be shared without a lock; it is Default-constructible (:145-146). The engine must treat Reassembler::push output as in-order frames (ordering is by arrival, not frame_id).
- Signaling contract: the engine receives Event from an unbounded mpsc and sends SignalMsg via SignalingClient::send (unbounded). Send errors mean the reconnect task has exited (ev_tx closed; see :99-101). The protocol crate supplies SignalMsg, PING_LITERAL, PONG_LITERAL, PROTOCOL_VERSION (signaling/src/lib.rs:15-16); those definitions are NOT in this unit and were not verified here.
- Signaling and WebRTC independence: per the module doc (signaling :1-10, Event::Disconnected doc at :43-44) an active WebRTC session survives signaling disconnects; the engine must not tear down media on Event::Disconnected from signaling.

## gotchas
- signaling/src/lib.rs:91,108: `attempt` is never reset to 0 after a successful registration. Once it reaches the 30 s cap (index 5 at :106), any later disconnect starts at 30 s, not 1 s. Preserve or fix deliberately.
- signaling/src/lib.rs:98: Event::Disconnected is emitted after EVERY connect_once return, including a failed initial connect (:94-98), so the engine sees repeated Disconnected events during backoff.
- signaling/src/lib.rs:122: connect_async has no application-level timeout. Connect blocks until the OS gives up, and the ping watchdog does not start until after register (:131-136).
- signaling/src/lib.rs:63,88,118: the outbound mpsc queue persists across reconnects (out_rx is passed by &mut into each connect_once). Messages sent while disconnected are delivered after reconnect with no staleness filter, and are unbounded.
- signaling/src/lib.rs:206-215: duplicate-registration is a standing condition: `duplicate` is set on RegisterError{reason=='duplicate'} and cleared only on Registered; it changes the retry delay to fixed 30 s (:103-105).
- signaling/src/lib.rs:184-188: a raw PONG_LITERAL text is matched before JSON parsing (cheapest match first, per comment). Any other inbound traffic does NOT count as liveness; only PONG resets missed_pongs.
- signaling/src/lib.rs:171-174: two consecutive pong misses close the socket (missed_pongs is not reset by other traffic). After each miss pong_deadline is cleared (:169-170), so the next miss needs another full ping cycle (25 s + 10 s).
- signaling/src/lib.rs:53-59: ensure_crypto must run before the first wss:// connect or the tokio worker panics (comment :50-52). It is guarded by a static std::sync::Once (:54). Err from install_default is ignored by design.
- signaling/src/lib.rs:200-203: unparseable or unknown JSON text is silently ignored (forward-compat, contract §3).
- turn.rs:284-286 and :287-289: recv_from_server returns None on ANY recv error (timeout, reset, etc.) and skips datagrams from non-server sources or with the wrong transaction id, up to 5 reads total. No retransmission of Allocate, Refresh or CreatePermission exists. Lost requests are recovered only by ICE retransmissions (turn.rs:180-181).
- turn.rs:190-197: ensure_permission records the peer IP as 'fresh' BEFORE sending CreatePermission and ignores the send result (`let _ =`). A lost or failed CreatePermission is not retried for 240 s, and relayed traffic is dropped by the server meanwhile.
- turn.rs:210: tick() refresh always requests lifetime 600 regardless of the granted lifetime, and REFRESH_OK responses are consumed with no lifetime update (:230). self.lifetime stays at the value fixed in finish_alloc (:162, clamped to min 60 s).
- turn.rs:231-251: 438 (stale nonce) handling adopts the new nonce and realm, clears all permissions (:242), and back-dates last_refresh by one lifetime (:243) so the next tick() refreshes immediately. The back-dating is `Instant::now() - self.lifetime`, which is the unverified panic/underflow hazard noted in platform_deps.
- turn.rs:222-254: handle_server_packet does not verify MESSAGE-INTEGRITY on any response, and does not check the packet source (the caller must). Data Indications are unauthenticated per RFC, which is expected.
- turn.rs:145-166 and :40-47: the unauthenticated-allocate branch (ALLOCATE_OK at :98-110) stores empty realm and nonce, so later permission and refresh messages carry empty REALM/NONCE attributes with a key derived from an empty realm. Behaviour against a server that grants unauthenticated allocations is unverified.
- turn.rs:318 MsgBuilder::attr_bytes writes the attribute length as `as u16` without a bounds check, so a value longer than 65535 bytes would be silently truncated. In practice send_via_relay payloads are bounded by MAX_PAYLOAD upstream, but the function does not enforce it.
- turn.rs:76-77: allocate() requires the caller to have configured a short read timeout on the socket. The function itself never calls set_read_timeout, so a socket left blocking forever would hang gathering.
- transport/lib.rs:186-190: send_video is a no-op until negotiated, and the engine must force a keyframe on connect. Frames sent before connection are not queued.
- transport/lib.rs:127-128, 198-208: video_pt and video_freq are resolved once and cached forever. A renegotiation that changes the payload type would leave them stale; no invalidation exists.
- transport/lib.rs:239-241, 248-250, 257-259: send_ctl, send_cursor and send_bulk return Error::ChannelClosed only if the id was NEVER learned (ok_or). If the id is known but rtc.channel() returns None (channel closed), the call silently returns Ok(()) and the payload is dropped.
- transport/lib.rs:303-305: MediaData with empty data is dropped silently (returns None).
- transport/lib.rs:285-292: only IceConnectionState::Disconnected maps to Inbound::Disconnected; all other ICE states, including Connected, return None. Connection-up is signalled separately via Event::Connected (:281-283).
- transport/lib.rs:313-317: EgressBitrateEstimate is mapped only for BweKind::Twcc and BweKind::Remb; any other kind returns None. The value is clamped to u32::MAX (:318).
- transport/lib.rs:67-70: the cursor channel is ordered:false with MaxRetransmits{0}, i.e. unreliable and unordered, not merely unreliable-latest. The 'latest' property depends on the receiver discarding stale values.
- transport/lib.rs:74-77: bulk is a separate reliable ordered channel on purpose, so file transfer cannot head-of-line block ctl input. This is an invariant the engine must not regress by multiplexing onto ctl.
- transport/lib.rs:213-217: RTP timestamps come from wall time since media_start (micros * 90000 / 1e6), not from capture time. Frame pacing jitter is therefore absorbed into the timestamp, not measured.
- packet.rs:176: the superseded-frame guard is strictly `frame_id.wrapping_sub(newest_done) > u32::MAX/2`, so frame_id == newest_done is NOT ignored. A duplicate or late fragment of the just-completed frame creates a new Partial. For a 1-shard frame (original_count 1) this re-emits the same frame. For a keyframe with original_count 1, a late recovery shard alone satisfies total_have >= original_count and re-emits it.
- packet.rs:190-191: the bounds check at :172 uses the CURRENT fragment's total_shards, but the shards Vec was sized from the FIRST fragment of that frame_id (:185). A later fragment with a larger shard_index for the same frame_id indexes past the Vec and panics (index out of bounds). Only peers can produce it (DTLS-protected channel), but a malformed frame_id collision would crash the viewer. Also original_count and recovery_count are taken from the first fragment only (:180-188) with no consistency check.
- packet.rs:169: each fragment's payload is copied (to_vec) BEFORE the duplicate/superseded checks, so late or duplicate fragments still allocate.
- packet.rs:212-213: partials are pruned only when a frame completes (retain older ids). There is no age or size bound, so during a total stall partials accumulate until the next successful completion.
- packet.rs:88,99,109-117: original_count and shard_index are cast with `as u16` and total_len with `as u32` without range checks. An access unit larger than about 72 MB (65535 * 1100) would silently wrap the count fields.
- packet.rs:82-107: if fec::encode_recovery fails for a keyframe (Unsupported or Rs error), the code silently falls back to the no-FEC path and writes recovery_count=0 in the header while still setting the keyframe flag (:117). The keyframe then loses its protection with no signal to the caller.
- packet.rs:20-21, 22: MAX_PAYLOAD=1100 leaves headroom under str0m DATAGRAM_MTU_TARGET (the constant's value is not in this unit). Keyframe shards are padded to 1152 bytes, so keyframe datagrams are 1168 bytes on the wire (16-byte header + 1152), slightly above the stated 1100 payload budget. Delta datagrams are at most 1116 bytes.
- packet.rs:246-252 (assemble): when recovery_count>0 each original's real length is inferred from total_len, not from the wire, so the last original's length is derived arithmetic: total_len - (n-1)*1100. Any mismatch between total_len and the actual shard data silently truncates.
- packet.rs:198-199: a keyframe completes as soon as total_have >= original_count, which means it can complete by RECOVERY shards alone. Assembly then calls fec::reconstruct (:272) and on failure returns None via `?` (:210) after the partial was already removed (:204), so the frame is dropped and recovery is not retried.
- fec.rs:22-24 and :6-9: reed-solomon-simd requires equal-length shards whose length is a multiple of 64. Callers pad to 64 (packet.rs pads to 1152). reconstruct (:44-58) does not validate shard lengths; a peer sending a short payload produces a shard length mismatch that the crate reports as an error (surfaces as frame drop, not a panic, under the assumptions of the crate).
- turn.rs:163,162: lifetime is clamped to at least 60 s (Duration::from_secs(lifetime.max(60))), so a server returning a shorter lifetime is overridden.

## tests
- engine/src/turn.rs:497-508 xor_addr_roundtrip_v4: pure, portable (IPv4 only; no IPv6 round-trip test although get_xor_addr has a v6 branch at :443-453).
- engine/src/turn.rs:510-533 data_indication_roundtrip: pure, portable; uses finish_alloc with dummy values, no network.
- engine/src/turn.rs:535-546 integrity_appends_24_bytes_and_patches_length: pure, portable; verifies HMAC length and header length patch only, not the HMAC value.
- engine/src/turn.rs:548-556 error_code_parses: pure, portable; checks 438 parsing.
- transport/src/packet.rs:294-310 roundtrip_multi_fragment_delta_frame: pure, portable; 3000-byte delta frame, in-order delivery.
- transport/src/packet.rs:312-334 keyframe_recovers_a_lost_fragment_via_fec: pure, portable; drops original index 1 of an 8000-byte keyframe and checks FEC recovery.
- transport/src/packet.rs:336-344 incomplete_delta_frame_is_dropped_when_superseded: pure, portable; asserts partials is empty after a newer frame completes.
- transport/src/fec.rs:64-100 recovers_a_dropped_shard: pure, portable; 4 originals, 2 recovery, drops indices 1 and 3.
- transport/src/lib.rs: NO tests. The Transport, channel config and on_event logic have no unit coverage in this unit.
- signaling/src/lib.rs: NO tests. Reconnect backoff, duplicate handling, watchdog and handle_text are untested in this unit.
- Missing coverage worth noting for the port: no test for packet.rs duplicate or late fragments after completion (gotcha at packet.rs:176), no test for a shard_index that exceeds the first fragment's count (packet.rs:190), no IPv6 XOR round-trip in turn.rs, and no real-server TURN integration test (all turn tests are offline).
