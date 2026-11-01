//! NVIDIA NVENC encoder (feature `nvenc`), bound at run time.
//!
//! `libnvidia-encode.so.1` and `libcuda.so.1` ship with the proprietary driver
//! and are `dlopen`ed — nothing is linked and no CUDA toolkit or Video Codec SDK
//! is needed to build (decision #5). Missing libraries, an old driver or no
//! NVIDIA GPU all surface as a probe failure with the reason, and the factory
//! moves on to VA-API.
//!
//! Recipe (mirrors the Media Foundation one in `codec/src/encode.rs`):
//! * preset **P3** + tuning **ULTRA_LOW_LATENCY** — the slowest/best of the
//!   three low-latency presets; desktop content is mostly static, so the GPU has
//!   the time and the bits go to legibility (same trade as Windows'
//!   `QualityVsSpeed=66`). ULL already means no B-frames, no lookahead and
//!   zero reorder delay; those are forced explicitly anyway.
//! * **CBR** at the live bitrate, single-frame VBV, no filler data (a static
//!   screen must cost nothing, not stuffing bits), max-QP floor 32 on every
//!   picture type, spatial AQ on.
//! * Infinite GOP / IDR period: keyframes only when the engine forces one
//!   (`NV_ENC_PIC_FLAG_FORCEIDR | OUTPUT_SPSPPS`); `repeatSPSPPS` as well, and
//!   the access unit is checked anyway (see `annexb`).
//! * One slice, H.264 Main (CABAC) or HEVC Main, BT.709 limited-range VUI.
//! * Bitrate changes go through `nvEncReconfigureEncoder` without a reset or
//!   an IDR.
//!
//! Input v1: the CPU frame is copied into a pitched CUDA allocation registered
//! with NVENC once (`NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`). BGRA goes in as
//! `NV_ENC_BUFFER_FORMAT_ARGB` and NVENC converts to YUV itself; NV12 goes in
//! as is. NVENC cannot scale, so a frame larger than the encode size is
//! resampled on the CPU first (`scale`).
//!
//! ## References
//! * NVIDIA Video Codec SDK 12.0 — *NVENC Video Encoder API Programming Guide*
//!   (§"Setting up hardware for encoding" … "Low-latency use cases":
//!   infinite GOP, CBR, single-frame VBV, `zeroReorderDelay`, P1–P3 + ULL).
//! * `nvEncodeAPI.h` / `dynlink_cuda.h`, FFmpeg nv-codec-headers n12.0.16.1 (MIT).
//! * FFmpeg `libavcodec/nvenc.c` (dynamic loading, CUDA device-pointer input,
//!   reconfigure-for-bitrate); Sunshine `src/nvenc/nvenc_base.cpp` (low-latency
//!   parameter choices for game/desktop streaming).

mod cuda;
mod ffi;

use std::ffi::{c_void, CStr};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use libloading::Library;
use platform_api::{
    Codec, CodecError, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat, VideoEncoder,
};

use self::cuda::{Context, Cuda, DeviceFrame};
use self::ffi::*;
use super::scale::{self, Plane};
use super::zeroed::Zeroed;
use super::{annexb, vbv_bits, MAX_QP};

/// Oldest NVENC API the binding is written against (`ffi` struct versions).
const MIN_API: (u32, u32) = (API_MAJOR, API_MINOR);
/// GPU used for encoding. Multi-GPU hosts encode on the first CUDA device,
/// which is the one driving the primary display in the common laptop/desktop
/// setups.
const CUDA_DEVICE: i32 = 0;

/// What the probe found: device name and which codecs it encodes.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    pub device: String,
    pub api: (u32, u32),
    pub h264: bool,
    pub hevc: bool,
}

/// One full NVENC capability probe (driver libs, GPU, session, codec GUIDs).
/// Takes ~100–300 ms; the factory runs it once.
pub(crate) fn probe() -> Result<Probe, String> {
    let api = Api::shared()?;
    let cuda = shared_cuda()?;
    let ctx = Rc::new(Context::new(&cuda, CUDA_DEVICE)?);
    let session = Session::open(&api, &ctx)?;
    let guids = session.encode_guids()?;
    let probe = Probe {
        device: ctx.device_name().to_string(),
        api: api.max_version,
        h264: guids.contains(&NV_ENC_CODEC_H264_GUID),
        hevc: guids.contains(&NV_ENC_CODEC_HEVC_GUID),
    };
    if !probe.h264 && !probe.hevc {
        return Err(format!("{} has no H.264/HEVC encoder", probe.device));
    }
    Ok(probe)
}

/// The NVENC entry points this encoder calls, resolved and checked once.
struct Fns {
    get_encode_guid_count: unsafe extern "C" fn(*mut c_void, *mut u32) -> NvEncStatus,
    get_encode_guids: unsafe extern "C" fn(*mut c_void, *mut Guid, u32, *mut u32) -> NvEncStatus,
    get_encode_caps:
        unsafe extern "C" fn(*mut c_void, Guid, *mut NvEncCapsParam, *mut i32) -> NvEncStatus,
    get_encode_preset_config_ex:
        unsafe extern "C" fn(*mut c_void, Guid, Guid, u32, *mut NvEncPresetConfig) -> NvEncStatus,
    initialize_encoder:
        unsafe extern "C" fn(*mut c_void, *mut NvEncInitializeParams) -> NvEncStatus,
    reconfigure_encoder:
        unsafe extern "C" fn(*mut c_void, *mut NvEncReconfigureParams) -> NvEncStatus,
    create_bitstream_buffer:
        unsafe extern "C" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NvEncStatus,
    destroy_bitstream_buffer: unsafe extern "C" fn(*mut c_void, *mut c_void) -> NvEncStatus,
    register_resource: unsafe extern "C" fn(*mut c_void, *mut NvEncRegisterResource) -> NvEncStatus,
    unregister_resource: unsafe extern "C" fn(*mut c_void, *mut c_void) -> NvEncStatus,
    map_input_resource:
        unsafe extern "C" fn(*mut c_void, *mut NvEncMapInputResource) -> NvEncStatus,
    unmap_input_resource: unsafe extern "C" fn(*mut c_void, *mut c_void) -> NvEncStatus,
    encode_picture: unsafe extern "C" fn(*mut c_void, *mut NvEncPicParams) -> NvEncStatus,
    lock_bitstream: unsafe extern "C" fn(*mut c_void, *mut NvEncLockBitstream) -> NvEncStatus,
    unlock_bitstream: unsafe extern "C" fn(*mut c_void, *mut c_void) -> NvEncStatus,
    get_sequence_params:
        unsafe extern "C" fn(*mut c_void, *mut NvEncSequenceParamPayload) -> NvEncStatus,
    destroy_encoder: unsafe extern "C" fn(*mut c_void) -> NvEncStatus,
    open_encode_session_ex:
        unsafe extern "C" fn(*mut NvEncOpenEncodeSessionExParams, *mut *mut c_void) -> NvEncStatus,
    get_last_error_string: unsafe extern "C" fn(*mut c_void) -> *const std::ffi::c_char,
}

/// `libnvidia-encode.so.1`, its function table and the driver's API version.
struct Api {
    _lib: Library,
    fns: Fns,
    max_version: (u32, u32),
}

// SAFETY: immutable function pointers + the library handle; NVENC sessions
// are independent objects, each used from one thread at a time.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    fn shared() -> Result<Arc<Self>, String> {
        static API: OnceLock<Result<Arc<Api>, String>> = OnceLock::new();
        API.get_or_init(|| Self::load().map(Arc::new)).clone()
    }

    fn load() -> Result<Self, String> {
        // SAFETY: loading the driver's NVENC library runs only its initialisers.
        let lib = unsafe { Library::new("libnvidia-encode.so.1") }.map_err(|e| {
            format!("libnvidia-encode.so.1 not loadable ({e}) — no NVIDIA driver with NVENC")
        })?;
        // SAFETY: documented signatures (nvEncodeAPI.h); copied out while `lib`
        // is alive and kept alive alongside the copies.
        let (get_max, create): (GetMaxSupportedVersionFn, CreateInstanceFn) = unsafe {
            (
                *lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0")
                    .map_err(|e| format!("NvEncodeAPIGetMaxSupportedVersion missing ({e})"))?,
                *lib.get(b"NvEncodeAPICreateInstance\0")
                    .map_err(|e| format!("NvEncodeAPICreateInstance missing ({e})"))?,
            )
        };
        let mut packed = 0u32;
        // SAFETY: out-pointer to a local.
        let status = unsafe { get_max(&mut packed) };
        if status != NV_ENC_SUCCESS {
            return Err(format!(
                "NvEncodeAPIGetMaxSupportedVersion: {}",
                status_name(status)
            ));
        }
        let max_version = unpack_max_version(packed);
        if max_version < MIN_API {
            return Err(format!(
                "the NVIDIA driver supports NVENC API {}.{}; {}.{} is needed (driver R520 or newer)",
                max_version.0, max_version.1, MIN_API.0, MIN_API.1
            ));
        }
        let mut list = NvEncodeApiFunctionList::zeroed();
        list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        // SAFETY: a zeroed, versioned function list the driver fills in.
        let status = unsafe { create(&mut list) };
        if status != NV_ENC_SUCCESS {
            return Err(format!(
                "NvEncodeAPICreateInstance: {}",
                status_name(status)
            ));
        }
        macro_rules! need {
            ($field:ident) => {
                list.$field.ok_or(concat!(
                    "the NVENC driver left ",
                    stringify!($field),
                    " unset"
                ))?
            };
        }
        let fns = Fns {
            get_encode_guid_count: need!(get_encode_guid_count),
            get_encode_guids: need!(get_encode_guids),
            get_encode_caps: need!(get_encode_caps),
            get_encode_preset_config_ex: need!(get_encode_preset_config_ex),
            initialize_encoder: need!(initialize_encoder),
            reconfigure_encoder: need!(reconfigure_encoder),
            create_bitstream_buffer: need!(create_bitstream_buffer),
            destroy_bitstream_buffer: need!(destroy_bitstream_buffer),
            register_resource: need!(register_resource),
            unregister_resource: need!(unregister_resource),
            map_input_resource: need!(map_input_resource),
            unmap_input_resource: need!(unmap_input_resource),
            encode_picture: need!(encode_picture),
            lock_bitstream: need!(lock_bitstream),
            unlock_bitstream: need!(unlock_bitstream),
            get_sequence_params: need!(get_sequence_params),
            destroy_encoder: need!(destroy_encoder),
            open_encode_session_ex: need!(open_encode_session_ex),
            get_last_error_string: need!(get_last_error_string),
        };
        Ok(Self {
            _lib: lib,
            fns,
            max_version,
        })
    }
}

fn shared_cuda() -> Result<Arc<Cuda>, String> {
    static CUDA: OnceLock<Result<Arc<Cuda>, String>> = OnceLock::new();
    CUDA.get_or_init(Cuda::load).clone()
}

/// An open NVENC session on a CUDA context; destroyed on drop.
struct Session {
    api: Arc<Api>,
    /// `Rc`: an encoder lives on the one thread that created it.
    ctx: Rc<Context>,
    enc: *mut c_void,
}

impl Session {
    fn open(api: &Arc<Api>, ctx: &Rc<Context>) -> Result<Self, String> {
        let _current = ctx.current()?;
        let mut params = NvEncOpenEncodeSessionExParams::zeroed();
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = NV_ENC_DEVICE_TYPE_CUDA;
        params.device = ctx.raw();
        params.api_version = API_VERSION;
        let mut enc: *mut c_void = std::ptr::null_mut();
        // SAFETY: versioned params; out-pointer to a local.
        let status = unsafe { (api.fns.open_encode_session_ex)(&mut params, &mut enc) };
        if status != NV_ENC_SUCCESS || enc.is_null() {
            // A failed open still returns a handle that must be destroyed.
            if !enc.is_null() {
                // SAFETY: the handle the failed call returned.
                unsafe { (api.fns.destroy_encoder)(enc) };
            }
            return Err(format!(
                "nvEncOpenEncodeSessionEx on {}: {} (all NVENC sessions busy, or the GPU has no encoder)",
                ctx.device_name(),
                status_name(status)
            ));
        }
        Ok(Self {
            api: api.clone(),
            ctx: ctx.clone(),
            enc,
        })
    }

    fn fns(&self) -> &Fns {
        &self.api.fns
    }

    /// `status` → `Err` naming the call, the status and the driver's own detail.
    fn check(&self, status: NvEncStatus, what: &str) -> Result<(), String> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        // SAFETY: valid session; the driver returns a NUL-terminated string it
        // owns (or null).
        let detail = unsafe { (self.fns().get_last_error_string)(self.enc) };
        let detail = if detail.is_null() {
            String::new()
        } else {
            // SAFETY: non-null, NUL-terminated, valid until the next NVENC call.
            let s = unsafe { CStr::from_ptr(detail) }.to_string_lossy();
            if s.is_empty() {
                String::new()
            } else {
                format!(" — {s}")
            }
        };
        Err(format!("{what}: {}{detail}", status_name(status)))
    }

    fn encode_guids(&self) -> Result<Vec<Guid>, String> {
        let mut count = 0u32;
        // SAFETY: out-pointer to a local.
        self.check(
            unsafe { (self.fns().get_encode_guid_count)(self.enc, &mut count) },
            "nvEncGetEncodeGUIDCount",
        )?;
        let mut guids = vec![
            Guid {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8]
            };
            count as usize
        ];
        let mut written = 0u32;
        // SAFETY: buffer of `count` GUIDs.
        self.check(
            unsafe {
                (self.fns().get_encode_guids)(self.enc, guids.as_mut_ptr(), count, &mut written)
            },
            "nvEncGetEncodeGUIDs",
        )?;
        guids.truncate(written as usize);
        Ok(guids)
    }

    fn cap(&self, codec: Guid, cap: u32) -> Result<i32, String> {
        let mut param = NvEncCapsParam::zeroed();
        param.version = NV_ENC_CAPS_PARAM_VER;
        param.caps_to_query = cap;
        let mut value = 0i32;
        // SAFETY: versioned param; out-pointer to a local.
        self.check(
            unsafe { (self.fns().get_encode_caps)(self.enc, codec, &mut param, &mut value) },
            "nvEncGetEncodeCaps",
        )?;
        Ok(value)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Destroying a CUDA-device session wants its context current.
        let _current = self.ctx.current();
        // SAFETY: we own the session; every buffer/registration made on it was
        // released by `NvencEncoder::drop` first.
        unsafe { (self.api.fns.destroy_encoder)(self.enc) };
    }
}

fn codec_guids(codec: Codec) -> Result<(Guid, Guid), CodecError> {
    match codec {
        Codec::H264 => Ok((NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_MAIN_GUID)),
        Codec::Hevc => Ok((NV_ENC_CODEC_HEVC_GUID, NV_ENC_HEVC_PROFILE_MAIN_GUID)),
        Codec::Av1 => Err(CodecError::NoEncoder(Codec::Av1)),
    }
}

fn set_rate(rc: &mut NvEncRcParams, cfg: &EncoderConfig) {
    rc.average_bit_rate = cfg.bitrate_bps;
    // Ignored in CBR; kept equal so a driver that reads it anyway agrees.
    rc.max_bit_rate = cfg.bitrate_bps;
    rc.vbv_buffer_size = vbv_bits(cfg.bitrate_bps, cfg.fps_num, cfg.fps_den);
    rc.vbv_initial_delay = rc.vbv_buffer_size;
}

fn bt709_vui(vui: &mut NvEncVuiParameters) {
    vui.video_signal_type_present_flag = 1;
    vui.video_format = VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.video_full_range_flag = 0;
    vui.colour_description_present_flag = 1;
    vui.colour_primaries = VUI_BT709;
    vui.transfer_characteristics = VUI_BT709;
    vui.colour_matrix = VUI_BT709;
    // max_num_reorder_frames = 0 / max_dec_frame_buffering = 1 in the stream,
    // so decoders output each picture immediately.
    vui.bitstream_restriction_flag = 1;
}

/// The low-latency recipe applied on top of the P3/ULL preset configuration.
fn apply_recipe(config: &mut NvEncConfig, cfg: &EncoderConfig) {
    config.version = NV_ENC_CONFIG_VER;
    config.gop_length = NVENC_INFINITE_GOPLENGTH;
    // IPPP… — mandatory with an infinite GOP, and no B-frame reorder delay.
    config.frame_interval_p = 1;
    let rc = &mut config.rc_params;
    rc.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
    set_rate(rc, cfg);
    rc.flags |= RC_ENABLE_MAX_QP | RC_ENABLE_AQ | RC_ZERO_REORDER_DELAY;
    // Lookahead buffers frames (latency); temporal AQ needs lookahead.
    rc.flags &= !(RC_ENABLE_LOOKAHEAD | RC_ENABLE_TEMPORAL_AQ);
    rc.lookahead_depth = 0;
    // QUALITY FLOOR — same knob and value as Windows (`codec::encode::MAX_QP`):
    // overshoot the bitrate rather than blur text; the engine paces frames.
    rc.max_qp = NvEncQp {
        qp_inter_p: MAX_QP,
        qp_inter_b: MAX_QP,
        qp_intra: MAX_QP,
    };
    // `multiPass` stays as the preset set it: NVENC's two-pass modes refine
    // within the current frame and add no frame of latency.
    match cfg.codec {
        Codec::Hevc => {
            config.profile_guid = NV_ENC_HEVC_PROFILE_MAIN_GUID;
            // SAFETY: the preset was fetched for the HEVC GUID, so `hevc` is the
            // initialised member of the union.
            let hevc = unsafe { &mut config.encode_codec_config.hevc };
            hevc.idr_period = NVENC_INFINITE_GOPLENGTH;
            hevc.flags |= HEVC_REPEAT_SPSPPS;
            hevc.flags &= !(HEVC_OUTPUT_AUD
                | HEVC_DISABLE_SPSPPS
                | HEVC_ENABLE_INTRA_REFRESH
                | HEVC_ENABLE_FILLER_DATA);
            hevc.slice_mode = 3;
            hevc.slice_mode_data = 1;
            bt709_vui(&mut hevc.vui);
        }
        _ => {
            config.profile_guid = NV_ENC_H264_PROFILE_MAIN_GUID;
            // SAFETY: as above, for the H.264 GUID.
            let h264 = unsafe { &mut config.encode_codec_config.h264 };
            h264.idr_period = NVENC_INFINITE_GOPLENGTH;
            h264.flags |= H264_REPEAT_SPSPPS;
            h264.flags &= !(H264_OUTPUT_AUD
                | H264_DISABLE_SPSPPS
                | H264_ENABLE_INTRA_REFRESH
                | H264_ENABLE_FILLER_DATA);
            h264.entropy_coding_mode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
            // sliceMode 3 = "N slices per picture", N = 1.
            h264.slice_mode = 3;
            h264.slice_mode_data = 1;
            bt709_vui(&mut h264.vui);
        }
    }
}

/// The registered CUDA input surface for one pixel format.
struct Input {
    frame: DeviceFrame,
    registered: *mut c_void,
    format: u32,
}

pub(crate) struct NvencEncoder {
    session: Session,
    input: Option<Input>,
    bitstream: *mut c_void,
    /// Both boxed: `init.encode_config` points at `config`, and the driver
    /// reads `init` again on every reconfigure.
    config: Box<NvEncConfig>,
    init: Box<NvEncInitializeParams>,
    cfg: EncoderConfig,
    /// The session's SPS/PPS(/VPS), Annex-B, for keyframes that lack them.
    headers: Vec<u8>,
    /// CPU resampling target when the frame is not the encode size.
    scaled: Vec<u8>,
    force_idr: bool,
    frame_idx: u32,
}

impl NvencEncoder {
    pub(crate) fn new(cfg: EncoderConfig) -> Result<Self, CodecError> {
        let backend = |e: String| CodecError::Backend(format!("NVENC: {e}"));
        let (codec_guid, _) = codec_guids(cfg.codec)?;
        let api = Api::shared().map_err(backend)?;
        let cuda = shared_cuda().map_err(backend)?;
        let ctx = Rc::new(Context::new(&cuda, CUDA_DEVICE).map_err(backend)?);
        let session = Session::open(&api, &ctx).map_err(backend)?;
        let _current = ctx.current().map_err(backend)?;

        if !session
            .encode_guids()
            .map_err(backend)?
            .contains(&codec_guid)
        {
            return Err(CodecError::NoEncoder(cfg.codec));
        }
        let max_w = session
            .cap(codec_guid, NV_ENC_CAPS_WIDTH_MAX)
            .map_err(backend)?;
        let max_h = session
            .cap(codec_guid, NV_ENC_CAPS_HEIGHT_MAX)
            .map_err(backend)?;
        if cfg.width as i64 > max_w as i64 || cfg.height as i64 > max_h as i64 {
            return Err(CodecError::Unsupported(format!(
                "NVENC on {} encodes {:?} up to {max_w}x{max_h}, not {}x{}",
                ctx.device_name(),
                cfg.codec,
                cfg.width,
                cfg.height
            )));
        }

        let mut preset = Box::new(NvEncPresetConfig::zeroed());
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.preset_cfg.version = NV_ENC_CONFIG_VER;
        // SAFETY: versioned, zeroed struct the driver fills in.
        session
            .check(
                unsafe {
                    (session.fns().get_encode_preset_config_ex)(
                        session.enc,
                        codec_guid,
                        NV_ENC_PRESET_P3_GUID,
                        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                        &mut *preset,
                    )
                },
                "nvEncGetEncodePresetConfigEx",
            )
            .map_err(backend)?;
        let mut config = Box::new(preset.preset_cfg);
        apply_recipe(&mut config, &cfg);

        let mut init = Box::new(NvEncInitializeParams::zeroed());
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encode_guid = codec_guid;
        init.preset_guid = NV_ENC_PRESET_P3_GUID;
        init.tuning_info = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encode_width = cfg.width;
        init.encode_height = cfg.height;
        init.dar_width = cfg.width;
        init.dar_height = cfg.height;
        // MUST be the real feed rate (see `EncoderConfig::fps_num`).
        init.frame_rate_num = cfg.fps_num.max(1);
        init.frame_rate_den = cfg.fps_den.max(1);
        // Synchronous: `nvEncLockBitstream` right after `nvEncEncodePicture`
        // returns this frame's bits — one frame in flight, nothing queued.
        init.enable_encode_async = 0;
        // The driver picks picture types (needed for FORCEIDR).
        init.enable_ptd = 1;
        init.encode_config = &mut *config;
        // SAFETY: fully initialised params; `config` outlives the session.
        session
            .check(
                unsafe { (session.fns().initialize_encoder)(session.enc, &mut *init) },
                "nvEncInitializeEncoder",
            )
            .map_err(backend)?;

        let mut create = NvEncCreateBitstreamBuffer::zeroed();
        create.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        // SAFETY: versioned struct; the driver sizes the buffer itself.
        session
            .check(
                unsafe { (session.fns().create_bitstream_buffer)(session.enc, &mut create) },
                "nvEncCreateBitstreamBuffer",
            )
            .map_err(backend)?;

        let mut enc = Self {
            session,
            input: None,
            bitstream: create.bitstream_buffer,
            config,
            init,
            cfg,
            headers: Vec::new(),
            scaled: Vec::new(),
            force_idr: true,
            frame_idx: 0,
        };
        enc.headers = enc.sequence_headers().map_err(backend)?;
        tracing::info!(
            "NVENC encoder: {} {:?} {}x{} @{}/{} fps, {} kbps, P3 + ultra-low-latency, CBR, max QP {MAX_QP}",
            ctx.device_name(),
            cfg.codec,
            cfg.width,
            cfg.height,
            cfg.fps_num,
            cfg.fps_den,
            cfg.bitrate_bps / 1000
        );
        Ok(enc)
    }

    fn sequence_headers(&self) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; 1024];
        let mut size = 0u32;
        let mut payload = NvEncSequenceParamPayload::zeroed();
        payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        payload.in_buffer_size = buf.len() as u32;
        payload.spspps_buffer = buf.as_mut_ptr().cast();
        payload.out_spspps_payload_size = &mut size;
        // SAFETY: buffer and size pointer outlive the call.
        self.session.check(
            unsafe { (self.session.fns().get_sequence_params)(self.session.enc, &mut payload) },
            "nvEncGetSequenceParams",
        )?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// Make `self.input` the registered surface for `format`, (re)creating it
    /// when the format changes.
    fn ensure_input(&mut self, current: &cuda::Current<'_>, format: u32) -> Result<(), String> {
        if self.input.as_ref().is_some_and(|i| i.format != format) {
            self.release_input();
        }
        if self.input.is_none() {
            let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
            let (width_bytes, rows) = if format == NV_ENC_BUFFER_FORMAT_NV12 {
                (w, h + h.div_ceil(2))
            } else {
                (w * 4, h)
            };
            let frame = DeviceFrame::alloc(current, width_bytes, rows)?;
            let mut reg = NvEncRegisterResource::zeroed();
            reg.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg.resource_type = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            reg.width = self.cfg.width;
            reg.height = self.cfg.height;
            reg.pitch = frame.pitch as u32;
            reg.resource_to_register = frame.ptr as *mut c_void;
            reg.buffer_format = format;
            reg.buffer_usage = NV_ENC_INPUT_IMAGE;
            // SAFETY: versioned struct describing our live allocation.
            self.session.check(
                unsafe { (self.session.fns().register_resource)(self.session.enc, &mut reg) },
                "nvEncRegisterResource",
            )?;
            self.input = Some(Input {
                frame,
                registered: reg.registered_resource,
                format,
            });
        }
        Ok(())
    }

    fn release_input(&mut self) {
        if let Some(input) = self.input.take() {
            // SAFETY: registered on this session and not mapped (every map is
            // unmapped before `encode` returns).
            unsafe { (self.session.fns().unregister_resource)(self.session.enc, input.registered) };
            // `input.frame` frees the CUDA memory as it drops here.
        }
    }

    /// Copy (and if needed resample) the CPU frame into the device surface.
    fn upload(&mut self, current: &cuda::Current<'_>, frame: &Frame<'_>) -> Result<(), CodecError> {
        let FrameData::Cpu {
            format,
            plane0,
            plane1,
        } = frame.data
        else {
            return Err(CodecError::Unsupported(
                "the NVENC encoder takes CPU frames (GPU import is a later task)".into(),
            ));
        };
        let backend = |e: String| CodecError::Backend(format!("NVENC: {e}"));
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        let same = (fw, fh) == (w, h);
        let fmt = match format {
            PixelFormat::Bgra8 => NV_ENC_BUFFER_FORMAT_ARGB,
            PixelFormat::Nv12 => NV_ENC_BUFFER_FORMAT_NV12,
        };
        self.ensure_input(current, fmt).map_err(backend)?;
        // Disjoint field borrows: the device surface and the scaling buffer.
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| backend("no input surface".into()))?;
        let scaled = &mut self.scaled;
        match format {
            PixelFormat::Bgra8 => {
                let (data, stride) = if same {
                    (plane0.data, plane0.stride)
                } else {
                    scaled.resize(w * 4 * h, 0);
                    scale::bilinear(
                        Plane {
                            data: plane0.data,
                            width: fw,
                            height: fh,
                            stride: plane0.stride,
                            channels: 4,
                        },
                        scaled,
                        w,
                        h,
                        w * 4,
                    );
                    (&scaled[..], w * 4)
                };
                input
                    .frame
                    .upload(current, data, stride, w * 4, h, 0)
                    .map_err(backend)?;
            }
            PixelFormat::Nv12 => {
                let uv = plane1.ok_or_else(|| {
                    CodecError::Unsupported("NV12 frame without a UV plane".into())
                })?;
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let (y, y_stride, c, c_stride) = if same {
                    (plane0.data, plane0.stride, uv.data, uv.stride)
                } else {
                    scaled.resize(w * h + cw * 2 * ch, 0);
                    let (y, c) = scaled.split_at_mut(w * h);
                    scale::bilinear(
                        Plane {
                            data: plane0.data,
                            width: fw,
                            height: fh,
                            stride: plane0.stride,
                            channels: 1,
                        },
                        y,
                        w,
                        h,
                        w,
                    );
                    scale::bilinear(
                        Plane {
                            data: uv.data,
                            width: fw.div_ceil(2),
                            height: fh.div_ceil(2),
                            stride: uv.stride,
                            channels: 2,
                        },
                        c,
                        cw,
                        ch,
                        cw * 2,
                    );
                    (&*y, w, &*c, cw * 2)
                };
                input
                    .frame
                    .upload(current, y, y_stride, w, h, 0)
                    .map_err(backend)?;
                input
                    .frame
                    .upload(current, c, c_stride, cw * 2, ch, h)
                    .map_err(backend)?;
            }
        }
        Ok(())
    }

    /// Encode the mapped surface and read the access unit back.
    fn encode_mapped(
        &mut self,
        mapped: *mut c_void,
        buffer_fmt: u32,
    ) -> Result<Option<(Vec<u8>, u32)>, String> {
        let pitch = self.input.as_ref().map_or(0, |i| i.frame.pitch) as u32;
        let mut pic = NvEncPicParams::zeroed();
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.input_width = self.cfg.width;
        pic.input_height = self.cfg.height;
        pic.input_pitch = pitch;
        pic.frame_idx = self.frame_idx;
        pic.input_time_stamp = self.frame_idx as u64;
        pic.input_buffer = mapped;
        pic.output_bitstream = self.bitstream;
        pic.buffer_fmt = buffer_fmt;
        pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
        if self.force_idr {
            pic.encode_pic_flags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        // SAFETY: versioned params referencing our mapped input and bitstream.
        let status = unsafe { (self.session.fns().encode_picture)(self.session.enc, &mut pic) };
        if status == NV_ENC_ERR_NEED_MORE_INPUT {
            // Only possible with reordering, which this session never does;
            // the contract allows "no unit yet" either way.
            return Ok(None);
        }
        self.session.check(status, "nvEncEncodePicture")?;

        let mut lock = NvEncLockBitstream::zeroed();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.output_bitstream = self.bitstream;
        // SAFETY: versioned struct; blocking lock (doNotWait = 0).
        self.session.check(
            unsafe { (self.session.fns().lock_bitstream)(self.session.enc, &mut lock) },
            "nvEncLockBitstream",
        )?;
        let data = if lock.bitstream_buffer_ptr.is_null() {
            Vec::new()
        } else {
            // SAFETY: the driver reports `bitstream_size_in_bytes` valid bytes at
            // `bitstream_buffer_ptr` until the unlock below.
            unsafe {
                std::slice::from_raw_parts(
                    lock.bitstream_buffer_ptr as *const u8,
                    lock.bitstream_size_in_bytes as usize,
                )
            }
            .to_vec()
        };
        let picture_type = lock.picture_type;
        // SAFETY: locked above.
        let unlocked =
            unsafe { (self.session.fns().unlock_bitstream)(self.session.enc, self.bitstream) };
        self.session.check(unlocked, "nvEncUnlockBitstream")?;
        Ok(Some((data, picture_type)))
    }
}

impl VideoEncoder for NvencEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        let backend = |e: String| CodecError::Backend(format!("NVENC: {e}"));
        let ctx = self.session.ctx.clone();
        let current = ctx.current().map_err(backend)?;
        self.upload(&current, frame)?;
        let registered = self
            .input
            .as_ref()
            .map(|i| i.registered)
            .ok_or_else(|| backend("no input surface".into()))?;

        let mut map = NvEncMapInputResource::zeroed();
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registered_resource = registered;
        // SAFETY: versioned struct naming our registered resource.
        self.session
            .check(
                unsafe { (self.session.fns().map_input_resource)(self.session.enc, &mut map) },
                "nvEncMapInputResource",
            )
            .map_err(backend)?;
        let result = self.encode_mapped(map.mapped_resource, map.mapped_buffer_fmt);
        // SAFETY: mapped above; unmapped exactly once whatever the encode did.
        let unmapped = unsafe {
            (self.session.fns().unmap_input_resource)(self.session.enc, map.mapped_resource)
        };
        if unmapped != NV_ENC_SUCCESS {
            tracing::warn!("NVENC: nvEncUnmapInputResource: {}", status_name(unmapped));
        }
        let Some((data, picture_type)) = result.map_err(backend)? else {
            return Ok(Vec::new());
        };
        self.frame_idx = self.frame_idx.wrapping_add(1);
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let keyframe = picture_type == NV_ENC_PIC_TYPE_IDR || annexb::is_idr(self.cfg.codec, &data);
        if keyframe {
            self.force_idr = false;
        }
        let data = if keyframe {
            annexb::with_parameter_sets(self.cfg.codec, data, &self.headers)
        } else {
            data
        };
        Ok(vec![EncodedUnit { data, keyframe }])
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 || bps == self.cfg.bitrate_bps {
            return Ok(());
        }
        let mut next = self.cfg;
        next.bitrate_bps = bps;
        set_rate(&mut self.config.rc_params, &next);
        let mut re = Box::new(NvEncReconfigureParams::zeroed());
        re.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        re.re_init_encode_params = *self.init;
        re.re_init_encode_params.encode_config = &mut *self.config;
        // No resetEncoder / forceIDR: a rate change must not cost a keyframe.
        re.flags = 0;
        let ctx = self.session.ctx.clone();
        let _current = ctx
            .current()
            .map_err(|e| CodecError::Backend(format!("NVENC: {e}")))?;
        // SAFETY: versioned params; `config` is live and owned by `self`.
        let status =
            unsafe { (self.session.fns().reconfigure_encoder)(self.session.enc, &mut *re) };
        match self.session.check(status, "nvEncReconfigureEncoder") {
            Ok(()) => {
                self.cfg = next;
                Ok(())
            }
            Err(e) => {
                // Keep `config` describing what the session actually runs.
                set_rate(&mut self.config.rc_params, &self.cfg);
                Err(CodecError::Backend(format!("NVENC: {e}")))
            }
        }
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let ctx = self.session.ctx.clone();
        let _current = ctx.current();
        self.release_input();
        // SAFETY: created on this session; no lock outstanding.
        unsafe { (self.session.fns().destroy_bitstream_buffer)(self.session.enc, self.bitstream) };
        // `session` then destroys the encoder.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset_like() -> Box<NvEncConfig> {
        let mut c = Box::new(NvEncConfig::zeroed());
        // What a ULL preset may hand back: filler on, AUD on, lookahead set.
        c.gop_length = 250;
        c.frame_interval_p = 3;
        c.rc_params.flags = RC_ENABLE_LOOKAHEAD | RC_ENABLE_TEMPORAL_AQ;
        c.rc_params.lookahead_depth = 8;
        c
    }

    #[test]
    fn recipe_h264() {
        let mut c = preset_like();
        // Writing a union field is safe; the preset may have set any of these.
        c.encode_codec_config.h264.flags =
            H264_ENABLE_FILLER_DATA | H264_OUTPUT_AUD | H264_DISABLE_SPSPPS;
        let cfg = EncoderConfig {
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 6_000_000,
            codec: Codec::H264,
        };
        apply_recipe(&mut c, &cfg);
        assert_eq!(c.version, NV_ENC_CONFIG_VER);
        assert_eq!(c.gop_length, NVENC_INFINITE_GOPLENGTH);
        assert_eq!(c.frame_interval_p, 1);
        assert_eq!(c.profile_guid, NV_ENC_H264_PROFILE_MAIN_GUID);
        let rc = c.rc_params;
        assert_eq!(rc.rate_control_mode, NV_ENC_PARAMS_RC_CBR);
        assert_eq!(rc.average_bit_rate, 6_000_000);
        assert_eq!(rc.vbv_buffer_size, 200_000);
        assert_eq!(rc.vbv_initial_delay, 200_000);
        assert_eq!(rc.flags & RC_ENABLE_MAX_QP, RC_ENABLE_MAX_QP);
        assert_eq!(rc.flags & RC_ENABLE_AQ, RC_ENABLE_AQ);
        assert_eq!(rc.flags & RC_ZERO_REORDER_DELAY, RC_ZERO_REORDER_DELAY);
        assert_eq!(rc.flags & (RC_ENABLE_LOOKAHEAD | RC_ENABLE_TEMPORAL_AQ), 0);
        assert_eq!(rc.lookahead_depth, 0);
        assert_eq!(rc.max_qp.qp_intra, MAX_QP);
        assert_eq!(rc.max_qp.qp_inter_p, MAX_QP);
        // SAFETY: H.264 member was the one written.
        let h = unsafe { c.encode_codec_config.h264 };
        assert_eq!(h.idr_period, NVENC_INFINITE_GOPLENGTH);
        assert_eq!(h.flags, H264_REPEAT_SPSPPS);
        assert_eq!((h.slice_mode, h.slice_mode_data), (3, 1));
        assert_eq!(h.entropy_coding_mode, NV_ENC_H264_ENTROPY_CODING_MODE_CABAC);
        assert_eq!(h.vui.colour_matrix, VUI_BT709);
        assert_eq!(h.vui.video_full_range_flag, 0);
        assert_eq!(h.vui.bitstream_restriction_flag, 1);
    }

    #[test]
    fn recipe_hevc_uses_the_hevc_member() {
        let mut c = preset_like();
        let cfg = EncoderConfig {
            codec: Codec::Hevc,
            ..EncoderConfig::default()
        };
        apply_recipe(&mut c, &cfg);
        assert_eq!(c.profile_guid, NV_ENC_HEVC_PROFILE_MAIN_GUID);
        // SAFETY: HEVC member was the one written.
        let h = unsafe { c.encode_codec_config.hevc };
        assert_eq!(h.idr_period, NVENC_INFINITE_GOPLENGTH);
        assert_eq!(h.flags, HEVC_REPEAT_SPSPPS);
        assert_eq!((h.slice_mode, h.slice_mode_data), (3, 1));
        assert_eq!(h.vui.colour_primaries, VUI_BT709);
    }

    /// `(profile_idc, width, height)` from an H.264 SPS NAL (start code
    /// included), for the profiles this encoder emits (no High-profile fields).
    fn parse_sps(nal: &[u8]) -> (u32, u32, u32) {
        use crate::codec::h264::tests::{rbsp_of, BitReader};
        let (_, rbsp) = rbsp_of(nal);
        let mut r = BitReader::new(&rbsp);
        let profile = r.bits(8);
        r.bits(16); // constraint flags, level
        r.ue(); // sps id
        assert!(profile < 100, "High-profile SPS fields are not parsed here");
        r.ue(); // log2_max_frame_num_minus4
        match r.ue() {
            0 => {
                r.ue();
            }
            2 => {}
            other => panic!("unexpected pic_order_cnt_type {other}"),
        }
        r.ue(); // max_num_ref_frames
        r.bit(); // gaps
        let w_mbs = r.ue() + 1;
        let h_mbs = r.ue() + 1;
        assert_eq!(r.bit(), 1, "frame_mbs_only");
        r.bit(); // direct_8x8_inference
        let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
        if r.bit() == 1 {
            (cl, cr, ct, cb) = (r.ue(), r.ue(), r.ue(), r.ue());
        }
        (
            profile,
            w_mbs * 16 - 2 * (cl + cr),
            h_mbs * 16 - 2 * (ct + cb),
        )
    }

    /// A moving BGRA test pattern (so P-frames carry real motion).
    fn pattern(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = ((x + t * 4) % 256) as u8;
                px[i + 1] = ((y + t * 2) % 256) as u8;
                px[i + 2] = (((x ^ y) + t) % 256) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    fn nal_types(au: &[u8]) -> Vec<u8> {
        annexb::nal_units(au).map(|n| n[0] & 0x1f).collect()
    }

    /// Real NVENC round trip on the machine's GPU: keyframe structure, forced
    /// IDR, live bitrate, scaled and NV12 input, HEVC. Gated on the driver
    /// library: on a machine without `libnvidia-encode.so.1` (CI) it reports
    /// the skip and passes. Set `SCT_DUMP_STREAM=<dir>` to keep the streams for
    /// inspection (`ffprobe`, `ffmpeg -i x.h264 -f rawvideo …`).
    #[test]
    fn device_round_trip() {
        let probe = match probe() {
            Ok(p) => p,
            Err(e) if e.contains("not loadable") => {
                eprintln!("skipping NVENC round trip: {e}");
                return;
            }
            Err(e) => panic!("NVENC libraries load but the probe failed: {e}"),
        };
        eprintln!("NVENC probe: {probe:?}");
        assert!(probe.h264);

        let cfg = EncoderConfig {
            width: 640,
            height: 360,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 2_000_000,
            codec: Codec::H264,
        };
        let mut enc = NvencEncoder::new(cfg).expect("H.264 encoder");
        let mut stream = Vec::new();
        let mut units = Vec::new();
        for t in 0..12 {
            if t == 6 {
                enc.force_keyframe();
            }
            if t == 8 {
                enc.set_bitrate(1_000_000).expect("live bitrate");
            }
            let px = pattern(640, 360, t);
            let got = enc
                .encode(&Frame::bgra(640, 360, &px, 640 * 4))
                .expect("encode");
            assert!(got.len() <= 1, "one access unit per frame");
            for u in got {
                stream.extend_from_slice(&u.data);
                units.push(u);
            }
        }
        // NVENC is synchronous here: one unit per frame, no warm-up.
        assert_eq!(units.len(), 12);
        let keys: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, u)| u.keyframe)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            keys,
            vec![0, 6],
            "IDR first and when forced, never otherwise"
        );
        for &k in &keys {
            let types = nal_types(&units[k].data);
            assert!(
                types.contains(&7) && types.contains(&8) && types.contains(&5),
                "{types:?}"
            );
            assert!(annexb::is_idr(Codec::H264, &units[k].data));
        }
        for (i, u) in units.iter().enumerate().filter(|(_, u)| !u.keyframe) {
            let types = nal_types(&u.data);
            assert!(!types.contains(&5), "frame {i} is not an IDR: {types:?}");
            // One slice per picture.
            assert_eq!(
                types.iter().filter(|&&t| t == 1).count(),
                1,
                "frame {i}: {types:?}"
            );
        }
        let sps = annexb::nal_units(&units[0].data)
            .find(|n| n[0] & 0x1f == 7)
            .map(|n| [&[0u8, 0, 0, 1][..], n].concat())
            .expect("SPS");
        assert_eq!(
            parse_sps(&sps),
            (77, 640, 360),
            "Main profile, cropped to 640x360"
        );

        // A frame larger than the encode size is resampled, not rejected.
        let big = pattern(1280, 720, 3);
        let scaled = enc
            .encode(&Frame::bgra(1280, 720, &big, 1280 * 4))
            .expect("scaled encode");
        assert_eq!(scaled.len(), 1);
        stream.extend_from_slice(&scaled[0].data);

        // NV12 input on the same session (re-registers the surface).
        let y = vec![128u8; 640 * 360];
        let uv = vec![128u8; 640 * 180];
        let nv12 = enc
            .encode(&Frame::nv12(640, 360, &y, 640, &uv, 640))
            .expect("NV12 encode");
        assert_eq!(nv12.len(), 1);
        stream.extend_from_slice(&nv12[0].data);

        // Last: a pure-red IDR, so a dump shows which matrix NVENC's internal
        // RGB→YUV used — BT.709 limited decodes to Y≈63 Cb≈102 Cr≈240
        // (BT.601 would give Y≈81 Cb≈90).
        enc.force_keyframe();
        let red: Vec<u8> = [0u8, 0, 255, 255].repeat(640 * 360);
        let red = enc
            .encode(&Frame::bgra(640, 360, &red, 640 * 4))
            .expect("red encode");
        assert!(red[0].keyframe);
        stream.extend_from_slice(&red[0].data);
        drop(enc);

        let mut hevc_stream = Vec::new();
        if probe.hevc {
            let mut enc = NvencEncoder::new(EncoderConfig {
                codec: Codec::Hevc,
                ..cfg
            })
            .expect("HEVC encoder");
            for t in 0..4 {
                let px = pattern(640, 360, t);
                let got = enc
                    .encode(&Frame::bgra(640, 360, &px, 640 * 4))
                    .expect("encode");
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].keyframe, t == 0);
                if t == 0 {
                    assert!(annexb::has_parameter_sets(Codec::Hevc, &got[0].data));
                    assert!(annexb::is_idr(Codec::Hevc, &got[0].data));
                }
                hevc_stream.extend_from_slice(&got[0].data);
            }
        }

        if let Some(dir) = std::env::var_os("SCT_DUMP_STREAM") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::write(dir.join("nvenc.h264"), &stream).expect("dump h264");
            if !hevc_stream.is_empty() {
                std::fs::write(dir.join("nvenc.hevc"), &hevc_stream).expect("dump hevc");
            }
        }
    }
}
