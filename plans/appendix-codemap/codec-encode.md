<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# codec-encode

## summary
Unit codec-encode maps three files: native/crates/codec/src/encode.rs (782 lines, read fully), native/crates/codec/src/variant.rs (46 lines, read fully), and native/crates/codec/Cargo.toml (28 lines, read fully). encode.rs is a Windows-only Media Foundation H.264/HEVC/AV1 encoder. It takes a D3D11 BGRA texture, converts it to NV12 on the GPU via crate::convert::Converter (convert.rs, not read), and emits compressed access units as EncodedFrame {data, keyframe, timestamp}. It has two paths chosen by a `software` bool (encode.rs:216, 254). The hardware path is an async MFT driven by IMFMediaEventGenerator (encode.rs:271-302) with zero-copy input via MFCreateDXGISurfaceBuffer (encode.rs:752-767). The software path is a sync MFT fed CPU NV12 read back through a lazily created staging texture (encode.rs:340-405; encode_sync at encode.rs:309-334). Encoder::new (encode.rs:104-116) tries the hardware build and, on any error, retries the whole build with the software MFT. build (encode.rs:118-221) enumerates via MFTEnumEx, unlocks async (HW only), sets the DXGI device manager (HW only), sets the output type before the input type, applies the ICodecAPI low-latency recipe best-effort per property (encode.rs:665-718), then sends BEGIN_STREAMING and START_OF_STREAM.

Process-global state is one std::sync::Once, MF_INIT (encode.rs:21), which calls MFStartup(MF_VERSION, MFSTARTUP_LITE) once and discards the result (encode.rs:23-28). There are no threads, channels, or other statics in these files. Runtime knobs are constants and comments: MAX_QP=32 (encode.rs:72), GOP 600 (encode.rs:706), B-pictures 0 (encode.rs:698-703), QualityVsSpeed 66 (encode.rs:712-717), CBR (encode.rs:681-686), AVLowLatencyMode (encode.rs:667-672). Bitrate changes live through set_bitrate (encode.rs:224-233). Keyframes are requested via CODECAPI_AVEncVideoForceKeyFrame (encode.rs:236-241). Sample timing is synthesized as frame_index * duration, where duration = 10_000_000*fps_den/fps_num in 100ns units (encode.rs:248-250), not taken from capture timestamps.

variant.rs is a hand-rolled VARIANT builder: VT_UI4 (u32v, variant.rs:30-32), VT_UI8 (u64v, variant.rs:37-39, dead_code allowed at line 36), and VT_BOOL (boolv, variant.rs:42-46, true = -1). It is used only by encode.rs and is entirely Windows COM-specific. Cargo.toml (lines 9-28) inherits workspace package metadata and lints, depends on thiserror and tracing (OS-agnostic), and adds a cfg(windows)-only windows crate dependency with nine features (lines 16-28). No test code exists in any of the three files.

## public_api
- encode.rs:33 pub struct EncoderConfig { pub width, height, fps_num, fps_den, bitrate_bps: u32, pub codec: Codec } (Copy, Debug, Clone)
- encode.rs:42-58 impl Default for EncoderConfig: 1920x1080, fps 30/1, 8_000_000 bps, Codec::H264
- encode.rs:79 pub struct Encoder (all fields private; Option/IMF/ID3D11 COM handles)
- encode.rs:104 pub fn Encoder::new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, Error>: HW build, then SW fallback on any error
- encode.rs:224 pub fn Encoder::set_bitrate(&mut self, bps: u32) -> Result<(), Error>: updates cfg.bitrate_bps, then ICodecAPI MeanBitRate (error propagates)
- encode.rs:236 pub fn Encoder::force_keyframe(&mut self): ForceKeyFrame=1, error ignored
- encode.rs:247 pub fn Encoder::encode(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<EncodedFrame>, Error>: may return an empty Vec
- encode.rs:485 pub fn can_encode(codec: Codec) -> bool: MFTEnumEx HARDWARE count>0 probe, no activation
- encode.rs:524 pub fn host_encodable() -> Vec<Codec>: returns [Av1?, Hevc?, H264] in that order; H264 always present
- encode.rs:23 pub(crate) fn ensure_mf_startup(): idempotent MFStartup via Once
- encode.rs:585 pub(crate) fn create_dxgi_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager, Error>
- encode.rs:600 pub(crate) fn stream_ids(transform: &IMFTransform) -> (u32, u32): errors ignored, defaults 0
- variant.rs:30 pub fn u32v(v: u32) -> VARIANT (VT_UI4)
- variant.rs:37 pub fn u64v(v: u64) -> VARIANT (VT_UI8, #[allow(dead_code)]; comment says LTR buffer control, but no call exists in these files)
- variant.rs:42 pub fn boolv(v: bool) -> VARIANT (VT_BOOL, true=-1, false=0)

## platform_deps
- **Media Foundation runtime: MFStartup(MF_VERSION, MFSTARTUP_LITE)** @ encode.rs:21-28
  - purpose: Process-wide MF init required before any MFT use
  - portable: Replacement needs once-per-process hardware-codec runtime init. The result is deliberately ignored, so init failure surfaces later as an enumeration or activation failure rather than a clear error.
- **MFTEnumEx (MFT_CATEGORY_VIDEO_ENCODER) with MFT_ENUM_FLAG_HARDWARE|ASYNCMFT|SORTANDFILTER, or SYNCMFT|SORTANDFILTER for software** @ encode.rs:122-128, 546-582, 485-518
  - purpose: Find an encoder by output subtype; take the first result (OS-sorted, so best-ranked)
  - portable: Needs a backend enumeration API that filters by codec and by hardware vs software, returns ranked candidates, and supports a no-activation existence probe. Ranking is delegated to the OS; the code does not inspect candidate identity.
- **IMFActivate::ActivateObject to create IMFTransform** @ encode.rs:580
  - purpose: Instantiate the encoder MFT
  - portable: Equivalent: instantiate the encoder from a discovered handle, with failure treated as recoverable so the SW fallback can run.
- **IMFTransform::GetAttributes; SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK,1) (HW only, fatal via ?); MF_LOW_LATENCY (best-effort)** @ encode.rs:139-146
  - purpose: Allow an async MFT to be driven by the app; request low-latency mode
  - portable: Needs an explicit async-mode opt-in for HW encoders and a low-latency hint. Async-unlock failure aborts the HW build (triggering SW fallback); low-latency failure is ignored.
- **MFCreateDXGIDeviceManager + IMFDXGIDeviceManager::ResetDevice(device, token) + ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, raw ptr as usize)** @ encode.rs:148-160, 585-598
  - purpose: Bind the shared D3D11 device so the encoder reads GPU surfaces with no copy
  - portable: Requires handing the encoder the same GPU device capture and conversion use, via the backend's device-sharing mechanism. The raw pointer is passed without AddRef; lifetime is held by the _dxgi_mgr field (encode.rs:87).
- **IMFTransform::GetStreamIDs** @ encode.rs:600-607
  - purpose: Discover input/output stream IDs (fixed 0/0 acceptable)
  - portable: Replacement must expose stream/port IDs or assume 0/0; errors ignored.
- **MF media types: MFCreateMediaType; MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE (MFVideoFormat_H264/HEVC/AV1 output; MFVideoFormat_NV12 input); MF_MT_AVG_BITRATE; MF_MT_INTERLACE_MODE progressive; MF_MT_FRAME_SIZE and MF_MT_FRAME_RATE packed hi<<32|lo; MF_MT_MPEG2_PROFILE=eAVEncH264VProfile_Main (best-effort)** @ encode.rs:609-656, 735-748
  - purpose: Declare format contracts; output type set before input type
  - portable: Equivalent: a codec-format descriptor with width, height, fixed-point rate, bitrate, progressive flag, and H.264 profile hint. Order matters: the encoder expects the compressed output format before the raw NV12 input.
- **ICodecAPI::SetValue via CODECAPI_AVLowLatencyMode, CODECAPI_AVEncCommonRateControlMode (CBR), CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncVideoMaxQP, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncVideoForceKeyFrame; transform.cast::<ICodecAPI>()** @ encode.rs:173-181, 224-241, 665-731
  - purpose: Runtime encoder tuning: rate control, quality floor, GOP, B-frames, keyframes, live bitrate
  - portable: Each knob maps to a backend property (rate-control mode, bitrate, quantizer max/floor, B-frame count, GOP/keyint, speed-quality preset, force-IDR). Contract: best-effort per property; a rejected knob keeps the default. Static properties (MaxQP) must be set before streaming starts. Missing ICodecAPI is tolerated (only attributes applied).
- **MFT_MESSAGE_NOTIFY_BEGIN_STREAMING and MFT_MESSAGE_NOTIFY_START_OF_STREAM** @ encode.rs:185-188
  - purpose: Start the encoder's streaming session after configuration
  - portable: Equivalent: a start-session call after all config. No matching end/flush message exists in these files (no Drop impl; see gotchas).
- **IMFMediaEventGenerator::GetEvent(flags=0) (blocking); IMFMediaEvent::GetType; METransformNeedInput; METransformHaveOutput** @ encode.rs:271-302
  - purpose: Event-driven async pump (1:1 input to output)
  - portable: Needs a readiness signal (input-accepting and output-available events) with a blocking wait. GetEvent has no timeout (see gotchas).
- **IMFTransform::ProcessInput(input_id, sample, 0); ProcessOutput(0, [MFT_OUTPUT_DATA_BUFFER], &status); MF_E_NOTACCEPTING and MF_E_TRANSFORM_NEED_MORE_INPUT HRESULTs; GetOutputStreamInfo (flags MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, cbSize, cbAlignment)** @ encode.rs:287, 312-325, 413-461
  - purpose: Feed frames and pull compressed output
  - portable: Equivalent contract: submit a GPU frame with time and duration; pull compressed packets or a would-block result; query required output buffer size and alignment. The NOTACCEPTING path drains, retries input once, then drains again.
- **MFCreateSample; MFCreateMemoryBuffer; MFCreateAlignedMemoryBuffer; IMFSample AddBuffer/SetSampleTime/SetSampleDuration/GetSampleTime/GetUINT32(MFSampleExtension_CleanPoint); IMFMediaBuffer Lock/Unlock/SetCurrentLength; ConvertToContiguousBuffer** @ encode.rs:383-404, 418-430, 463-479, 770-782
  - purpose: Sample and buffer plumbing; keyframe flag is CleanPoint==1; contiguous copy-out
  - portable: Equivalent: packet objects with timestamp, duration, buffer, and a keyframe flag. CleanPoint is the only keyframe signal used. copy_sample_bytes allocates a fresh Vec per frame.
- **MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, subresource 0, bottom_up=false)** @ encode.rs:752-767
  - purpose: Zero-copy wrap of a GPU NV12 texture as an encoder input buffer
  - portable: Requires a GPU-native surface handle the encoder can read directly. Top-down, subresource 0, no copy.
- **D3D11: ID3D11Device::CreateTexture2D (staging, DXGI_FORMAT_NV12, D3D11_USAGE_STAGING, CPU READ); ID3D11DeviceContext::CopyResource; Map(D3D11_MAP_READ)/Unmap; mapped RowPitch; GetImmediateContext** @ encode.rs:340-405, 194-196, 253
  - purpose: Software path: GPU-to-CPU NV12 readback honoring row pitch
  - portable: Equivalent: staging readback of a 2-plane 4:2:0 frame into CPU memory, honoring source row stride. CopyResource then Map stalls the GPU; the immediate context is used synchronously.
- **D3D11_TEXTURE2D_DESC, DXGI_SAMPLE_DESC, DXGI_FORMAT_NV12 types** @ encode.rs:12-15, 348-362
  - purpose: Staging texture description
  - portable: Needs NV12 format support and CPU-readable staging allocation.
- **CoTaskMemFree for MFT-allocated IMFActivate arrays** @ encode.rs:514, 574
  - purpose: Free the array returned by MFTEnumEx (COM task allocator)
  - portable: enumerate_encoder_with frees the array without releasing elements (see gotchas). Replacement needs matching release discipline.
- **variant.rs: windows::Win32::System::Variant (VARIANT, VARENUM, VT_UI4/UI8/BOOL, VARIANT_0/0_0/0_0_0) and Foundation::VARIANT_BOOL; ManuallyDrop wrapping** @ variant.rs:6-46
  - purpose: Build COM VARIANT values for ICodecAPI::SetValue
  - portable: Entirely COM-specific. Replacement: typed property values (u32, u64, bool) for the backend's property setter; no equivalent union needed.
- **Cargo.toml target.'cfg(windows)'.dependencies.windows with features Win32_Foundation, Win32_System_Com, Win32_System_Ole, Win32_System_Variant, Win32_Media_MediaFoundation, Win32_Media_KernelStreaming, Win32_Graphics_Direct3D11, Win32_Graphics_Dxgi, Win32_Graphics_Dxgi_Common** @ Cargo.toml:16-28
  - purpose: Windows API bindings for the crate
  - portable: Gated to Windows, so on macOS/Linux the dependency is absent. Win32_System_Ole and Win32_Media_KernelStreaming are not referenced in these three files (may be used in convert.rs or lib.rs; not verified). Workspace windows version lives in the root Cargo.toml, not read.

## portable_parts
- encode.rs:33-59 EncoderConfig struct and defaults (no OS types)
- encode.rs:72 MAX_QP=32 quality floor and rationale comment (61-71)
- encode.rs:248-250 frame duration and frame-index timestamp math (10_000_000*fps_den/fps_num, 100ns units)
- encode.rs:271-302 event-driven 1:1 pump logic (feed on first NeedInput; break after a fed input plus at least one output, or on a second NeedInput with no output), as an abstract state machine
- encode.rs:309-334 sync pump logic including the NOTACCEPTING drain-retry-drain sequence
- encode.rs:665-718 low-latency policy values (CBR, MaxQP=32, B=0, GOP=600, QualityVsSpeed=66, LowLatency on) as data, plus the best-effort-per-property rule (658-664)
- encode.rs:524-533 host_encodable preference order (AV1, HEVC, H264 guaranteed)
- encode.rs:536-542 codec-to-subtype mapping (logic only; GUID values are Windows)
- encode.rs:735-748 fixed-point packing of frame size and frame rate
- encode.rs:421-422 output buffer sizing policy (cbSize if >0, else W*H*3/2; alignment via cbAlignment-1)
- encode.rs:118-134 hardware-then-software selection flags and retry structure
- variant.rs:42-46 boolv semantics (true=-1, false=0) and the intent of u32v/boolv as typed value constructors

## abstraction_points
- Encoder::encode(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<EncodedFrame>, Error> (encode.rs:247): input is a D3D11 GPU texture. Port needs a trait or enum over the backend's native GPU surface, keeping the 'may return zero frames' contract.
- Encoder::new(device: &ID3D11Device, cfg) (encode.rs:104): device type is backend-specific. Abstract as an opaque shared-GPU-device handle.
- Encoder::build(device, cfg, is_hw: bool) (encode.rs:118): the hardware/software split. Enumeration flags, async unlock, device manager, and event-pump vs sync-pump all branch on is_hw. Stored as events: Option<...> (encode.rs:82) and software: bool (encode.rs:92).
- encode() vs encode_sync() (encode.rs:262-264) and wrap_texture_sample vs readback_nv12_sample (encode.rs:254-260): zero-copy vs CPU-readback input path. Abstract as a frame-submission strategy.
- drain_output sample allocation (encode.rs:414-431): MFT-provided vs caller-provided output samples. Abstract as an output-buffer strategy.
- apply_low_latency_recipe(api: &ICodecAPI, cfg) (encode.rs:665) and set_codec_value (encode.rs:721): a backend property-setter seam with knob names. Each knob (rate control, quantizer floor, GOP, B-frames, speed/quality, keyframe, low latency, bitrate) is a per-backend mapping. Keep the best-effort rule.
- can_encode / host_encodable / enumerate_encoder_with (encode.rs:485-582): backend discovery seam returning codecs and a no-activation existence probe.
- variant.rs u32v/u64v/boolv (variant.rs:30-46): typed property-value constructors; these vanish entirely outside COM.
- crate::convert::Converter (encode.rs:90, 194, 253): BGRA-to-NV12 GPU converter, a clear seam. Signature is inferred from use only (new(device, w, h) -> Result; convert_to_nv12(texture) -> Result<ID3D11Texture2D>). convert.rs was not read.

## cross_module_contracts
- Imports from crate root (lib.rs, not read): Codec enum with variants H264, Hevc, Av1 (encode.rs:526-540, 56); EncodedFrame { data: Vec<u8>, keyframe: bool, timestamp: i64 } (encode.rs:475-478); Error with variants NoEncoder(Codec) and Win(windows::core::Error) (encode.rs:128, 326, 371, 592). Error must be constructible from windows::core::Error via ? (encode.rs:142, 155, 169 and others).
- crate::convert::Converter from convert.rs (not read), used by encode.rs:90, 194, 253. Inferred: Converter::new(&ID3D11Device, u32, u32) -> Result<Converter, Error>; convert_to_nv12(&mut self?, &ID3D11Texture2D) -> Result<ID3D11Texture2D, Error>, with the returned texture possibly reused across calls (inferred, unverified).
- Caller contract (encode.rs:47-52, 31): the capture loop must feed frames at exactly fps_num/fps_den, because CBR divides bitrate by the declared rate. A mismatch (declared 60, fed 30) halved delivered bits, per the comment.
- Caller contract (encode.rs:31, 223-233): bitrate is driven live by the transport's bandwidth estimate via set_bitrate.
- Caller contract (encode.rs:235-241): force_keyframe is called on viewer join or decoder loss. Result is best-effort and silent.
- Caller contract (encode.rs:247-304): encode may return Ok(empty), for example when the encoder has not yet produced output. The caller must tolerate zero frames per input and must not assume 1:1.
- Device sharing (encode.rs:99-100, 194-196, 218): the same ID3D11Device must back capture, conversion, the DXGI manager, and the staging path. The encoder stores device.clone() and uses GetImmediateContext, so all GPU calls go through the immediate context on the calling thread. No multi-thread sharing is established in these files.
- Process-global MF init (encode.rs:21-28): invoked by Encoder::new and by can_encode/host_encodable (encode.rs:105, 486). Must happen before any MFT call. No MFShutdown exists in these files.
- Threading: no unsafe impl Send/Sync in these files; no thread is spawned. Whether Encoder may move across threads is decided by the windows crate's interface types (not verified). COM apartment initialization is not performed in these files (no CoInitializeEx), so it must happen on the caller's thread.
- Downstream: output Vec<EncodedFrame> feeds packetization (encode.rs:769 comment: 'for packetization'). Packetization module not read.

## gotchas
- Timestamps are synthesized as frame_index * duration using the declared frame rate, not capture timestamps (encode.rs:248-250). Any capture-rate mismatch or dropped frame desynchronises timing. frame_index increments before encode succeeds (encode.rs:250), so a failed encode still advances time.
- Staging map leak on error paths (encode.rs:378-397): after Map at 379, the early-return `?` operators at 383, 385, 395 and 396 skip Unmap at 397. The staging resource stays mapped and the next Map(READ) on it will fail. Keep the software path's map/unmap in a guard.
- enumerate_encoder_with leaks COM references (encode.rs:571-574): it clones slice[0], then frees the MFT-allocated array with CoTaskMemFree without releasing any element, so every IMFActivate stays referenced. can_encode (encode.rs:508-515) does release each element with take() before freeing, so the two paths differ.
- The HW failure in Encoder::new is only logged (encode.rs:108-113) and the original error is discarded when the SW build starts. Only the SW build's error is returned.
- set_bitrate updates cfg.bitrate_bps before calling the encoder (encode.rs:225-231), so cfg reflects the requested rate even if SetValue fails and returns Err.
- GetEvent blocks with no timeout (encode.rs:274-279). If the HW encoder never emits NeedInput or HaveOutput, encode hangs. No watchdog, no timeout parameter.
- The async pump breaks on a second NeedInput without output (encode.rs:289-292), returning an empty Vec, and breaks when it has output after feeding (encode.rs:298-301). An encoder that buffers output past one input is fed the next frame's NeedInput and returns nothing for this call, so output is delayed by one or more frames.
- HaveOutput arriving before any input was fed calls drain_output (encode.rs:293-296). ProcessOutput then returns NEED_MORE_INPUT, which is mapped to Ok(None) (encode.rs:452-453).
- drain_output ignores pEvents content (encode.rs:448-449: the collection is dropped without inspection). MFT stream-change events and dwStatus flags such as MFT_OUTPUT_DATA_BUFFER_INCOMPLETE are not handled. Only NEED_MORE_INPUT is special-cased.
- Output buffer fallback size is W*H*3/2 (encode.rs:421), the raw NV12 frame size, though the comment on line 420 calls it the compressed worst case. Used only when cbSize is 0.
- Keyframe detection relies solely on MFSampleExtension_CleanPoint == 1 (encode.rs:466-472). An encoder that does not set it reports every frame as non-key; a missing attribute defaults to false via unwrap_or(false).
- Software readback does CopyResource, Map and a CPU row copy every frame (encode.rs:375-397), allocating a new MF memory buffer per frame (encode.rs:383). It stalls the GPU and allocates per frame. The staging texture is reused; the MF buffer is not.
- readback_nv12_sample takes width and height from cfg (encode.rs:346, 357-358), not from the input texture's desc. An input whose size differs from cfg silently produces a wrong-sized copy. No size check exists.
- Software readback assumes the source chroma plane begins at pitch*h (encode.rs:390) and that the output buffer is tightly packed at stride = width (encode.rs:338-339, 387-394). It relies on D3D11's NV12 layout: Y plane, then interleaved UV at half height.
- MFT_MESSAGE_SET_D3D_MANAGER passes the COM pointer as usize (encode.rs:154-155) without AddRef. Lifetime depends on the _dxgi_mgr field (encode.rs:87). Field declaration order puts transform (encode.rs:80) before _dxgi_mgr (encode.rs:87), so the transform drops first at teardown. Reordering the fields inverts this.
- There is no Drop impl and no flush or end-of-stream message (MFT_MESSAGE_NOTIFY_END_OF_STREAM, NOTIFY_END_STREAMING, DRAIN) anywhere in these files. Teardown relies on COM Release alone.
- The module doc (encode.rs:1-7) claims an LTR-recovery keyframe strategy and an 'effectively-infinite GOP'. The code does neither: there is no LTR control call, and GOPSize is 600 (encode.rs:704-706, whose comment says some H.264 MFTs reject u32::MAX). u64v (variant.rs:36-39) is dead_code and its comment claims LTR buffer control, but nothing calls it in these files.
- The module doc (encode.rs:1) says H264/HEVC, but output_subtype and host_encodable also cover AV1 (encode.rs:526, 540).
- apply_low_latency_recipe asks for CODECAPI_AVEncVideoMaxQP (encode.rs:696) as a static property that must precede BEGIN_STREAMING (encode.rs:693-694). The call order in build (encode.rs:176, then 186) satisfies this.
- MF_MT_MPEG2_PROFILE is forced to Main for H.264 only (encode.rs:630-632), best-effort. HEVC and AV1 get no profile hint.
- The ICodecAPI cast is the only route to tuning knobs. When it fails, only MF_LOW_LATENCY runs and all rate control, QP, GOP and B-frame settings are silently skipped (encode.rs:173-181).
- force_keyframe discards the error (encode.rs:239); set_bitrate propagates it (encode.rs:229-230).
- The software fallback is built without async unlock and without a device manager (encode.rs:139-141, 150-159). It therefore reads CPU input only and never touches the shared device directly.
- convert_to_nv12 output goes into wrap_texture_sample (encode.rs:253, 259) and into the software path's clone (encode.rs:256). On the HW path the MF sample references the texture only through the surface buffer. Its lifetime across the blocking encode loop depends on Converter reusing its output (not verified).
- A zero-output encode call is a normal outcome (encode.rs:266-303 returns Ok(out) even when empty). Callers must not treat an empty Vec as an error.

## tests
- No #[test] or #[cfg(test)] blocks exist in encode.rs, variant.rs, or Cargo.toml (verified by full read).
- No integration or fixture tests are referenced from these files. Encoder behaviour is exercised only at run time through the app (not verified elsewhere).
- Portability: all encoder logic requires a D3D11 device and the MF runtime (Windows). Pure logic (EncoderConfig defaults, output_subtype mapping, host_encodable ordering, frame duration math, set_frame_size/rate packing, boolv/u32v values) is OS-agnostic and testable without Windows once the Windows-typed signatures are split out.
