//! Media Foundation hardware H.264/HEVC encoder, zero-copy from a shared D3D11
//! texture (Plan 04 §5b). Implements the plan's low-latency recipe exactly:
//! async HW MFT unlocked, D3D manager set for GPU input, NV12 in, and the
//! `ICodecAPI`/attribute settings that keep glass-to-glass latency at the floor
//! (zero B-frames, CBR, single-slice, effectively-infinite GOP, low-latency
//! mode). Keyframe strategy is LTR-recovery with forced IDR reserved for connect
//! / total desync.

use std::sync::Once;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Media::MediaFoundation::*;

use crate::variant::{boolv, u32v};
use crate::{Codec, EncodedFrame, Error};

static MF_INIT: Once = Once::new();

pub(crate) fn ensure_mf_startup() {
    MF_INIT.call_once(|| {
        // SAFETY: idempotent process-wide init. MFSTARTUP_LITE is enough for MFTs.
        let _ = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) };
    });
}

/// Encoder parameters (§5b). Frame rate is a ratio; bitrate is the current BWE
/// target fed from the transport (§6 adaptive bitrate).
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    pub codec: Codec,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            codec: Codec::H264,
        }
    }
}

/// A configured MF encoder bound to the shared D3D11 device. Hardware MFTs run
/// the async event pump with zero-copy GPU input; the software fallback (the
/// Microsoft H.264 encoder, present on every Windows) is a sync MFT fed CPU NV12
/// read back from the GPU converter — used when a machine's hardware encoder for
/// the negotiated codec is missing or broken.
pub struct Encoder {
    transform: IMFTransform,
    /// Some for async (hardware) MFTs; sync software MFTs have no generator.
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    cfg: EncoderConfig,
    _dxgi_mgr: Option<IMFDXGIDeviceManager>,
    frame_index: i64,
    /// GPU BGRA→NV12 converter (§5b — DDA gives BGRA, the encoder wants NV12).
    converter: crate::convert::Converter,
    /// Software path: staging NV12 texture for GPU→CPU readback (lazy).
    software: bool,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    staging: Option<ID3D11Texture2D>,
}

impl Encoder {
    /// Enumerate, instantiate and configure the MFT for `cfg.codec`, sharing
    /// `device` with capture so input textures need no cross-device copy (§5c).
    /// If the hardware path fails **anywhere** during construction (some machines
    /// have HW MFTs that enumerate but die on activation/configuration with
    /// E_UNEXPECTED), retry the whole build against the software encoder.
    pub fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, Error> {
        ensure_mf_startup();
        match Self::build(device, cfg, true) {
            Ok(enc) => Ok(enc),
            Err(e) => {
                tracing::warn!(
                    "hardware {:?} encoder failed ({e}) — trying the software encoder",
                    cfg.codec
                );
                Self::build(device, cfg, false)
            }
        }
    }

    fn build(device: &ID3D11Device, cfg: EncoderConfig, is_hw: bool) -> Result<Self, Error> {
        let subtype = output_subtype(cfg.codec);

        // §5b enumerate/instantiate, matched on the desired output subtype.
        let flags = if is_hw {
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
        } else {
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
        };
        let transform =
            enumerate_encoder_with(subtype, flags)?.ok_or(Error::NoEncoder(cfg.codec))?;
        if !is_hw {
            tracing::warn!(
                "no hardware {:?} encoder — using the software encoder (CPU)",
                cfg.codec
            );
        }

        // Unlock async + request low latency on the transform attributes (§5b).
        // Sync software MFTs have no async contract — skip the unlock there.
        // SAFETY: freshly-activated transform.
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                if is_hw {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                }
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }

        // Zero-copy GPU input: bind a DXGI device manager wrapping the shared
        // device (§5b). Software MFTs read CPU memory — no manager.
        let dxgi_mgr = if is_hw {
            let mgr = create_dxgi_manager(device)?;
            // SAFETY: manager is a valid IUnknown; encoder is D3D11-aware.
            unsafe {
                let mgr_ptr = mgr.as_raw() as usize;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr)?;
            }
            Some(mgr)
        } else {
            None
        };

        let (input_id, output_id) = stream_ids(&transform);

        // §5b: set OUTPUT type first (encoders want the compressed type set
        // before the raw input type), then the NV12 input type. Traced per step
        // so a codec-specific rejection is pinpointed in the log rather than
        // surfacing as an opaque HRESULT from `Encoder::new`.
        tracing::debug!("encoder: setting output type ({:?})", cfg.codec);
        set_output_type(&transform, output_id, &cfg, subtype)?;
        tracing::debug!("encoder: setting NV12 input type");
        set_input_type(&transform, input_id, &cfg)?;

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            tracing::debug!("encoder: applying low-latency recipe");
            apply_low_latency_recipe(api, &cfg);
        } else {
            tracing::warn!(
                "encoder exposes no ICodecAPI; low-latency recipe reduced to attributes"
            );
        }

        // Begin streaming (§5b).
        // SAFETY: configured transform.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        // Async (hardware) MFTs drive encode via the event generator; a sync
        // software MFT does not implement it.
        let events: Option<IMFMediaEventGenerator> =
            if is_hw { Some(transform.cast()?) } else { None };
        let converter = crate::convert::Converter::new(device, cfg.width, cfg.height)?;
        // SAFETY: standard immediate-context fetch.
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext()? };

        tracing::info!(
            "encoder ready: {:?} ({}, {}x{})",
            cfg.codec,
            if is_hw { "hardware" } else { "software" },
            cfg.width,
            cfg.height
        );

        Ok(Self {
            transform,
            events,
            codec_api,
            input_id,
            output_id,
            cfg,
            _dxgi_mgr: dxgi_mgr,
            frame_index: 0,
            converter,
            software: !is_hw,
            context,
            device: device.clone(),
            staging: None,
        })
    }

    /// Update the CBR target from the transport's BWE (§6 adaptive bitrate).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<(), Error> {
        self.cfg.bitrate_bps = bps;
        if let Some(api) = &self.codec_api {
            // SAFETY: valid ICodecAPI; VARIANT owned for the call.
            unsafe {
                api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &u32v(bps))?;
            }
        }
        Ok(())
    }

    /// Request an IDR on the next frame (viewer just joined / decoder loss).
    pub fn force_keyframe(&mut self) {
        if let Some(api) = &self.codec_api {
            // SAFETY: valid ICodecAPI.
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &u32v(1)) };
        }
    }

    /// Encode one captured BGRA texture. Converts to NV12 on the GPU (§5b),
    /// wraps the NV12 surface as a zero-copy sample, drives the async MFT event
    /// loop (`METransformNeedInput` → `ProcessInput`, `METransformHaveOutput` →
    /// `ProcessOutput`), and returns any compressed access units produced.
    pub fn encode(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<EncodedFrame>, Error> {
        let duration = 10_000_000i64 * self.cfg.fps_den as i64 / self.cfg.fps_num.max(1) as i64;
        let sample_time = self.frame_index * duration;
        self.frame_index += 1;

        // BGRA (DDA) → NV12 (encoder input) on the GPU.
        let nv12 = self.converter.convert_to_nv12(texture)?;
        let sample = if self.software {
            // Software MFT reads CPU memory: read the NV12 back and wrap it.
            let nv12 = nv12.clone();
            self.readback_nv12_sample(&nv12, sample_time, duration)?
        } else {
            wrap_texture_sample(nv12, sample_time, duration)?
        };

        if self.events.is_none() {
            return self.encode_sync(&sample);
        }

        let mut out = Vec::new();
        let mut fed = false;
        // Pump events until the encoder has consumed the input and drained the
        // output it wants to emit for it. HW MFTs are event-driven, so we react
        // to the generator rather than blindly calling ProcessOutput.
        loop {
            // SAFETY: valid event generator. Flag 0 blocks until the next event
            // (NeedInput → HaveOutput) — correct for the low-latency 1:1 pump.
            let event = unsafe {
                self.events
                    .as_ref()
                    .expect("async pump requires event generator")
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?
            };
            // GetType yields the raw MediaEventType; compare against the MFT
            // event constants by value (bare idents in `match` would bind, not
            // compare).
            let met = MF_EVENT_TYPE(unsafe { event.GetType()? } as i32);
            if met == METransformNeedInput {
                if !fed {
                    // SAFETY: sample built above; input id valid.
                    unsafe { self.transform.ProcessInput(self.input_id, &sample, 0)? };
                    fed = true;
                } else {
                    // Encoder wants the next frame; we have none this call.
                    break;
                }
            } else if met == METransformHaveOutput {
                if let Some(frame) = self.drain_output()? {
                    out.push(frame);
                }
            }
            if fed && !out.is_empty() {
                // Got at least one AU for this input; typical low-latency 1:1.
                break;
            }
        }
        Ok(out)
    }

    /// Sync (software) MFT pump: feed the input, then drain until the encoder
    /// asks for more input. The MS software H.264 encoder is 1-in/1-out at
    /// steady state, with a possible short warm-up delay.
    fn encode_sync(&mut self, sample: &IMFSample) -> Result<Vec<EncodedFrame>, Error> {
        // SAFETY: configured sync transform; sample owned by caller.
        unsafe {
            match self.transform.ProcessInput(self.input_id, sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // Output pending — drain, then retry the input once.
                    let mut out = Vec::new();
                    while let Some(f) = self.drain_output()? {
                        out.push(f);
                    }
                    self.transform.ProcessInput(self.input_id, sample, 0)?;
                    while let Some(f) = self.drain_output()? {
                        out.push(f);
                    }
                    return Ok(out);
                }
                Err(e) => return Err(Error::Win(e)),
            }
        }
        let mut out = Vec::new();
        while let Some(f) = self.drain_output()? {
            out.push(f);
        }
        Ok(out)
    }

    /// Read the GPU NV12 conversion result back to CPU memory and wrap it in an
    /// `IMFSample` for a software encoder. Staging texture is created lazily and
    /// reused; rows are tightly packed (stride = width) as MF expects for NV12
    /// with no MF_MT_DEFAULT_STRIDE override.
    fn readback_nv12_sample(
        &mut self,
        nv12: &ID3D11Texture2D,
        time: i64,
        duration: i64,
    ) -> Result<IMFSample, Error> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.cfg.width,
                Height: self.cfg.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: valid device + desc.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            self.staging = tex;
        }
        let staging = self
            .staging
            .clone()
            .ok_or(Error::NoEncoder(self.cfg.codec))?;

        let total = w * h * 3 / 2;
        // SAFETY: GPU copy + CPU map of the staging texture; unmapped before use.
        unsafe {
            self.context.CopyResource(&staging, nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;

            let buffer = MFCreateMemoryBuffer(total as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            // Y plane rows, then interleaved UV rows (chroma at pitch*h).
            for y in 0..h {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * w), w);
            }
            let src_uv = src.add(pitch * h);
            let dst_uv = dst.add(w * h);
            for y in 0..(h / 2) {
                std::ptr::copy_nonoverlapping(src_uv.add(y * pitch), dst_uv.add(y * w), w);
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(total as u32)?;
            self.context.Unmap(&staging, 0);

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            Ok(sample)
        }
    }

    /// Pull one compressed sample via `ProcessOutput` and copy its bytes out.
    /// When the MFT does not provide its own output samples (software encoders),
    /// hand it one sized from `GetOutputStreamInfo`.
    fn drain_output(&self) -> Result<Option<EncodedFrame>, Error> {
        // SAFETY: stream-info query + optional sample/buffer construction.
        let provided: Option<IMFSample> = unsafe {
            let info = self.transform.GetOutputStreamInfo(self.output_id)?;
            if info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0 {
                None
            } else {
                let size = if info.cbSize > 0 {
                    info.cbSize
                } else {
                    // Compressed worst case: a raw frame won't be exceeded.
                    self.cfg.width * self.cfg.height * 3 / 2
                };
                let buffer = if info.cbAlignment > 1 {
                    MFCreateAlignedMemoryBuffer(size, info.cbAlignment - 1)?
                } else {
                    MFCreateMemoryBuffer(size)?
                };
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            }
        };

        let mut out_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: std::mem::ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: single-element output buffer array as the API expects.
        let hr = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut out_buffer), &mut status)
        };
        // SAFETY: taken exactly once (recovers provided or MFT-returned sample).
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut out_buffer.pSample) };
        // Drop any event collection the MFT attached.
        let _ = unsafe { std::mem::ManuallyDrop::take(&mut out_buffer.pEvents) };
        if let Err(e) = hr {
            // MF_E_TRANSFORM_NEED_MORE_INPUT is normal — no output yet.
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            tracing::warn!(
                "encoder: ProcessOutput failed hr={:#010x} status={:#x}",
                e.code().0,
                out_buffer.dwStatus
            );
            return Err(Error::Win(e));
        }

        let Some(sample) = sample else {
            return Ok(None);
        };
        // Keyframe flag: MFSampleExtension_CleanPoint == 1 marks an IDR.
        let keyframe = unsafe {
            sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v == 1)
                .unwrap_or(false)
        };
        let timestamp = unsafe { sample.GetSampleTime().unwrap_or(0) };
        let data = copy_sample_bytes(&sample)?;
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            timestamp,
        }))
    }
}

/// Cheap probe: is there a hardware encoder for `codec` on this machine? Counts
/// `MFTEnumEx` results without activating them (Plan 04 §3 negotiation).
pub fn can_encode(codec: Codec) -> bool {
    ensure_mf_startup();
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: output_subtype(codec),
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: out-array/count pair per MFTEnumEx; we free the array below.
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_info),
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return false;
        }
        if !activates.is_null() {
            // Release each activate ref, then the array.
            let slice = std::slice::from_raw_parts_mut(activates, count as usize);
            for a in slice.iter_mut() {
                let _ = a.take();
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        }
    }
    count > 0
}

/// The codecs this host can encode, in the plan's preference order (§3). H.264
/// is always included: even with no (or a broken) hardware encoder, the
/// Microsoft software H.264 encoder exists on every Windows and `Encoder::new`
/// falls back to it.
pub fn host_encodable() -> Vec<Codec> {
    let mut v = Vec::new();
    for c in [Codec::Av1, Codec::Hevc] {
        if can_encode(c) {
            v.push(c);
        }
    }
    v.push(Codec::H264); // guaranteed baseline (software fallback)
    v
}

/// The MF output subtype GUID for a codec (§5b).
fn output_subtype(codec: Codec) -> windows::core::GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

/// §5b enumerate with explicit flags (hardware-async or software-sync) for the
/// desired output subtype; activate the first result → `IMFTransform`.
fn enumerate_encoder_with(
    subtype: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
) -> Result<Option<IMFTransform>, Error> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: out-array pointer/count pair per the MFTEnumEx contract.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
    }
    if count == 0 || activates.is_null() {
        return Ok(None);
    }
    // SAFETY: MFTEnumEx allocated `count` IMFActivate slots.
    let slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let first = slice[0].clone();
    // Free the array (each retained IMFActivate is kept by clone above).
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };

    let Some(activate) = first else {
        return Ok(None);
    };
    // SAFETY: valid activate object.
    let transform: IMFTransform = unsafe { activate.ActivateObject()? };
    Ok(Some(transform))
}

/// §5b zero-copy input: `MFCreateDXGIDeviceManager` + `ResetDevice(device)`.
pub(crate) fn create_dxgi_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager, Error> {
    let mut token = 0u32;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: standard MF DXGI manager creation.
    unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    }
    let mgr = mgr.ok_or(Error::NoEncoder(Codec::H264))?;
    // SAFETY: device is a valid ID3D11Device (an IUnknown).
    unsafe {
        mgr.ResetDevice(device, token)?;
    }
    Ok(mgr)
}

pub(crate) fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32; 1];
    let mut output = [0u32; 1];
    // GetStreamIDs may return E_NOTIMPL for fixed 0/0 streams — that's fine.
    // SAFETY: buffers sized for a single stream each.
    let _ = unsafe { transform.GetStreamIDs(&mut input, &mut output) };
    (input[0], output[0])
}

/// Compressed OUTPUT media type: subtype + frame size + rate + CBR bitrate +
/// progressive + baseline/high profile (§5b).
fn set_output_type(
    transform: &IMFTransform,
    output_id: u32,
    cfg: &EncoderConfig,
    subtype: windows::core::GUID,
) -> Result<(), Error> {
    // SAFETY: standard MF media-type construction.
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_frame_size(&mt, cfg.width, cfg.height)?;
        set_frame_rate(&mt, cfg.fps_num, cfg.fps_den)?;
        // Some H.264 HW MFTs require an explicit profile in the output type or
        // fail SetOutputType with E_UNEXPECTED. Main profile is universally
        // decodable (including the software fallback decoder). Best-effort: if the
        // MFT infers its own profile, ignore the rejection.
        if subtype == MFVideoFormat_H264 {
            let _ = mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
        }
        transform.SetOutputType(output_id, &mt, 0)?;
    }
    Ok(())
}

/// NV12 INPUT media type (every HW encoder wants NV12 — §5b). The BGRA→NV12
/// conversion is done on-GPU upstream (Video Processor MFT / compute shader).
fn set_input_type(
    transform: &IMFTransform,
    input_id: u32,
    cfg: &EncoderConfig,
) -> Result<(), Error> {
    // SAFETY: standard MF media-type construction.
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_frame_size(&mt, cfg.width, cfg.height)?;
        set_frame_rate(&mt, cfg.fps_num, cfg.fps_den)?;
        transform.SetInputType(input_id, &mt, 0)?;
    }
    Ok(())
}

/// The §5b low-latency recipe via `ICodecAPI::SetValue` — mirror Sunshine.
///
/// **Best-effort per property.** Different HW MFTs validate these knobs
/// differently: some H.264 encoders reject e.g. an effectively-infinite GOP or a
/// specific rate-control value with `E_UNEXPECTED`. A single rejected knob must
/// not abort encoder creation (that black-screens the whole session) — log it and
/// keep the encoder's default for that property.
fn apply_low_latency_recipe(api: &ICodecAPI, cfg: &EncoderConfig) {
    // Single-picture slice, no multi-frame lookahead.
    set_codec_value(
        api,
        &CODECAPI_AVLowLatencyMode,
        &boolv(true),
        "AVLowLatencyMode",
    );
    // CBR rate control, steered live by the adaptive-bitrate controller (§6).
    // This is what makes it SMOOTH instead of stalling: the target bitrate tracks
    // the link's actual throughput (the transport's send-queue depth signal), so
    // the encoder never outruns the link — exactly what the browser's congestion
    // control did automatically in the Electron build. On a slow link the target
    // (and thus per-frame quality) drops, but the picture keeps MOVING; on a fast
    // link it climbs back for sharpness. Resolution is never reduced (the browser's
    // "maintain-resolution"); frame rate flexes via source pacing.
    set_codec_value(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        &u32v(eAVEncCommonRateControlMode_CBR.0 as u32),
        "RateControlMode=CBR",
    );
    set_codec_value(
        api,
        &CODECAPI_AVEncCommonMeanBitRate,
        &u32v(cfg.bitrate_bps),
        "MeanBitRate",
    );
    // Zero B-frames — mandatory (reorder delay would blow the budget).
    set_codec_value(
        api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        &u32v(0),
        "BPictureCount=0",
    );
    // Long GOP: no frequent periodic IDR (LTR recovery instead). Some H.264 MFTs
    // reject u32::MAX, so use a large finite value they accept.
    set_codec_value(api, &CODECAPI_AVEncMPVGOPSize, &u32v(600), "GOPSize");
    // Favor SPEED over quality-per-bit (0=fastest, 100=best). Matters most for
    // the software encoder, where per-frame CPU time IS the latency floor; a
    // hardware MFT largely ignores it. Best-effort like the rest.
    set_codec_value(
        api,
        &CODECAPI_AVEncCommonQualityVsSpeed,
        &u32v(33),
        "QualityVsSpeed=33",
    );
}

/// Set one `ICodecAPI` value, logging (not failing) if the MFT rejects it.
fn set_codec_value(
    api: &ICodecAPI,
    name: *const windows::core::GUID,
    var: &windows::Win32::System::Variant::VARIANT,
    label: &str,
) {
    // SAFETY: valid ICodecAPI; `var` is a well-formed VARIANT owned by the caller.
    if let Err(e) = unsafe { api.SetValue(name, var) } {
        tracing::warn!("encoder: ICodecAPI {label} rejected ({e}) — keeping default");
    }
}

// --- small MF helpers ---------------------------------------------------------

fn set_frame_size(mt: &IMFMediaType, w: u32, h: u32) -> Result<(), Error> {
    // MF packs width<<32 | height into MF_MT_FRAME_SIZE.
    let packed = ((w as u64) << 32) | h as u64;
    // SAFETY: valid media type.
    unsafe { mt.SetUINT64(&MF_MT_FRAME_SIZE, packed)? };
    Ok(())
}

fn set_frame_rate(mt: &IMFMediaType, num: u32, den: u32) -> Result<(), Error> {
    let packed = ((num as u64) << 32) | den as u64;
    // SAFETY: valid media type.
    unsafe { mt.SetUINT64(&MF_MT_FRAME_RATE, packed)? };
    Ok(())
}

/// Wrap a shared D3D11 texture as an `IMFSample` (§5b: `MFCreateDXGISurfaceBuffer`
/// → `MFCreateSample` + `AddBuffer` + set time/duration). No CPU copy.
fn wrap_texture_sample(
    texture: &ID3D11Texture2D,
    time: i64,
    duration: i64,
) -> Result<IMFSample, Error> {
    // SAFETY: valid texture; subresource 0, top-down.
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// Copy a compressed sample's contiguous bytes into a `Vec` for packetization.
fn copy_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, Error> {
    // SAFETY: sample from ProcessOutput has at least one buffer.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        let out = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        buffer.Unlock()?;
        Ok(out)
    }
}
