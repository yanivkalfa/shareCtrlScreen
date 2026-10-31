<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# protocol

## summary
The protocol crate (native/crates/protocol, Cargo.toml plus src/{lib,config,control,signaling,transfer,input}.rs) is pure data-model and framing logic. It has no cfg(windows), no unsafe, no threads, no channels, no statics, no COM, and no Win32 calls. Its only external dependencies are serde, serde_json, thiserror, sha2, hex and uuid (Cargo.toml [dependencies]). It is the wire contract shared by the signaling server, the engine and the input crate. The only OS-touching code is std::fs in Config::load/persist (config.rs:197-227), uuid::Uuid::new_v4 for the machine identity (config.rs:100), and Windows naming knowledge in transfer.rs (reserved device names, lines 109-117). Windows-specific meaning is carried by names and comments only: the start_with_windows field (config.rs:84, 116, doc at 82-83 refers to engine::autostart per-user Run key), the saved_logins DPAPI comment (config.rs:88-91), and the drop_at comment (transfer.rs:34-36, the receiver focuses the window under the point). The crate itself compiles and tests on any host. Everything in it is OS-agnostic in code; porting work for this crate is almost entirely renames, a config-field alias decision, and hardening two filesystem paths. lib.rs:9-10 states the crate is pure logic with no Windows/COM dependency. input.rs also exists in src/ and is declared at lib.rs:14, but it was outside this unit and was not read beyond its module header and Button attributes.

## public_api
- lib.rs:12-16 pub mod config, control, input, signaling, transfer
- lib.rs:18 re-exports Config, IceServer, Mode, Permission (config)
- lib.rs:19 re-exports ControlMsg
- lib.rs:20 re-exports Button, InputMsg (input, not read in depth)
- lib.rs:21 re-exports IceCandidate, IceServers, SignalData, SignalMsg (signaling). Caps, PING_LITERAL and PONG_LITERAL are NOT re-exported
- lib.rs:22 re-exports BulkFrame, FileMeta. CHUNK_BYTES, MAX_FILE_BYTES and sanitize_file_name are NOT re-exported
- lib.rs:25 pub const PROTOCOL_VERSION: u32 = 1 (the v field of register)
- lib.rs:29 pub fn is_valid_uuid_v4(&str) -> bool, a hand-rolled lowercase-only v4 check (8-4-4-4-12, version nibble '4' at index 14, variant [89ab] at index 19)
- config.rs:14-21 pub enum Mode {Approve (default), Password}, serde lowercase
- config.rs:25-41 pub enum Permission {View (default), Control} with as_str()
- config.rs:44-51 pub struct IceServer {urls: String, username: Option<String>, credential: Option<String>}
- config.rs:58-93 pub struct Config (serde camelCase, struct-level default) with 18 pub fields: uuid, server_url, mode, password_hash, password_permission, ice_servers, share_audio, share_display_id, recent_ids, capture_shortcuts, clipboard_sync, paste_dropped_files, start_with_windows, auto_login, saved_logins
- config.rs:125 Config::hash(&str)->String (SHA-256 hex)
- config.rs:132 verify_password(&self,&str)->bool (constant-time)
- config.rs:144 verify_proof(&self, nonce:&str, proof:&str)->bool
- config.rs:161 set_password(&mut self, Option<&str>)
- config.rs:166 add_recent(&mut self,&str) (trim, reject empty or >64 chars, dedupe, cap 10)
- config.rs:178 normalize(&mut self)
- config.rs:187 effective_mode(&self)->Mode
- config.rs:197 Config::load(&Path)->Self (never fails)
- config.rs:220 persist(&self,&Path)->io::Result<()>
- control.rs:10 pub enum ControlMsg: Perm{value}, Bye, KeyframeRequest, Cursor{x,y,shape,visible}, ViewSize{width,height}, Ping{seq}, Pong{seq}, Clipboard{text}
- signaling.rs:18 pub enum SignalMsg (13 variants, detailed below)
- signaling.rs:115-125 pub struct IceServers {urls: Vec<String>, username, credential}
- signaling.rs:129-131 pub struct Caps {decode: Vec<String>} (not re-exported)
- signaling.rs:138-142 pub enum SignalData {Offer{sdp}, Answer{sdp}, Ice{candidate}}
- signaling.rs:147-153 pub struct IceCandidate {candidate, sdp_mid, sdp_mline_index}
- signaling.rs:157-158 pub const PING_LITERAL and PONG_LITERAL
- transfer.rs:22 pub const CHUNK_BYTES: usize = 16*1024
- transfer.rs:25 pub const MAX_FILE_BYTES: u64 = 2 GiB (declared here, enforced elsewhere)
- transfer.rs:29-39 pub struct FileMeta {name, size: u64, drop_at: Option<(f64,f64)>}
- transfer.rs:43-48 pub enum BulkFrame {Begin(FileMeta), Chunk(Vec<u8>), End, Abort(String)}
- transfer.rs:52 BulkFrame::encode(&self)->Vec<u8>
- transfer.rs:74 BulkFrame::decode(&[u8])->Option<BulkFrame>
- transfer.rs:97 pub fn sanitize_file_name(&str)->Option<String>

## platform_deps
- **std::fs::read_to_string, std::fs::write, std::fs::rename, std::fs::create_dir_all (config.rs:198, 222-226). Rename-over-existing semantics: POSIX rename is atomic replace. Rust std on Windows uses MoveFileExW with REPLACE_EXISTING, which can fail with a sharing violation if the destination is open in another process (for example an antivirus or indexer handle).** @ config.rs:197-227 (Config::load and Config::persist)
  - purpose: Persist the user config atomically (tmp file then rename) and load it at startup.
  - portable: Behavioural contract: persist must be a crash-safe replace of the destination. On macOS and Linux, rename(2) gives this, but only if the tmp file is in the same directory (it is, via with_extension). No fsync is called before rename, so a power loss can leave a zero-length file (possible on all three OSes). A port should add fsync of the file and parent dir if durability matters, and must keep the retry or error behaviour for a locked destination on Windows. The config directory itself must come from the caller (per-user data dir: ~/Library/Application Support on macOS, $XDG_CONFIG_HOME or ~/.config on Linux, %APPDATA% on Windows). The crate takes a &Path and does not resolve it.
- **uuid::Uuid::new_v4() uses the OS CSPRNG via the uuid crate (config.rs:100). Also used for the default Config.uuid, which is the machine's address.** @ config.rs:100 (impl Default for Config)
  - purpose: Generate the once-only machine identity.
  - portable: Pure library call; works on all targets with no per-OS code. The contract only requires v4 randomness, not a particular source.
- **Windows-only semantics encoded in a field NAME: start_with_windows (config.rs:82-84, default false at config.rs:116). The doc comment says the per-user Run key (engine::autostart), logon not boot. The field is serialized as the JSON key startWithWindows.** @ config.rs:82-84, config.rs:116
  - purpose: Persisted user preference: launch the app at user logon.
  - portable: The protocol does not implement autostart. The field is just a bool. The port must choose: keep the JSON key for compatibility, or add a serde alias and a neutral name such as start_at_login (serde(alias = "startWithWindows")). Autostart is LaunchAgent on macOS, XDG autostart .desktop on Linux, and Run key on Windows. This is an engine concern, not a protocol concern.
- **saved_logins is a BTreeMap<String,String> whose values are opaque. The doc comment (config.rs:88-91) says they are encrypted at rest with DPAPI, scoped to the Windows account. The protocol stores whatever string the engine writes.** @ config.rs:92 (field), config.rs:88-91 (comment)
  - purpose: Persisted per-host passwords for auto-login (viewer side).
  - portable: Protocol is agnostic. The engine must replace DPAPI with a per-user secret store and keep the blob format or migrate it. A DPAPI blob in an existing config is not decryptable on other OSes, so migration needs the old value to be dropped. The field stores ciphertext as a String, so a non-UTF-8 blob would need base64 first, and that is already the case on Windows if the engine encodes the blob that way.
- **Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9) are rejected unconditionally, on every OS (transfer.rs:109-117). Name is prefixed with '_'. Separators '/', '\\' and ':' are all treated as path separators regardless of host (transfer.rs:100).** @ transfer.rs:97-120 (sanitize_file_name)
  - purpose: Stop a hostile peer's filename from escaping the receive directory or hitting a Windows device.
  - portable: Code is OS-agnostic and is the right behaviour on every OS. On macOS and Linux the names are harmless, so this only adds a conservative rename. A macOS/Linux port must not special-case this away: the sender can be Windows, so the rules must stay universal. Note the gap list in the gotchas.

## portable_parts
- lib.rs:29-61 is_valid_uuid_v4: pure, no alloc
- lib.rs:25 PROTOCOL_VERSION
- config.rs:14-41 Mode and Permission enums and Permission::as_str
- config.rs:44-51 IceServer struct
- config.rs:58-121 Config struct, Default impl, and the defaults table (server_url wss://sharectrl-signal.netameta.workers.dev/ws at config.rs:101; default ICE stun:stun.l.google.com:19302 at config.rs:105-109; share_audio true, clipboard_sync true, paste_dropped_files true, capture_shortcuts false, start_with_windows false, auto_login false)
- config.rs:125-163 hash, verify_password, verify_proof, set_password (pure, except uuid in default)
- config.rs:166-193 add_recent, normalize, effective_mode (pure)
- config.rs:230-248 ct_eq_hex and hex_decode (constant-time compare)
- control.rs:7-62 ControlMsg with its custom default_visible and is_true helpers
- signaling.rs:18-113 SignalMsg (pure serde)
- signaling.rs:115-158 IceServers, Caps, SignalData, IceCandidate, PING_LITERAL, PONG_LITERAL
- transfer.rs:22-120 CHUNK_BYTES, MAX_FILE_BYTES, FileMeta, BulkFrame encode and decode, sanitize_file_name (pure, no alloc beyond String)

## abstraction_points
- No trait is needed in this crate. The only seams are for the caller: Config::load(path: &Path) and Config::persist(path: &Path) take a path, so the caller owns config-directory resolution and the platform decision.
- A port could add a CfgDir or a storage trait around Config::load/persist to test atomic replace and the Windows rename-lock path without touching the real filesystem. Not required by this crate.
- Secret storage seam: saved_logins values are opaque String. The engine should own encrypt/decrypt via a per-OS backend (DPAPI, Keychain, libsecret or equivalent). The protocol must stay agnostic (no trait).
- Autostart seam: start_with_windows is only a bool here. The engine should own the platform implementation (Run key, LaunchAgent, XDG autostart).
- Clipboard, cursor and input types in control.rs and input.rs carry no OS types. Keyboard and mouse code in the engine or input crate maps them to native APIs (SendInput on Windows, CGEvent on macOS, uinput/XTest/Wayland portal on Linux). The protocol needs no abstraction for that.

## cross_module_contracts
- Consumers (grep of 'protocol' imports): engine/src/handshake.rs, engine/src/lib.rs, engine/src/pipeline.rs, engine/src/transfer.rs, input/src/lib.rs, signaling/src/lib.rs. Everything is plain owned data (String, Vec, Option, u32, f64, bool). There are no references and no lifetimes, so it is Send and Sync for free.
- Signaling server contract (signaling.rs:1-7): the server relays text frames, one JSON object per frame, replacing `to` with `from`, and treats `signal.data` as opaque. The Cloudflare Worker is out of this repo's native tree and was not read. Adding fields to SignalMsg must be additive, because the server must relay them unchanged.
- Ping/pong at the signaling level (signaling.rs:111-112) is a raw-string keepalive. The server matches the literal {"type":"ping"} (signaling.rs:155-158), so any change to Serialize output for the unit variants breaks keepalive. Data-channel ping/pong is separate (control.rs:42-50, with seq u32).
- Data-channel control (control.rs): reliable channel. Perm is sent once on channel open and again on each change (control.rs:11-12). Cursor is sent out of band (control.rs:23-25). ViewSize is viewer to host (control.rs:35-39). KeyframeRequest is viewer to host and drives encoder IDR or LTR recovery (control.rs:18-20). Clipboard is bidirectional text (control.rs:51-52).
- Input (input.rs header, not read in full): mm rides the unreliable channel. All other input rides the reliable channel so key-up and mouse-up cannot be lost (input.rs:4-6, referencing contract §6 channel split).
- Bulk channel (transfer.rs:1-16): one SCTP message per frame, so no length prefix. The first byte is the kind. Transfers are one at a time per session. Begin implicitly ends any previous transfer. Frames carry no transfer id. The channel is ordered and reliable. Receiver treats all content as hostile.
- Threading: this crate spawns no threads, holds no locks and has no channels. Every function is synchronous and reentrant. Config::load and persist do blocking filesystem I/O, so the engine must call them off any latency-critical thread (the crate states no rule for this).

## gotchas
- config.rs:197-217 Config::load: on ANY read error (including permission denied or a Windows sharing violation, not just NotFound) the code writes defaults over the file via persist (config.rs:211-214). That regenerates the machine uuid (config.rs:100), so a transient read failure silently changes the machine's identity. A port should distinguish NotFound from other errors and not overwrite on transient errors.
- config.rs:205 on corrupt JSON the file is renamed to path.with_extension("bad") and the rename error is ignored. An existing .bad file is replaced on Unix; on Windows the rename may fail, and then the corrupt file stays and is overwritten by persist. Also, one unknown enum value (for example a future mode string) makes the whole file count as corrupt and resets everything, because serde is strict for Mode and Permission (config.rs:14-32) while the struct itself tolerates unknown keys (no deny_unknown_fields).
- config.rs:58-59 struct-level #[serde(default)] calls Config::default() for missing fields, and Default generates a NEW random uuid each call (config.rs:100). So a config file lacking the uuid key gets a fresh identity on every load of that file. Only the file's own uuid survives.
- config.rs:178-183 normalize() comments say it 'clamps enums' and enforces the password fail-safe, but the code does neither. Enum clamping does not happen (serde errors instead, see above). The fail-safe lives in effective_mode (config.rs:187-193). normalize only lowercases the uuid and caps recent_ids. It does not validate the uuid with is_valid_uuid_v4 (lib.rs:29), so a non-v4 uuid is accepted from disk.
- config.rs:228-227 persist writes via with_extension("tmp") and does not fsync. Crash between write and rename can leave a 0-byte config.json. On Windows, rename can also fail with a sharing violation (see platform_deps).
- config.rs:124-158: the password hash is SHA-256 hex, and the proof is SHA256(hex_text_of_hash + ':' + nonce), hashing the hex STRING, not the raw bytes (config.rs:152-156, test at 261-265). Any reimplementation in another language must hash the ASCII hex text. verify_proof rejects proof length 0 or >128 (config.rs:149). The nonce length is not checked, and the proof hex is decoded case-insensitively by the hex crate, so an uppercase proof is accepted even though uuid validation is lowercase-only.
- config.rs:232-244 ct_eq_hex: constant time only after the length check. Lengths are public anyway. Both sides must be valid hex of equal, non-zero length.
- config.rs:166-174 add_recent trims and rejects empty or >64 chars. normalize (config.rs:181) only drops entries longer than 64 and does not trim. A recents entry with leading or trailing whitespace from disk survives normalize.
- config.rs:88-91 says saved_logins are DPAPI-encrypted and written only after a password is accepted. The protocol does not enforce either rule. It is an engine invariant. Clearing or migrating across OSes must treat the existing blobs as opaque and undecryptable elsewhere.
- signaling.rs:25-28 RegisterError.reason is an untyped String with a comment listing values. ConnectResponse.permission (signaling.rs:63-64), reason (signaling.rs:66-67) and codec (signaling.rs:71-72) are stringly typed, and no validation happens here. Compare with ControlMsg Perm, which uses the enum (control.rs:14). The receiver must validate. An unknown codec string must fall back to H264 only if the engine decides so: the comment at signaling.rs:68-70 says absent means H264, which is not the same as unknown.
- signaling.rs:111-112 Ping/Pong unit variants serialize to exactly {"type":"ping"} and {"type":"pong"}. Test signaling.rs:165-171 enforces this. Do not add fields to these variants.
- signaling.rs:93-97 RelayError has no `from`, and its `to` is optional. signaling.rs:20-23 Register has no to/from and no skip attributes, so it always serializes uuid and v. These asymmetries are part of the byte-compatible contract.
- signaling.rs:33-43 ConnectRequest.password has #[serde(default)] but NO skip_serializing_if, so the first attempt serializes an explicit null. The test at signaling.rs:186-187 depends on this. The other optional routing fields do skip when None.
- signaling.rs:117-125 IceServers.urls is Vec<String>, but config.rs:46 IceServer.urls is a single String. The config value is passed to the transport verbatim, so a conversion step is needed on the engine side. These are two different shapes for the same concept.
- control.rs:30-33 Cursor.visible is default true and is omitted when true (skip_serializing_if is_true). A missing visible key deserializes as true. Cursor x and y are f64 and are NOT clamped to [0,1] here, despite the doc comment. ViewSize width and height (control.rs:40-41) are u32 and are not checked for zero or for the screen size, despite the doc comment's 'never upscaling past its own screen'. Enforcement is the engine's job.
- control.rs:51-54 Clipboard text is 'size-capped by the sender'. The receiver has no cap. serde_json will parse an arbitrarily large string, so a hostile peer can force a large allocation unless the transport caps message size. This is an unenforced invariant in this crate.
- transfer.rs:25 MAX_FILE_BYTES = 2 GiB is declared here, but no check happens in this crate. FileMeta.size (transfer.rs:33) is u64 and is taken verbatim from the peer. The enforcement site is in the engine.
- transfer.rs:28-38 FileMeta.drop_at is Option<(f64,f64)>, serialized as a JSON array [x, y]. The range [0,1] is not checked. NaN and infinity are impossible in JSON, but huge values are accepted.
- transfer.rs:52-70 BulkFrame::encode copies every payload into a new Vec (Chunk copies the 16 KiB buffer). Begin uses unwrap_or_default(), so a serialize failure would emit a bare 0 byte that decodes to None. transfer.rs:80 decode copies with to_vec(). Net effect is two full copies per chunk per side. A zero-copy port (for example one that writes directly into a send buffer) must not rely on these methods.
- transfer.rs:82 Abort decodes with from_utf8_lossy, so invalid UTF-8 never fails. Empty Chunk payloads (kind 1, zero bytes) are accepted by decode (transfer.rs:80).
- transfer.rs:100 sanitize_file_name splits on '/', '\\' and ':' using rsplit, so the result is always the last component, on any host. The filter at transfer.rs:103 strips control characters and < > " | ? *. Then transfer.rs:105 trims whitespace and dots from both ends, which also removes leading dots (for example '.bashrc' becomes 'bashrc'). Result '' or '.' or '..' gives None (transfer.rs:106-108).
- transfer.rs:119 the length bound is 180 CHARS (chars().take(180)), not bytes. On Linux a 180-codepoint name of 4-byte UTF-8 characters is 720 bytes, above the 255-byte NAME_MAX common on ext4, and file creation will fail with ENAMETOOLONG. On Windows the UTF-16 length also differs from the char count. The bound does not guarantee creatability on any OS, so the engine must enforce a byte bound (around 200 bytes, leaving room for a dedupe suffix) on each target.
- transfer.rs:110-117 RESERVED covers CON, PRN, AUX, NUL, COM1-9 and LPT1-9. It does not cover CONIN$, CONOUT$, the superscript-digit variants COM¹ and so on, or COM0/LPT0. The check uses only the stem before the first '.' (transfer.rs:114), so 'con.tar.gz' is caught. A stem with a trailing space ('CON .txt') is handled only because the cleaned name is trimmed first.
- transfer.rs:105-106 the reserved-name check runs AFTER the length trim but before the length bound. A name of 180 chars with a reserved stem is prefixed with '_' and can exceed 180 chars (that is, not bounded). This matters only for the byte-length issue above.
- signaling.rs:198-205 the test signal_data_roundtrip calls matches! without assert!, so it asserts nothing about the variant. Test coverage here is weaker than it appears.
- lib.rs:29-61 is_valid_uuid_v4 accepts only lowercase. The config uuid is lowercased (config.rs:179) before any check, but the check itself is never run on loaded data, so the lowercase rule is not enforced where the server expects it.

## tests
- lib.rs:63-78 uuid_validation_matches_contract: accepts a lowercase v4, rejects uppercase, wrong version nibble, wrong variant nibble, and 'not-a-uuid'. Portable (pure).
- config.rs:254-270 challenge_response_roundtrip: computes the viewer proof and checks verify_proof accepts it, rejects a zero proof and a wrong nonce. Portable (pure). Good for cross-language compatibility vectors.
- config.rs:272-281 password_mode_without_hash_is_fail_safe_approve: portable.
- config.rs:283-292 recents_dedupe_and_cap: portable.
- control.rs:68-82 perm_wire_shape and bye_wire_shape: exact JSON for Perm (contains "t":"perm" and "value":"control") and Bye ({"t":"bye"}). Portable.
- signaling.rs:164-171 ping_literal_matches_contract: exact PING/PONG bytes. Portable and critical for the server.
- signaling.rs:173-188 connect_request_omits_null_routing_fields: checks to present, from absent, and password explicitly null. Portable.
- signaling.rs:190-206 signal_data_roundtrip: incomplete, see gotchas. Portable.
- transfer.rs:126-141 frames_roundtrip: encode then decode for all four frame kinds. Portable.
- transfer.rs:143-148 decode_rejects_garbage: empty input, unknown kind 9, truncated JSON. Portable.
- transfer.rs:150-166 sanitize_strips_path_traversal: Windows-style and Unix-style traversal and drive-letter names. Portable.
- transfer.rs:168-174 sanitize_rejects_empty_and_dots: portable.
- transfer.rs:176-180 sanitize_defuses_reserved_device_names: CON to _CON, nul.txt to _nul.txt. Portable.
- transfer.rs:182-188 sanitize_drops_control_and_illegal_chars: portable.
- No test exists for: config load corruption path (.bad rename), persist atomicity, the uuid-regeneration-on-missing-key behaviour, byte-length of sanitized names, Cursor/ViewSize range handling, or MAX_FILE_BYTES. Adding these is a cheap, portable parity task for a port.
