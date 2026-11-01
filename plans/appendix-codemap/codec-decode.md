<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# codec-decode

## summary
Unit codec-decode covers three files under C:/Yanivs/shareCtrlScreen/native/crates/codec/src: decode.rs (761 lines, the D3D11/Media Foundation decoder), lib.rs (109 lines, crate root with the Codec enum, negotiation, EncodedFrame and Error), and convert.rs (157 lines, a BGRA-to-NV12 converter that belongs to the encode side and is included only because it was in the task list). The whole crate is gated by `#![cfg(windows)]` at lib.rs:16, so nothing in it compiles on macOS or Linux. Only lib.rs:26-75 (Codec, negotiate, EncodedFrame) and the pure helper copy_nv12 (decode.rs:444-470) contain no OS API. Everything else talks to Media Foundation (MFT, IMFSample, IMFMediaType, IMFDXGIDeviceManager), Direct3D 11 (ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoDevice/VideoProcessor in convert.rs) or DXGI (DXGI_FORMAT_NV12).

decode.rs implements a `Decoder` that wraps an IMFTransform decoder MFT for H.264, HEVC or AV1. It prefers a hardware MFT (enumerated via MFTEnumEx with MFT_ENUM_FLAG_HARDWARE, decode.rs:585-622), binds it to the shared ID3D11Device through MFT_MESSAGE_SET_D3D_MANAGER (decode.rs:82-88), and outputs GPU surfaces returned as an (ID3D11Texture2D, array_index) pair (decode.rs:748-761). If no hardware MFT exists it falls back to the synchronous software MFT, copies the system-memory NV12 output into a STAGING texture with Map, then CopyResource into a DEFAULT shader-resource texture (decode.rs:342-416). decode() (decode.rs:159-195) feeds one access unit, handles MF_E_NOTACCEPTING by draining and retrying once, then calls drain_output (decode.rs:230-324), a 3-attempt loop that absorbs MF_E_TRANSFORM_STREAM_CHANGE by adopting the decoder's NV12 type and resizing. The public discovery helpers can_decode, can_decode_hw and viewer_decodable (decode.rs:518-578) decide what the viewer advertises during codec negotiation.

convert.rs is the encode-side BGRA-to-NV12 path. It builds an ID3D11VideoProcessorEnumerator and processor at a hard-coded 60/60 fps progressive content description (convert.rs:35-57), allocates one reusable NV12 RENDER_TARGET texture and output view (convert.rs:59-96), and per frame creates an input view and calls VideoProcessorBlt (convert.rs:110-156). lib.rs declares the module shape: `mod convert` (private), `pub mod decode`, `pub mod encode`, `mod variant`, and re-exports DecodedFrame and Decoder (lib.rs:18-24). No static, lazy or global state is defined in any of these three files. The only process-wide initialisation the decoder depends on is encode::ensure_mf_startup (MFStartup), called from decode.rs:74 and decode.rs:529 but defined in encode.rs, which was not part of this unit.

## public_api
- lib.rs:23 pub use decode::{DecodedFrame, Decoder}
- lib.rs:24 pub use encode::{Encoder, EncoderConfig}
- lib.rs:29 pub enum Codec { H264, Hevc, Av1 } (derives Debug, Clone, Copy, PartialEq, Eq)
- lib.rs:36 Codec::as_caps_str(self) -> &'static str ("H264"/"HEVC"/"AV1")
- lib.rs:44 Codec::from_caps_str(s) -> Option<Codec>
- lib.rs:57 Codec::negotiate(host_encodes: &[Codec], viewer_decodes: &[Codec]) -> Codec (preference AV1, HEVC, H264; H264 fallback even without overlap)
- lib.rs:69 pub struct EncodedFrame { data: Vec<u8>, keyframe: bool, timestamp: i64 } (100-ns MF timestamp)
- lib.rs:78 pub enum Error { Win(windows::core::Error) via #[from], NoEncoder(Codec), NoDecoder(Codec), NeedMoreInput }
- decode.rs:29 pub struct DecodedFrame { pub texture: ID3D11Texture2D, pub array_index: u32, pub timestamp: i64 }
- decode.rs:38 pub struct Decoder (all fields private)
- decode.rs:68 Decoder::new(device: &ID3D11Device, codec: Codec, width: u32, height: u32) -> Result<Self, Error>
- decode.rs:159 Decoder::decode(&mut self, au: &[u8], keyframe: bool, timestamp: i64) -> Result<Option<DecodedFrame>, Error>
- decode.rs:433 Decoder::dimensions(&self) -> (u32, u32)
- decode.rs:518 pub fn can_decode(codec) -> bool (hardware OR software MFT enumerated; the software branch is a SYNCMFT enumeration)
- decode.rs:524 pub fn can_decode_hw(codec) -> bool
- decode.rs:569 pub fn viewer_decodable() -> Vec<Codec> (AV1/HEVC only if hardware; H264 always appended last)
- convert.rs:19 pub(crate) struct Converter (private fields: video_device, video_context, enumerator, processor, nv12, output_view)
- convert.rs:29 Converter::new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, Error>
- convert.rs:110 Converter::convert_to_nv12(&mut self, bgra: &ID3D11Texture2D) -> Result<&ID3D11Texture2D, Error>

## platform_deps
- **Crate-wide cfg(windows) gate** @ lib.rs:16
  - purpose: Excludes the whole codec crate from non-Windows builds; everything below is therefore compiled only on Windows.
  - portable: A port must replace the entire module tree, not individual calls. The Codec enum, negotiate, EncodedFrame and the negotiation unit test are the only parts that could be moved behind a cfg-free module.
- **Media Foundation transform (IMFTransform) decoder via MFTEnumEx + IMFActivate::ActivateObject** @ decode.rs:585-622 (enumerate_decoder); decode.rs:528-559 (can_decode_with); decode.rs:518-526 (can_decode / can_decode_hw)
  - purpose: Discover and instantiate hardware (MFT_ENUM_FLAG_HARDWARE) or synchronous software (MFT_ENUM_FLAG_SYNCMFT) video decoder MFTs for MFVideoFormat_H264/HEVC/AV1 input, MFMediaType_Video major type.
  - portable: Need an equivalent per-codec enumeration of hardware decoders (flagging HW or SW) and an instantiation call that returns a decoder object with a feed/drain interface and a per-stream output-format query. Enumeration without activation (can_decode_with) must be cheap; activation of the first candidate is the only instantiation attempt in enumerate_decoder.
- **MFT input/output pipeline calls: ProcessInput, ProcessOutput, GetOutputStreamInfo, GetOutputAvailableType, GetOutputCurrentType, SetInputType, SetOutputType, ProcessMessage (BEGIN_STREAMING, START_OF_STREAM, SET_D3D_MANAGER)** @ decode.rs:86 (SET_D3D_MANAGER), 105-109 (stream ids, input/output types), 113-114 (NOTIFY_BEGIN_STREAMING, START_OF_STREAM), 176 (ProcessInput), 208 (GetOutputStreamInfo), 243-246 (ProcessOutput), 656 and 707 (GetOutputAvailableType iteration), 423-424 (GetOutputCurrentType), 670 and 711 (SetOutputType)
  - purpose: Push compressed access units in and pull decoded frames out with a single-element output buffer array.
  - portable: Required behaviour: (1) input can return a 'not accepting' status meaning drain then resubmit; (2) output can return 'need more input' (no frame yet, not an error); (3) output can return a 'stream change' status after which the frame that triggered it is still pending and must be re-requested after the new output type is adopted; (4) either the MFT supplies its own output samples (hardware) or the caller supplies sized buffers from the reported stream info (software); (5) format queries return a list of available types that is iterated until failure.
- **D3D manager binding: IMFDXGIDeviceManager created from the shared ID3D11Device, passed to the MFT as a raw pointer (mgr.as_raw() as usize)** @ decode.rs:82-88 (bind), decode.rs:44 and 141 (ownership held in _dxgi_mgr), encode.rs create_dxgi_manager (called at decode.rs:83, not read in this unit)
  - purpose: Lets the hardware decoder allocate its output surfaces as GPU textures on the same device the renderer uses, so decoded frames are zero-copy.
  - portable: Equivalent: hand the hardware decoder the GPU device/context that will sample the frames (for example a Metal device, or a VA-API display plus a GL/Vulkan import context on Linux). Only hardware decoders receive the binding; software decoders must not be given it (decode.rs:79-81 notes they reject it).
- **D3D11 device and immediate context: ID3D11Device::GetImmediateContext, CreateTexture2D, ID3D11DeviceContext::Map/Unmap/CopyResource, device.clone()** @ decode.rs:119 (immediate context), decode.rs:143 (device clone), decode.rs:345-360 (texture creation), decode.rs:375-376 (Map WRITE on STAGING), decode.rs:411 (Unmap), decode.rs:413 (CopyResource staging to DEFAULT), decode.rs:474-503 (create_nv12_texture)
  - purpose: Software-decode upload path: a CPU-written staging NV12 texture is copied into a sampleable NV12 texture that the renderer samples like a hardware frame.
  - portable: Equivalent must provide a CPU-writable NV12 (or plane-wise Y and UV) buffer that is then made available to the GPU sampler with one copy. The immediate context is used from the decode thread; a port must keep that context single-threaded or serialised with the renderer.
- **DXGI_FORMAT_NV12 texture format and DXGI_SAMPLE_DESC** @ decode.rs:22, 487; convert.rs:14, 65 (encode-side texture)
  - purpose: The decoded and converted pixel format is 8-bit NV12 (Y plane then interleaved UV plane, chroma at stride*coded_rows).
  - portable: The native format of the hardware decoder and the renderer must agree on NV12 layout or add a conversion. Both the coded height (chroma offset) and the pitch matter; see copy_nv12.
- **IMF2DBuffer::Lock2D / Unlock2D (authoritative stride), with contiguous IMFMediaBuffer::Lock fallback** @ decode.rs:385-396 (Lock2D with pitch.unsigned_abs()), decode.rs:401-409 (contiguous Lock fallback using src_stride), decode.rs:418-431 (src_stride from MF_MT_DEFAULT_STRIDE)
  - purpose: Obtain the real row pitch of a system-memory NV12 sample from the software decoder.
  - portable: A port must expose the row stride of the CPU-mapped decoder output. The code takes the absolute value of a signed pitch (negative means bottom-up) and does not reverse row order.
- **Media Foundation media type attributes: MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_FRAME_SIZE (packed as (w<<32)|h), MF_MT_DEFAULT_STRIDE, MF_MT_MINIMUM_DISPLAY_APERTURE (16-byte MFVideoArea blob read with from_le_bytes at offsets 8..16)** @ decode.rs:633-637 (input type), 659-661 (coded size, stride), 683-695 (display_aperture), 708-710 (output type frame size), 426 (stride)
  - purpose: Describe the coded and displayed size of the stream. Display aperture gives the cropped visible size (for example 1080 out of coded 1088 for H.264).
  - portable: Need a way to query the decoder's coded size, the visible (cropped) rectangle and the row stride after a format change. The blob parsing is little-endian and assumes exactly 16 bytes.
- **MF_LOW_LATENCY attribute on the transform (IMFTransform::GetAttributes + SetUINT32)** @ decode.rs:99-103
  - purpose: Ask the decoder for low-latency mode (no output reordering delay); failure is ignored.
  - portable: Equivalent: a flag that disables reorder/delay buffering in the decoder session. The header comment at decode.rs:13-14 refers to CODECAPI_AVLowLatencyMode as VT_UI4, but the code sets MF_LOW_LATENCY via SetUINT32; a port should not copy the comment's API name.
- **MFSampleExtension_CleanPoint on input samples; MFCreateMemoryBuffer / MFCreateAlignedMemoryBuffer / MFCreateSample / SetSampleTime / SetSampleDuration (166_667 hard-coded = 60 fps)** @ decode.rs:722-745 (make_input_sample, duration at 738, CleanPoint at 740-742), decode.rs:202-228 (make_output_sample buffer creation with cbAlignment-1)
  - purpose: Wrap each access unit in a sample with a 100-ns timestamp and a duration (the software AV1 MFT will not emit output without a duration), mark IDRs as clean points, and allocate output sample buffers for software decoders.
  - portable: The keyframe flag must reach the decoder as a random-access point marker. The 60 fps duration is an assumed constant independent of the real frame rate.
- **MFT_OUTPUT_STREAM_PROVIDES_SAMPLES flag test** @ decode.rs:209-211 (make_output_sample)
  - purpose: Distinguish decoders that allocate output samples themselves (hardware) from those that need caller buffers (software).
  - portable: Behavioural contract only: the decoder reports whether the caller must allocate output.
- **MFT_MESSAGE_NOTIFY_BEGIN_STREAMING and MFT_MESSAGE_NOTIFY_START_OF_STREAM** @ decode.rs:113-114
  - purpose: Start the decoder session after types are set.
  - portable: Equivalent: explicit session start after format setup. There is no matching END_OF_STREAM, END_STREAMING or Shutdown call anywhere in this unit (no Drop impl; see gotchas).
- **HRESULT codes: MF_E_NOTACCEPTING (decode.rs:178), MF_E_TRANSFORM_NEED_MORE_INPUT (252), MF_E_TRANSFORM_STREAM_CHANGE (256), MF_E_INVALIDMEDIATYPE (only referenced in a comment, decode.rs:260)** @ decode.rs:178, 252, 256, 260
  - purpose: Flow-control signals from the MFT: back-pressure, no-output-yet and format-change.
  - portable: The three signals map to: input back-pressure (drain then retry), no output available yet, and output format changed (renegotiate, then retry without consuming the pending frame).
- **CoTaskMemFree for the array returned by MFTEnumEx** @ decode.rs:555 (can_decode_with), decode.rs:613 (enumerate_decoder)
  - purpose: Free the array allocated by MFTEnumEx.
  - portable: Any port of enumeration needs an equivalent array-release contract; see the leak noted in gotchas.
- **D3D11 video processor pipeline: ID3D11VideoDevice (cast from device), ID3D11VideoContext (cast from context), CreateVideoProcessorEnumerator, CreateVideoProcessor, CreateVideoProcessorInputView, CreateVideoProcessorOutputView, VideoProcessorBlt; D3D11_VIDEO_PROCESSOR_CONTENT_DESC with 60/60 fps** @ convert.rs:30-33 (casts), 35-57 (content desc and enumerator/processor), 80-96 (output view), 114-133 (input view per frame), 142-149 (VideoProcessorBlt), 152 (input view release)
  - purpose: Encode-side GPU BGRA to NV12 colour conversion and chroma subsampling without CPU readback. Belongs to the encode path; included here only because it was in the task list.
  - portable: Contract: the GPU converter must take a BGRA frame and write an NV12 target in place, with the output target reused across frames. The hard-coded 60/60 content rate (convert.rs:37-47) is not derived from the real capture rate.

## portable_parts
- lib.rs:29-42 Codec enum with as_caps_str (OS-agnostic, string mapping only)
- lib.rs:44-51 Codec::from_caps_str (OS-agnostic)
- lib.rs:57-64 Codec::negotiate (OS-agnostic preference logic: AV1, HEVC, H264 fallback)
- lib.rs:68-75 EncodedFrame struct (OS-agnostic data container)
- lib.rs:89-109 negotiation unit test (OS-agnostic logic, currently gated by cfg(windows) at the crate root)
- decode.rs:444-470 copy_nv12 (pure pointer and pitch copy with independent pitches and coded heights; no OS API)
- decode.rs:518-526 can_decode and can_decode_hw as policy wrappers over can_decode_with
- decode.rs:569-578 viewer_decodable policy (AV1 and HEVC advertised only when hardware-decodable, H.264 always appended)
- decode.rs:505-511 input_subtype mapping from Codec to MF subtype GUIDs (the GUID values are MF-specific, but the mapping shape is portable)
- decode.rs:166-171, 330-335 rate-limited debug logging pattern (count-based: first 10 then every 120)
- decode.rs:159-195 decode() control flow (feed, handle not-accepting, drain) can be kept as the structure of a port with the MF calls replaced

## abstraction_points
- decode.rs:68 Decoder::new(device: &ID3D11Device, codec: Codec, width, height) is the natural seam: a trait with new(gpu_device, codec, width, height) -> Result<Self>, decode(&mut self, au: &[u8], keyframe: bool, timestamp: i64) -> Result<Option<Frame>>, and dimensions() -> (u32,u32), where Frame is an opaque per-platform GPU surface plus index and timestamp (mirroring DecodedFrame at decode.rs:29-33).
- decode.rs:29-33 DecodedFrame { texture: ID3D11Texture2D, array_index: u32, timestamp: i64 }: the texture field is the only D3D type in the public output and is the seam for a platform-neutral frame (surface handle + plane/slice index + timestamp).
- decode.rs:518, 524, 569 can_decode / can_decode_hw / viewer_decodable: pure policy over an enumeration query. Abstract as a hardware-decoders query and a software-decoders query per codec, keeping viewer_decodable's AV1/HEVC-only-if-hardware policy (decode.rs:562-578) in shared code.
- lib.rs:57 Codec::negotiate and lib.rs:36-51 caps string conversions are already OS-agnostic and can be lifted to a shared module as-is.
- decode.rs:342-416 upload_software_sample: the CPU-staging-to-GPU upload is the seam between a system-memory decoder output and a GPU-texture renderer input. On macOS/Linux, one abstraction for 'NV12 CPU buffer to GPU sampleable NV12 image' covers this.
- decode.rs:444-470 copy_nv12 is already a pure function over raw pointers and pitches and needs no abstraction.
- convert.rs:19-157 Converter (pub(crate)): a trait with new(device, w, h) and convert_to_nv12(&mut self, bgra_frame) -> &nv12_frame; the platform implementation is the GPU video processor on Windows, with another colour-conversion path elsewhere.

## cross_module_contracts
- The caller (outside this unit, probably the viewer/renderer) creates the ID3D11Device passed to Decoder::new (decode.rs:68-70). The Decoder clones that device (decode.rs:143) and fetches the immediate context (decode.rs:119); both are reference-counted COM objects, so the device outlives the decoder.
- Output textures handed out in DecodedFrame (decode.rs:29-33) are shared COM references to the decoder's own output surface (hardware path, sample_to_texture decode.rs:748-761) or to Decoder-owned upload_tex (software path, decode.rs:416). On the hardware path, the MF sample wrapping the surface is dropped at the end of drain_output (decode.rs:298-321), while DecodedFrame keeps a texture reference. The MFT can recycle the surface slot on a later ProcessOutput, so the caller must consume or copy the texture before calling decode() again. This is an implied invariant that no comment states.
- Decoder::dimensions() (decode.rs:433-435) changes mid-stream after a format change (decode.rs:274-288), but DecodedFrame carries no width or height. Callers must re-query dimensions() after each decode() that returns Some.
- The software upload path (decode.rs:342-416) writes self.upload_staging and self.upload_tex, then calls CopyResource on the shared immediate context. The renderer must read upload_tex after decode() returns and must not use the same immediate context from another thread concurrently. Nothing in this unit enforces that.
- Transport contract (lib.rs:68-75 EncodedFrame and decode.rs:159-163): decode() expects one complete access unit per call, a keyframe flag that means a clean random-access point, and a 100-ns presentation timestamp. The decoder assumes zero output reordering (decode.rs:11-13) because the encoder uses zero B-frames; there is no reorder buffer here.
- Negotiation contract (lib.rs:57-64, decode.rs:569-578): the viewer advertises viewer_decodable() and the host picks from the intersection. Negotiate always falls back to H264 even without overlap (lib.rs:63), so H.264 must be decodable on every viewer (guaranteed by the software MFT fallback).
- Error contract (lib.rs:77-87): Error::Win wraps the windows crate error. Hardware and software decoder failures both surface as Error::Win. NoDecoder(codec) is returned only when enumeration finds nothing (decode.rs:77) or a required output type is missing (decode.rs:676, 717). Other error sites in this file hard-code Codec::H264 regardless of the real codec (decode.rs:365, 369, 502, 676, 717).
- No channels, spawned threads, async runtime or statics appear in these three files. Thread safety is not declared anywhere (no unsafe impl Send or Sync in decode.rs), so Send/Sync status is whatever the windows crate's COM wrappers provide. The code assumes single-threaded use through the immediate context (convert.rs:109 says 'single encode thread').

## gotchas
- The header comment says low latency should be set through CODECAPI_AVLowLatencyMode as VT_UI4 (decode.rs:13-14). The code instead sets MF_LOW_LATENCY through IMFTransform::GetAttributes().SetUINT32 (decode.rs:99-103) and ignores errors. The comment names a different API from the one used.
- Low latency only works with zero reordering from the encoder (decode.rs:11-13). This unit has no reorder buffer, so a stream with B-frames would silently produce out-of-order timestamps.
- Software upload takes the absolute value of a signed Lock2D pitch (decode.rs:392), so a negative (bottom-up) pitch is treated as top-down. If both the 2D lock and the contiguous lock fail, the staging texture is still copied to the GPU texture (decode.rs:401-413), so the renderer shows stale or uninitialised data. The copy result is never checked.
- copy_nv12 (decode.rs:444-470) places the chroma plane at pitch*max(src_rows, h) and copies h/2 rows, so the caller's coded height (src_height) must be correct. src_height is initialised to the display height (decode.rs:147) and updated from the coded height only after a stream change (decode.rs:266-268). Before the first stream change, a 1080p H.264 stream with coded height 1088 copies chroma from the wrong offset on the software path.
- Construction calls set_decoder_output_type with the caller's width and height (decode.rs:109, 697-718), and SetOutputType is propagated with `?` (decode.rs:711). The comment at decode.rs:258-260 says forcing 1080 back onto an H.264 output fails with MF_E_INVALIDMEDIATYPE. The constructor therefore depends on the MFT accepting the hint size, while the mid-stream path adopts the advertised type as-is. These two behaviours conflict, and the constructor is the side that can fail.
- A stream change is absorbed inside drain_output by a 'continue' that uses one of the three attempts (decode.rs:233, 289). Three consecutive stream changes return Ok(None) with a frame still pending inside the MFT (decode.rs:323), so that frame is delayed until the next decode() call.
- In the MF_E_NOTACCEPTING branch, if drain_output returns Some(f) and the re-feed ProcessInput then fails, the `?` at decode.rs:183 returns Err and discards f. If drain returns None, the sample is fed a second time (decode.rs:186) and the function falls through to a drain (decode.rs:194). A decoded frame is lost on error after a successful drain.
- decode() calls drain_output once per input (decode.rs:194), so a decoder that emits more than one output per input yields at most one frame per call. The rest wait in the MFT for later calls. No flush or end-of-stream handling exists in this unit.
- Hardware decoders get no caller-supplied output sample (make_output_sample returns None at decode.rs:203-205), so the code relies on the MFT to provide it. A hardware MFT that did not set MFT_OUTPUT_STREAM_PROVIDES_SAMPLES would make ProcessOutput fail. The software path checks the flag at decode.rs:209 and returns None if the MFT provides samples, so both paths rely on the MFT's own report.
- enumerate_decoder (decode.rs:585-622) keeps only the first activate: `let first = slice[0].clone()` (decode.rs:612), then CoTaskMemFree on the array (decode.rs:613). The IMFActivate references in the array are never released, so every candidate's reference leaks, not just the unused ones. By contrast, can_decode_with (decode.rs:550-556) calls take() on each entry before freeing the array. If ActivateObject fails for the first candidate (decode.rs:618), the whole function returns an error and does not try the next candidate or fall back to software.
- can_decode (decode.rs:518-521) returns true when a software decoder exists (SYNCMFT branch at decode.rs:520), even though viewer_decodable advertises AV1 or HEVC only when can_decode_hw is true (decode.rs:571-574). The name can_decode does not mean hardware-decodable; the policy lives in viewer_decodable.
- The Decoder has no Drop impl, and END_OF_STREAM, END_STREAMING and Shutdown messages are never sent (decode.rs:113-114 are the only ProcessMessage calls besides SET_D3D_MANAGER). Teardown depends on COM Release order. Field declaration order drops transform (decode.rs:39) before _dxgi_mgr (decode.rs:44), so the manager outlives the transform. This implicit ordering invariant is not stated in a comment.
- The software path makes a full CPU copy of each frame (Map, memcpy, Unmap, then CopyResource, decode.rs:372-413) and creates two D3D textures on first use (decode.rs:344-360). No reuse or double buffering exists beyond the single staging texture that is rewritten each frame.
- The software fallback is selected only when hardware enumeration returns zero candidates or a null array (decode.rs:607-609 via continue). If hardware enumeration succeeds but ActivateObject fails, there is no software fallback (see the enumerate_decoder gotcha).
- make_input_sample (decode.rs:722-745) calls MFCreateMemoryBuffer(au.len() as u32) without a bounds check, so an access unit of 4 GiB or more would truncate. Practically irrelevant, but it is an unchecked cast.
- CleanPoint is set only when keyframe is true (decode.rs:740-742), and is absent otherwise rather than set to 0. The 60 fps sample duration (decode.rs:738) is a hard-coded constant even though the real frame rate is negotiated elsewhere.
- Errors hard-code Codec::H264 at decode.rs:365, 369, 502, 676 and 717, and convert.rs:78 and 96 hard-code NoEncoder(H264) on the encode path. An HEVC or AV1 failure is reported as an H.264 error.
- display_aperture (decode.rs:683-695) reads the MFVideoArea blob with little-endian integer reads at fixed offsets and treats a non-positive size as 'no aperture'. The coded size then stands in for the display size (decode.rs:662), so cropping is lost on decoders that publish no aperture.
- adopt_decoder_output_type (decode.rs:649-677) and set_decoder_output_type (decode.rs:697-718) both iterate available output types with `while let Ok(...)` until the first failure. Any error other than end-of-list ends the iteration silently.
- The hardware-path DecodedFrame texture is a raw cast through GetResource with an out-pointer (decode.rs:754-757). The returned texture is an owned COM reference released when DecodedFrame drops. A GetSubresourceIndex failure silently defaults the array index to 0 (decode.rs:758).
- convert.rs Converter::new creates the VideoProcessor with a fixed 60/60 content description (convert.rs:37-47) and reuses one NV12 target (convert.rs:59-78), but creates a fresh input view per frame (convert.rs:114-133). The stream is cloned for the Blt call (convert.rs:147) and the original ManuallyDrop is reclaimed afterward (convert.rs:152), so the input view is dropped exactly once.
- Mid-stream resize (decode.rs:274-288) rebuilds the software upload textures at the new size on the next frame, but the hardware path has no equivalent. The new size is visible only through dimensions(), so callers must re-check it on every decoded frame, not only at construction.

## tests
- lib.rs:89-109 (module tests, negotiation_prefers_av1_then_falls_back): checks Codec::negotiate for AV1 preference, HEVC and H264 fallback, and the H264 default with no overlap. The logic is pure and OS-agnostic, but the whole crate is cfg(windows) (lib.rs:16), so this test runs only on Windows today. It would move to a shared module unchanged.
- decode.rs: no tests. There is no test module, no #[cfg(test)] and no test helpers in this file.
- convert.rs: no tests.
- No integration test or runtime probe for the decoder exists in this unit. Hardware availability is checkable only at runtime through can_decode and can_decode_hw (decode.rs:518-526), which require a live Media Foundation runtime and cannot run on a headless CI image without a GPU or a software MFT.
