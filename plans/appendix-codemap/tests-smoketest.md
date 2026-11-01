<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# tests+smoketest

## summary
latency_smoketest.rs (89 lines) is a Windows-only example binary that measures per-stage wall time of capture, H.264 encode and decode on one machine, with no network. Its Windows body is at latency_smoketest.rs:11-84 and a non-Windows stub at :86-89 prints a notice and returns. The Windows body creates capture::Duplicator::new(0,0) (:17), codec::Encoder::new(dup.device(), EncoderConfig::default()) (:24), and codec::Decoder::new(dup.device(), Codec::H264, 1920, 1080).ok() (:31). It loops for at most 5 s or 300 samples (:34-36). Each iteration calls dup.acquire(32ms) (:38), skips pointer_only frames (:39), reinits on AccessLost and ignores the result (:41-44), and breaks on other errors (:45-48). Encode runs on frame.texture with unwrap_or_default (:53). The first encoded unit is decoded (:57-60). dup.release() is at :62, and samples are recorded only when units is non-empty (:64-66). Output at :78-83 reports averages against the section 2 budget. The file has no pub items and no tests. Its Windows dependencies are declared at engine/Cargo.toml:33-50. The test inventory is 54 #[test] functions across 17 files, all in #[cfg(test)] modules. Thirty-eight run on any OS as written. Nine are gated by Windows-only modules or crate attributes. Three more sit in the engine crate, which does not compile off Windows (see gotchas). Pure logic is trapped in Windows-gated code, including Codec::negotiate (codec/src/lib.rs:57), fit_within and parse_stun_mapped_address (pipeline.rs), and the autostart command format.

## public_api
- native/crates/engine/examples/latency_smoketest.rs:11-84 fn main (cfg(windows)); no pub items are exported.
- native/crates/engine/examples/latency_smoketest.rs:86-89 fn main (cfg(not(windows))) prints a Windows-only notice and returns.
- Used from capture: Duplicator::new(u32,u32), device(), acquire(Duration) -> Result<Option<Frame>, Error>, Frame.pointer_only, Frame.texture, reinit(), release(), Error::AccessLost (latency_smoketest.rs:17,38-62).
- Used from codec: Encoder::new(&device, EncoderConfig::default()), encode(&texture) -> Result<Vec<unit{data,keyframe}>>, Decoder::new(&device, Codec::H264, w, h), decode(&data, keyframe, i64), Codec::H264 (latency_smoketest.rs:24,31,53,59).

## platform_deps
- **DXGI Desktop Duplication via capture::Duplicator, exposing a shared ID3D11Device via dup.device()** @ native/crates/engine/examples/latency_smoketest.rs:17,24,31,38,62; implementation native/crates/capture/src/lib.rs (#![cfg(windows)] at :15)
  - purpose: Source of GPU-resident screen frames for the smoke test
  - portable: macOS needs a ScreenCaptureKit or CGDisplayStream source yielding GPU textures; Linux needs PipeWire, KMS or XShm. Contract: acquire(32ms) returns Ok(None) on no new frame, a distinct AccessLost on desktop switch or UAC needing reinit, and pointer-only frames the caller must skip. The texture must stay valid until release() and be encodable on the same device as the encoder.
- **Media Foundation H.264 hardware MFT encoder bound to the D3D11 device, and D3D11VA decoder (codec crate)** @ native/crates/engine/examples/latency_smoketest.rs:24-31,53,59; native/crates/codec/src/lib.rs:16 (#![cfg(windows)])
  - purpose: Measures encode and decode latency, the core of the go/no-go budget
  - portable: macOS: VideoToolbox (VTCompressionSession, real-time, zero-latency settings) for encode and decode. Linux: VAAPI, NVENC or V4L2 M2M with a software fallback. The API takes a GPU surface in and returns an encoded access unit. Encoder errors must surface as values; the smoketest's unwrap_or_default turns them into skipped frames.
- **Process-wide tracing subscriber** @ native/crates/engine/examples/latency_smoketest.rs:15; tracing-subscriber dev-dependency at native/crates/engine/Cargo.toml:50-51
  - purpose: Log output from capture and codec
  - portable: Platform-neutral; works unchanged on macOS and Linux.

## portable_parts
- latency_smoketest.rs:33-83 timing and aggregation (Instant stage timing, closure averages, 5 s deadline, 300-sample cap) is OS-agnostic once capture and codec are abstracted.
- codec/src/lib.rs:28-64 Codec enum, as_caps_str, from_caps_str and negotiate are pure; they are gated only by the crate-level #![cfg(windows)] at :16.
- protocol/src/transfer.rs:97-120 sanitize_file_name is pure and treats /, \ and : as separators on any host (:98-100).
- protocol/src/config.rs, control.rs, input.rs, signaling.rs, lib.rs: serde wire types and pure logic with no OS calls.
- transport/src/fec.rs and packet.rs: pure FEC and fragmentation/reassembly with no OS calls.
- engine/src/turn.rs: pure STUN/TURN message builder and parser (HMAC-SHA1 and MD5 long-term key). No OS calls in the parts under test.
- engine/src/transfer.rs:131-220 Incoming and unique_path are pure std::fs and path logic. Only receive_dir() (:23-28) reads USERPROFILE.
- input/src/scancode.rs: pure DOM-code to Set-1 table. input/src/lib.rs:274-290 already has a non-Windows Injector stub.
- engine/src/pipeline.rs:2449-2509: fit_within and parse_stun_mapped_address are pure, but the module is Windows-gated (engine/src/lib.rs:15-16). Moving them out would let their tests run everywhere.

## abstraction_points
- Capture: a trait such as FrameSource { acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError>; release(&mut self); reinit(&mut self) -> Result<(), CaptureError>; device(&self) -> GpuDeviceHandle } with AccessLost as a distinct variant. The only in-scope caller is latency_smoketest.rs:38-49.
- Encode/decode: VideoEncoder { encode(&mut self, frame: &GpuFrame) -> Result<Vec<EncodedUnit>, EncodeError> } and VideoDecoder { decode(&mut self, data: &[u8], keyframe: bool, ts: i64) -> Result<..> }, plus a shared GPU-device handle type. Smoketest call sites: :24, :31, :53, :59.
- Secret sealing: engine/src/secret.rs:22 protect(&str) -> Option<String> and :54 unprotect(&str) -> Option<String>. Ungated call sites at engine/src/lib.rs:458 and :517 mean every platform needs an impl (DPAPI, Keychain, or Secret Service/keyring). None must never fall back to plaintext (secret.rs:21).
- Autostart: engine/src/autostart.rs:84 is_enabled(), :91 set_enabled(bool), :146 reconcile(bool). Windows uses HKCU Run; macOS would use a LaunchAgent plist and Linux an XDG autostart .desktop file. Keep AUTOSTART_FLAG ("--autostart", :27).
- Clipboard: engine/src/clipboard.rs:54 sequence(), :61 get_text(), :89 set_text(&str) -> Option<u32>, :132 set_files(&[PathBuf]) -> Option<u32>. macOS maps to NSPasteboard changeCount; Linux has no global sequence counter under X11 or Wayland, so change detection needs a different strategy.
- Input injection: input/src/lib.rs:274-290 already provides a non-Windows Injector stub with set_target_rect and focus_window_at; the real impl is mod imp at :35.

## cross_module_contracts
- latency_smoketest.rs calls capture and codec directly on one thread with no tokio runtime or channels. Each loop iteration is synchronous, and the frame is released at :62 after encode and decode. It exercises capture, encode and decode only, with no transport or render.
- Engine pipeline (engine/src/pipeline.rs, Windows-only per engine/src/lib.rs:15-16) owns the production capture->encode->transport and transport->decode->render chains.
- Engine <-> secret: protect/unprotect are called synchronously (engine/src/lib.rs:458, :517) with owned Strings and hex-encoded ciphertext.
- Engine <-> clipboard: called from the control-channel handler (engine/src/lib.rs, cfg(windows) at :13). The echo guard depends on the sequence number returned by set_text/set_files (clipboard.rs:118, :174). Win32 clipboard is process-global.
- Engine <-> autostart: reconcile() runs at startup (cfg(windows) at engine/src/lib.rs:11).
- Engine <-> transfer: Incoming is driven by BulkFrame messages (protocol/src/transfer.rs:74-85); the receive dir is fixed by receive_dir() (engine/src/transfer.rs:23-28).
- Shared global state in tests: static CLIPBOARD_TEST_LOCK (clipboard.rs:185) serializes clipboard tests. Autostart tests mutate the real HKCU Run key (autostart.rs:186-201).

## gotchas
- BUILD BLOCKER (inferred from reading; not compiled): engine/src/lib.rs:14-15 declares `pub mod secret;` ungated, and secret.rs:15-18 imports windows::Win32::Security::Cryptography. engine/Cargo.toml:39-50 puts `windows` only under [target.'cfg(windows)'.dependencies]. So engine does not compile on macOS or Linux, and neither do its tests. The ungated engine call sites at lib.rs:458 and :517 also need non-Windows impls.
- Smoketest timing can be flattering: encode and decode errors are swallowed (latency_smoketest.rs:53 unwrap_or_default; :31 .ok() on decoder creation, so t_dec stays 0 if the decoder fails). Only units.first() is decoded (:57). The decode timestamp is samples.len() (:59), not 100-ns MF units. Decoder size is hard-coded to 1920x1080 (:31).
- Smoketest on a static screen prints 'no frames captured (static screen?)' and exits (:69-72). Acquire timeout of 32 ms with Ok(_) => continue (:40) also covers no-new-frame.
- Codec negotiation falls back to H264 even with no overlap (codec/src/lib.rs:63; tested at :104-107).
- Clipboard invariant: the sequence number advances when the clipboard is CLOSED, not when data is set. Read it only after ClipboardGuard drops (clipboard.rs:112-118, :171-173). Reading early makes the echo guard bounce text back, a ping-pong loop. Port to NSPasteboard.changeCount with the same ordering rule.
- Clipboard constants and contracts: MAX_TEXT_BYTES = 256*1024 (clipboard.rs:24). OpenClipboard retries 5 times at 10 ms (:34-39). GlobalAlloc memory is owned by the clipboard after a successful SetClipboardData and must not be freed then (:106-109). CF_HDROP layout is DROPFILES header, NUL-separated UTF-16 paths, double NUL (:130-131, :159-164). Text sync ignores file lists (test :202-204).
- Secret invariant: DPAPI failure returns None and the caller must NOT fall back to plaintext (secret.rs:21). Unprotect of a copied config returns None (:52-53). CRYPTPROTECT_UI_FORBIDDEN is set (:38, :69).
- Autostart invariants: HKCU Run is deliberate so the app starts as the logged-in user at logon, not boot (autostart.rs:3-12). Registered command must be quoted with AUTOSTART_FLAG after the closing quote (:29-34, test :175-183). reconcile() re-points stale paths (:140-145). Removing an absent value is success (:102-105).
- Transfer invariants (engine/src/transfer.rs:1-13): names are reduced to a bare component and the final path must be a direct child of the receive dir (:56-58). Nothing is overwritten; collisions get ' (n)' suffixes (:112-129). Declared size is a hard cap counted on write (:74-76). Truncated transfers are deleted (:86-95). receive_dir uses USERPROFILE with temp_dir fallback (:23-28); macOS and Linux need HOME or equivalent.
- Protocol invariants: MAX_FILE_BYTES caps declared size (used at engine/src/transfer.rs:42). BulkFrame::decode returns None for empty or unknown kinds so a malformed peer cannot panic (protocol/src/transfer.rs:72-74).
- Test-quality gotchas: several tests return early and pass when the OS resource is busy (clipboard.rs:198-200, :223-226, :235-237; autostart.rs:189-191). Two no-op assertions: protocol/src/input.rs:90-96 and protocol/src/signaling.rs:205 use matches! without assert!, so they never fail on the wrong variant.
- Autostart test enable_then_disable_roundtrips mutates the real HKCU Run key on the test machine (autostart.rs:186-201). It restores prior state but is not hermetic, and parallel runs can race.
- CI gotcha: .github/workflows/native-ci.yml runs build, test and clippy only on windows-latest (lines 19-20, 23-24, 38-45). There is no macOS or Linux job, so the engine build blocker is invisible to CI.
- pipeline.rs tests are gated by the module (engine/src/lib.rs:15-16). fit_within and parse_stun_mapped_address are pure (pipeline.rs:2451-2508) but cannot run off Windows today.
- codec tests (codec/src/lib.rs:89-108) compile only on Windows because the whole crate is #![cfg(windows)] (:16).

## tests
- codec/src/lib.rs:93 negotiation_prefers_av1_then_falls_back. Inside #![cfg(windows)] crate. Logic is pure; gated by the crate attribute only.
- input/src/scancode.rs:268 reverse_lookup_roundtrips. Portable; runs everywhere.
- input/src/scancode.rs:279 known_non_extended. Portable.
- input/src/scancode.rs:297 known_extended. Portable.
- input/src/scancode.rs:315 unsupported_and_unknown. Portable.
- transport/src/fec.rs:64 recovers_a_dropped_shard. Portable.
- protocol/src/config.rs:254 challenge_response_roundtrip. Portable.
- protocol/src/config.rs:272 password_mode_without_hash_is_fail_safe_approve. Portable.
- protocol/src/config.rs:283 recents_dedupe_and_cap. Portable.
- transport/src/packet.rs:294 roundtrip_multi_fragment_delta_frame. Portable.
- transport/src/packet.rs:312 keyframe_recovers_a_lost_fragment_via_fec. Portable.
- transport/src/packet.rs:336 incomplete_delta_frame_is_dropped_when_superseded. Portable.
- protocol/src/control.rs:68 perm_wire_shape. Portable.
- protocol/src/control.rs:78 bye_wire_shape. Portable.
- engine/src/handshake.rs:59 proof_verifies_against_config. Portable logic, but the engine crate does not compile off Windows today (secret.rs).
- engine/src/handshake.rs:69 nonce_is_16_bytes_hex. Same engine caveat. Name says 16 bytes; the assertion checks 32 hex chars.
- protocol/src/lib.rs:67 uuid_validation_matches_contract. Portable.
- protocol/src/input.rs:67 move_wire_shape. Portable.
- protocol/src/input.rs:79 button_roundtrip. Portable. Gotcha: matches! at :90 is not asserted, so it is a no-op.
- engine/src/autostart.rs:174 desired_command_is_quoted_and_flagged. Module is cfg(windows) (engine/src/lib.rs:11). Format logic is portable; the Run-key target is Windows-specific.
- engine/src/autostart.rs:185 enable_then_disable_roundtrips. NEEDS WINDOWS: writes and deletes the real HKCU Run value; returns early on registry lock (:189-191).
- engine/src/clipboard.rs:193 file_list_roundtrips_as_a_drop_list. NEEDS WINDOWS: CF_HDROP via Win32 clipboard; module cfg(windows) (engine/src/lib.rs:13). Returns early if clipboard is locked (:198-200).
- engine/src/clipboard.rs:208 empty_file_list_is_rejected. Logic portable; module Windows-gated.
- engine/src/clipboard.rs:213 sequence_is_readable. NEEDS WINDOWS: GetClipboardSequenceNumber. Asserts only that the call returns.
- engine/src/clipboard.rs:219 roundtrips_text_and_reports_new_sequence. NEEDS WINDOWS: Win32 clipboard, non-ASCII marker. Returns early on lock (:223-226).
- engine/src/clipboard.rs:232 empty_text_is_not_reported. NEEDS WINDOWS: Win32 clipboard. Conditional assertion (:235-237).
- protocol/src/signaling.rs:164 ping_literal_matches_contract. Portable.
- protocol/src/signaling.rs:173 connect_request_omits_null_routing_fields. Portable.
- protocol/src/signaling.rs:190 signal_data_roundtrip. Portable. Gotcha: matches! at :205 is not asserted.
- engine/src/secret.rs:106 roundtrips_through_dpapi. NEEDS WINDOWS: CryptProtectData/CryptUnprotectData. Cannot build off Windows today.
- engine/src/secret.rs:115 rejects_garbage. Malformed-hex case is portable; the non-DPAPI-blob case needs DPAPI on Windows. Engine caveat applies.
- engine/src/secret.rs:121 hex_roundtrip. Portable logic in a module that does not compile off Windows today.
- protocol/src/transfer.rs:126 frames_roundtrip. Portable.
- protocol/src/transfer.rs:143 decode_rejects_garbage. Portable.
- protocol/src/transfer.rs:150 sanitize_strips_path_traversal. Portable; treats / and \ and drive colon as separators on any host (:98-100).
- protocol/src/transfer.rs:168 sanitize_rejects_empty_and_dots. Portable.
- protocol/src/transfer.rs:176 sanitize_defuses_reserved_device_names. Portable; reserved names apply on every platform by design.
- protocol/src/transfer.rs:182 sanitize_drops_control_and_illegal_chars. Portable.
- engine/src/turn.rs:497 xor_addr_roundtrip_v4. Portable logic; engine caveat.
- engine/src/turn.rs:510 data_indication_roundtrip. Portable logic; engine caveat.
- engine/src/turn.rs:535 integrity_appends_24_bytes_and_patches_length. Portable (hmac/sha1/md-5); engine caveat.
- engine/src/turn.rs:548 error_code_parses. Portable; engine caveat.
- engine/src/transfer.rs:151 writes_a_file_and_reports_path. Portable; engine caveat. Does not exercise receive_dir().
- engine/src/transfer.rs:162 traversal_name_stays_inside_the_directory. Portable; engine caveat.
- engine/src/transfer.rs:174 never_overwrites_an_existing_file. Portable; engine caveat.
- engine/src/transfer.rs:188 rejects_more_bytes_than_declared. Portable; engine caveat.
- engine/src/transfer.rs:196 rejects_absurd_declared_size. Portable; engine caveat.
- engine/src/transfer.rs:203 rejects_unusable_name. Portable; engine caveat.
- engine/src/transfer.rs:210 truncated_transfer_is_discarded. Portable; engine caveat.
- engine/src/pipeline.rs:2454 fit_within_never_upscales. Pure logic; currently Windows-only only because pipeline is cfg(windows) (engine/src/lib.rs:15-16).
- engine/src/pipeline.rs:2460 fit_within_preserves_aspect_and_evenness. Pure; same gating.
- engine/src/pipeline.rs:2470 fit_within_handles_degenerate_input. Pure; same gating.
- engine/src/pipeline.rs:2477 parses_xor_mapped_address_ipv4. Pure STUN parse; same gating.
- engine/src/pipeline.rs:2502 rejects_non_success_response. Pure STUN parse; same gating.
- latency_smoketest.rs (example, not a #[test]): NEEDS WINDOWS for the measurement. Compiles and prints a notice on other OSes (:86-89). Run: cargo run -p engine --example latency_smoketest (header :8). Needs a Windows GPU with a HW H.264 MFT encoder and a changing screen (:69-72). Prints averages only, with no pass/fail threshold; budget targets are in comments at :79-81.
