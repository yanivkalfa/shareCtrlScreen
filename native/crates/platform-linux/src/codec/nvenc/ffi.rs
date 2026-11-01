//! Minimal runtime binding of NVIDIA's `nvEncodeAPI.h`, API **12.0**
//! (`nv-codec-headers` tag `n12.0.16.1`, MIT — the copy ffmpeg builds against).
//!
//! Only the structs and entry points the encoder uses are declared. Every
//! struct is a field-for-field mirror of the header (reserved arrays included:
//! the driver reads the whole struct whose size the version word implies), and
//! its size and the offsets the encoder touches are pinned by `const` asserts
//! generated from the real header with `gcc` on x86_64 Linux — a layout slip is
//! a compile error, not memory corruption at run time.
//!
//! Bitfields are declared as one `u32` word plus mask constants; GCC allocates
//! them LSB-first on every Linux ABI we build for.
//!
//! Version policy: the struct versions encode the API version (12.0). A driver
//! accepts any client version up to `NvEncodeAPIGetMaxSupportedVersion`, so 12.0
//! runs on every driver since R520 (Oct 2022) — the probe refuses older ones
//! with a message naming the required driver.

use std::ffi::{c_char, c_int, c_void};

use crate::codec::zeroed::Zeroed;

pub(crate) const API_MAJOR: u32 = 12;
pub(crate) const API_MINOR: u32 = 0;
/// `NVENCAPI_VERSION`.
pub(crate) const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);

/// `NVENCAPI_STRUCT_VERSION(ver)`.
const fn struct_version(ver: u32) -> u32 {
    API_VERSION | (ver << 16) | (0x7 << 28)
}

pub(crate) const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub(crate) const NV_ENC_CONFIG_VER: u32 = struct_version(8) | (1 << 31);
pub(crate) const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
pub(crate) const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
pub(crate) const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub(crate) const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub(crate) const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2);
pub(crate) const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub(crate) const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(4);
pub(crate) const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub(crate) const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
pub(crate) const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub(crate) const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);

pub(crate) type NvEncStatus = c_int;
pub(crate) const NV_ENC_SUCCESS: NvEncStatus = 0;
pub(crate) const NV_ENC_ERR_NEED_MORE_INPUT: NvEncStatus = 17;

/// `NVENCSTATUS` names, indexed by value (header order).
const STATUS_NAMES: [&str; 26] = [
    "NV_ENC_SUCCESS",
    "NV_ENC_ERR_NO_ENCODE_DEVICE",
    "NV_ENC_ERR_UNSUPPORTED_DEVICE",
    "NV_ENC_ERR_INVALID_ENCODERDEVICE",
    "NV_ENC_ERR_INVALID_DEVICE",
    "NV_ENC_ERR_DEVICE_NOT_EXIST",
    "NV_ENC_ERR_INVALID_PTR",
    "NV_ENC_ERR_INVALID_EVENT",
    "NV_ENC_ERR_INVALID_PARAM",
    "NV_ENC_ERR_INVALID_CALL",
    "NV_ENC_ERR_OUT_OF_MEMORY",
    "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
    "NV_ENC_ERR_UNSUPPORTED_PARAM",
    "NV_ENC_ERR_LOCK_BUSY",
    "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
    "NV_ENC_ERR_INVALID_VERSION",
    "NV_ENC_ERR_MAP_FAILED",
    "NV_ENC_ERR_NEED_MORE_INPUT",
    "NV_ENC_ERR_ENCODER_BUSY",
    "NV_ENC_ERR_EVENT_NOT_REGISTERD",
    "NV_ENC_ERR_GENERIC",
    "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
    "NV_ENC_ERR_UNIMPLEMENTED",
    "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
    "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
    "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
];

pub(crate) fn status_name(status: NvEncStatus) -> &'static str {
    usize::try_from(status)
        .ok()
        .and_then(|i| STATUS_NAMES.get(i))
        .copied()
        .unwrap_or("unknown NVENCSTATUS")
}

pub(crate) const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

// NV_ENC_PARAMS_RC_MODE
pub(crate) const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
// NV_ENC_TUNING_INFO
pub(crate) const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
// NV_ENC_PIC_STRUCT
pub(crate) const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x1;
// NV_ENC_PIC_TYPE
pub(crate) const NV_ENC_PIC_TYPE_IDR: u32 = 0x3;
// NV_ENC_PIC_FLAGS
pub(crate) const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub(crate) const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
// NV_ENC_BUFFER_FORMAT
pub(crate) const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x1;
/// Memory order B,G,R,A — "word-ordered A8R8G8B8" on a little-endian machine.
pub(crate) const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
// NV_ENC_INPUT_RESOURCE_TYPE / NV_ENC_BUFFER_USAGE / NV_ENC_DEVICE_TYPE
pub(crate) const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 0x1;
pub(crate) const NV_ENC_INPUT_IMAGE: u32 = 0x0;
pub(crate) const NV_ENC_DEVICE_TYPE_CUDA: u32 = 0x1;
// NV_ENC_H264_ENTROPY_CODING_MODE
pub(crate) const NV_ENC_H264_ENTROPY_CODING_MODE_CABAC: u32 = 0x1;
// NV_ENC_CAPS
pub(crate) const NV_ENC_CAPS_WIDTH_MAX: u32 = 16;
pub(crate) const NV_ENC_CAPS_HEIGHT_MAX: u32 = 17;
// NV_ENC_VUI_* (ITU-T H.264 Annex E values)
pub(crate) const VUI_VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
pub(crate) const VUI_BT709: u32 = 1;

// NV_ENC_RC_PARAMS bitfield word.
pub(crate) const RC_ENABLE_MAX_QP: u32 = 1 << 1;
pub(crate) const RC_ENABLE_AQ: u32 = 1 << 3;
pub(crate) const RC_ENABLE_LOOKAHEAD: u32 = 1 << 5;
pub(crate) const RC_ENABLE_TEMPORAL_AQ: u32 = 1 << 8;
pub(crate) const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;
// NV_ENC_CONFIG_H264 bitfield word.
pub(crate) const H264_OUTPUT_AUD: u32 = 1 << 6;
pub(crate) const H264_DISABLE_SPSPPS: u32 = 1 << 7;
pub(crate) const H264_ENABLE_INTRA_REFRESH: u32 = 1 << 10;
pub(crate) const H264_REPEAT_SPSPPS: u32 = 1 << 12;
pub(crate) const H264_ENABLE_FILLER_DATA: u32 = 1 << 17;
// NV_ENC_CONFIG_HEVC bitfield word.
pub(crate) const HEVC_OUTPUT_AUD: u32 = 1 << 4;
pub(crate) const HEVC_DISABLE_SPSPPS: u32 = 1 << 6;
pub(crate) const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
pub(crate) const HEVC_ENABLE_INTRA_REFRESH: u32 = 1 << 8;
pub(crate) const HEVC_ENABLE_FILLER_DATA: u32 = 1 << 14;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
    Guid {
        data1,
        data2,
        data3,
        data4,
    }
}

pub(crate) const NV_ENC_CODEC_H264_GUID: Guid = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub(crate) const NV_ENC_CODEC_HEVC_GUID: Guid = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub(crate) const NV_ENC_H264_PROFILE_MAIN_GUID: Guid = guid(
    0x60b5c1d4,
    0x67fe,
    0x4790,
    [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
);
pub(crate) const NV_ENC_HEVC_PROFILE_MAIN_GUID: Guid = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
pub(crate) const NV_ENC_PRESET_P3_GUID: Guid = guid(
    0x36850110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);

#[repr(C)]
pub(crate) struct NvEncCapsParam {
    pub version: u32,
    pub caps_to_query: u32,
    pub reserved: [u32; 62],
}

#[repr(C)]
pub(crate) struct NvEncCreateBitstreamBuffer {
    pub version: u32,
    pub size: u32,
    pub memory_heap: u32,
    pub reserved: u32,
    pub bitstream_buffer: *mut c_void,
    pub bitstream_buffer_ptr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncQp {
    pub qp_inter_p: u32,
    pub qp_inter_b: u32,
    pub qp_intra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncRcParams {
    pub version: u32,
    pub rate_control_mode: u32,
    pub const_qp: NvEncQp,
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    /// `enableMinQP … reservedBitFields`, see `RC_*`.
    pub flags: u32,
    pub min_qp: NvEncQp,
    pub max_qp: NvEncQp,
    pub initial_rc_qp: NvEncQp,
    pub temporal_layer_idx_mask: u32,
    pub temporal_layer_qp: [u8; 8],
    pub target_quality: u8,
    pub target_quality_lsb: u8,
    pub lookahead_depth: u16,
    pub low_delay_key_frame_scale: u8,
    pub y_dc_qp_index_offset: i8,
    pub u_dc_qp_index_offset: i8,
    pub v_dc_qp_index_offset: i8,
    pub qp_map_mode: u32,
    pub multi_pass: u32,
    pub alpha_layer_bitrate_ratio: u32,
    pub cb_qp_index_offset: i8,
    pub cr_qp_index_offset: i8,
    pub reserved2: u16,
    pub reserved: [u32; 4],
}

/// `NV_ENC_CONFIG_H264_VUI_PARAMETERS` (= the HEVC one).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncVuiParameters {
    pub overscan_info_present_flag: u32,
    pub overscan_info: u32,
    pub video_signal_type_present_flag: u32,
    pub video_format: u32,
    pub video_full_range_flag: u32,
    pub colour_description_present_flag: u32,
    pub colour_primaries: u32,
    pub transfer_characteristics: u32,
    pub colour_matrix: u32,
    pub chroma_sample_location_flag: u32,
    pub chroma_sample_location_top: u32,
    pub chroma_sample_location_bot: u32,
    pub bitstream_restriction_flag: u32,
    pub timing_info_present_flag: u32,
    pub num_unit_in_ticks: u32,
    pub time_scale: u32,
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncConfigH264 {
    /// `enableTemporalSVC … reservedBitFields`, see `H264_*`.
    pub flags: u32,
    pub level: u32,
    pub idr_period: u32,
    pub separate_colour_plane_flag: u32,
    pub disable_deblocking_filter_idc: u32,
    pub num_temporal_layers: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub adaptive_transform_mode: u32,
    pub fmo_mode: u32,
    pub bdirect_mode: u32,
    pub entropy_coding_mode: u32,
    pub stereo_mode: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub vui: NvEncVuiParameters,
    pub ltr_num_frames: u32,
    pub ltr_trust_mode: u32,
    pub chroma_format_idc: u32,
    pub max_temporal_layers: u32,
    pub use_b_frames_as_ref: u32,
    pub num_ref_l0: u32,
    pub num_ref_l1: u32,
    pub reserved1: [u32; 267],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncConfigHevc {
    pub level: u32,
    pub tier: u32,
    pub min_cu_size: u32,
    pub max_cu_size: u32,
    /// `useConstrainedIntraPred … reserved`, see `HEVC_*`.
    pub flags: u32,
    pub idr_period: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames_in_dpb: u32,
    pub ltr_num_frames: u32,
    pub vps_id: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub max_temporal_layers_minus1: u32,
    pub vui: NvEncVuiParameters,
    pub ltr_trust_mode: u32,
    pub use_b_frames_as_ref: u32,
    pub num_ref_l0: u32,
    pub num_ref_l1: u32,
    pub reserved1: [u32; 214],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_CODEC_CONFIG`. The AV1 / ME-only members are not used and are no
/// larger than H.264's (the union is 1792 bytes, asserted below).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union NvEncCodecConfig {
    pub h264: NvEncConfigH264,
    pub hevc: NvEncConfigHevc,
    pub reserved: [u32; 320],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncConfig {
    pub version: u32,
    pub profile_guid: Guid,
    pub gop_length: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: u32,
    pub mv_precision: u32,
    pub rc_params: NvEncRcParams,
    pub encode_codec_config: NvEncCodecConfig,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncExternalMeHintCounts {
    pub bits: u32,
    pub reserved1: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct NvEncInitializeParams {
    pub version: u32,
    pub encode_guid: Guid,
    pub preset_guid: Guid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    /// `reportSliceOffsets … reservedBitFields`.
    pub flags: u32,
    pub priv_data_size: u32,
    pub priv_data: *mut c_void,
    pub encode_config: *mut NvEncConfig,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [NvEncExternalMeHintCounts; 2],
    pub tuning_info: u32,
    pub buffer_format: u32,
    pub reserved: [u32; 287],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub(crate) struct NvEncReconfigureParams {
    pub version: u32,
    pub re_init_encode_params: NvEncInitializeParams,
    /// `resetEncoder:1, forceIDR:1, reserved:30`.
    pub flags: u32,
}

#[repr(C)]
pub(crate) struct NvEncPresetConfig {
    pub version: u32,
    pub preset_cfg: NvEncConfig,
    pub reserved1: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub(crate) struct NvEncPicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: *mut c_void,
    pub output_bitstream: *mut c_void,
    pub completion_event: *mut c_void,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    /// `NV_ENC_CODEC_PIC_PARAMS` union (H.264/HEVC/AV1 per-picture params).
    /// All-zero = "no per-picture overrides", which is all this encoder needs;
    /// sized to the union's largest (AV1) member.
    pub codec_pic_params: [u64; 194],
    pub me_hint_counts_per_block: [NvEncExternalMeHintCounts; 2],
    pub me_external_hints: *mut c_void,
    pub reserved1: [u32; 6],
    pub reserved2: [*mut c_void; 2],
    pub qp_delta_map: *mut i8,
    pub qp_delta_map_size: u32,
    pub reserved_bit_fields: u32,
    pub me_hint_ref_pic_dist: [u16; 2],
    pub alpha_buffer: *mut c_void,
    pub me_external_sb_hints: *mut c_void,
    pub me_sb_hints_count: u32,
    pub reserved3: [u32; 285],
    pub reserved4: [*mut c_void; 58],
}

#[repr(C)]
pub(crate) struct NvEncLockBitstream {
    pub version: u32,
    /// `doNotWait:1, ltrFrame:1, getRCStats:1, reserved:29`.
    pub flags: u32,
    pub output_bitstream: *mut c_void,
    pub slice_offsets: *mut u32,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer_ptr: *mut c_void,
    pub picture_type: u32,
    pub picture_struct: u32,
    pub frame_avg_qp: u32,
    pub frame_satd: u32,
    pub ltr_frame_idx: u32,
    pub ltr_frame_bitmap: u32,
    pub temporal_id: u32,
    pub reserved: [u32; 12],
    pub intra_mb_count: u32,
    pub inter_mb_count: u32,
    pub average_mvx: i32,
    pub average_mvy: i32,
    pub alpha_layer_size_in_bytes: u32,
    pub reserved1: [u32; 218],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub(crate) struct NvEncMapInputResource {
    pub version: u32,
    pub sub_resource_index: u32,
    pub input_resource: *mut c_void,
    pub registered_resource: *mut c_void,
    pub mapped_resource: *mut c_void,
    pub mapped_buffer_fmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub(crate) struct NvEncRegisterResource {
    pub version: u32,
    pub resource_type: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub sub_resource_index: u32,
    pub resource_to_register: *mut c_void,
    pub registered_resource: *mut c_void,
    pub buffer_format: u32,
    pub buffer_usage: u32,
    pub p_input_fence_point: *mut c_void,
    pub reserved1: [u32; 247],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
pub(crate) struct NvEncOpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub(crate) struct NvEncSequenceParamPayload {
    pub version: u32,
    pub in_buffer_size: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub spspps_buffer: *mut c_void,
    pub out_spspps_payload_size: *mut u32,
    pub reserved: [u32; 250],
    pub reserved2: [*mut c_void; 64],
}

type Enc = *mut c_void;
type Unused = *mut c_void;

/// `NV_ENCODE_API_FUNCTION_LIST`, slot for slot. Entry points this binding does
/// not call are kept as opaque pointers so the slots after them stay put.
#[repr(C)]
pub(crate) struct NvEncodeApiFunctionList {
    pub version: u32,
    pub reserved: u32,
    pub open_encode_session: Unused,
    pub get_encode_guid_count: Option<unsafe extern "C" fn(Enc, *mut u32) -> NvEncStatus>,
    pub get_encode_profile_guid_count: Unused,
    pub get_encode_profile_guids: Unused,
    pub get_encode_guids:
        Option<unsafe extern "C" fn(Enc, *mut Guid, u32, *mut u32) -> NvEncStatus>,
    pub get_input_format_count: Unused,
    pub get_input_formats: Unused,
    pub get_encode_caps:
        Option<unsafe extern "C" fn(Enc, Guid, *mut NvEncCapsParam, *mut c_int) -> NvEncStatus>,
    pub get_encode_preset_count: Unused,
    pub get_encode_preset_guids: Unused,
    pub get_encode_preset_config: Unused,
    pub initialize_encoder:
        Option<unsafe extern "C" fn(Enc, *mut NvEncInitializeParams) -> NvEncStatus>,
    pub create_input_buffer: Unused,
    pub destroy_input_buffer: Unused,
    pub create_bitstream_buffer:
        Option<unsafe extern "C" fn(Enc, *mut NvEncCreateBitstreamBuffer) -> NvEncStatus>,
    pub destroy_bitstream_buffer: Option<unsafe extern "C" fn(Enc, *mut c_void) -> NvEncStatus>,
    pub encode_picture: Option<unsafe extern "C" fn(Enc, *mut NvEncPicParams) -> NvEncStatus>,
    pub lock_bitstream: Option<unsafe extern "C" fn(Enc, *mut NvEncLockBitstream) -> NvEncStatus>,
    pub unlock_bitstream: Option<unsafe extern "C" fn(Enc, *mut c_void) -> NvEncStatus>,
    pub lock_input_buffer: Unused,
    pub unlock_input_buffer: Unused,
    pub get_encode_stats: Unused,
    pub get_sequence_params:
        Option<unsafe extern "C" fn(Enc, *mut NvEncSequenceParamPayload) -> NvEncStatus>,
    pub register_async_event: Unused,
    pub unregister_async_event: Unused,
    pub map_input_resource:
        Option<unsafe extern "C" fn(Enc, *mut NvEncMapInputResource) -> NvEncStatus>,
    pub unmap_input_resource: Option<unsafe extern "C" fn(Enc, *mut c_void) -> NvEncStatus>,
    pub destroy_encoder: Option<unsafe extern "C" fn(Enc) -> NvEncStatus>,
    pub invalidate_ref_frames: Unused,
    pub open_encode_session_ex:
        Option<unsafe extern "C" fn(*mut NvEncOpenEncodeSessionExParams, *mut Enc) -> NvEncStatus>,
    pub register_resource:
        Option<unsafe extern "C" fn(Enc, *mut NvEncRegisterResource) -> NvEncStatus>,
    pub unregister_resource: Option<unsafe extern "C" fn(Enc, *mut c_void) -> NvEncStatus>,
    pub reconfigure_encoder:
        Option<unsafe extern "C" fn(Enc, *mut NvEncReconfigureParams) -> NvEncStatus>,
    pub reserved1: Unused,
    pub create_mv_buffer: Unused,
    pub destroy_mv_buffer: Unused,
    pub run_motion_estimation_only: Unused,
    pub get_last_error_string: Option<unsafe extern "C" fn(Enc) -> *const c_char>,
    pub set_io_cuda_streams: Unused,
    pub get_encode_preset_config_ex:
        Option<unsafe extern "C" fn(Enc, Guid, Guid, u32, *mut NvEncPresetConfig) -> NvEncStatus>,
    pub get_sequence_param_ex: Unused,
    pub reserved2: [Unused; 277],
}

/// `NvEncodeAPIGetMaxSupportedVersion`: `(major << 4) | minor`.
pub(crate) type GetMaxSupportedVersionFn = unsafe extern "C" fn(*mut u32) -> NvEncStatus;
/// `NvEncodeAPICreateInstance`.
pub(crate) type CreateInstanceFn =
    unsafe extern "C" fn(*mut NvEncodeApiFunctionList) -> NvEncStatus;

/// `NvEncodeAPIGetMaxSupportedVersion`'s packed value → `(major, minor)`.
pub(crate) fn unpack_max_version(v: u32) -> (u32, u32) {
    (v >> 4, v & 0xf)
}

// SAFETY (all impls): plain C data — integers, raw pointers, `Option<fn>`,
// arrays and unions of those; zero is the documented default of every field.
unsafe impl Zeroed for NvEncCapsParam {}
unsafe impl Zeroed for NvEncCreateBitstreamBuffer {}
unsafe impl Zeroed for NvEncConfig {}
unsafe impl Zeroed for NvEncInitializeParams {}
unsafe impl Zeroed for NvEncReconfigureParams {}
unsafe impl Zeroed for NvEncPresetConfig {}
unsafe impl Zeroed for NvEncPicParams {}
unsafe impl Zeroed for NvEncLockBitstream {}
unsafe impl Zeroed for NvEncMapInputResource {}
unsafe impl Zeroed for NvEncRegisterResource {}
unsafe impl Zeroed for NvEncOpenEncodeSessionExParams {}
unsafe impl Zeroed for NvEncSequenceParamPayload {}
unsafe impl Zeroed for NvEncodeApiFunctionList {}

/// Layout pinned against `nvEncodeAPI.h` n12.0.16.1 (`sizeof`/`offsetof` from
/// gcc, x86_64 / LP64). Only LP64 targets are built for Linux.
#[cfg(target_pointer_width = "64")]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    const _: () = assert!(size_of::<Guid>() == 16);
    const _: () = assert!(size_of::<NvEncCapsParam>() == 256);
    const _: () = assert!(size_of::<NvEncCreateBitstreamBuffer>() == 776);
    const _: () = assert!(offset_of!(NvEncCreateBitstreamBuffer, bitstream_buffer) == 16);
    const _: () = assert!(size_of::<NvEncRcParams>() == 128);
    const _: () = assert!(offset_of!(NvEncRcParams, average_bit_rate) == 20);
    const _: () = assert!(offset_of!(NvEncRcParams, vbv_buffer_size) == 28);
    const _: () = assert!(offset_of!(NvEncRcParams, min_qp) == 40);
    const _: () = assert!(offset_of!(NvEncRcParams, max_qp) == 52);
    const _: () = assert!(offset_of!(NvEncRcParams, temporal_layer_qp) == 80);
    const _: () = assert!(offset_of!(NvEncRcParams, lookahead_depth) == 90);
    const _: () = assert!(offset_of!(NvEncRcParams, low_delay_key_frame_scale) == 92);
    const _: () = assert!(offset_of!(NvEncRcParams, qp_map_mode) == 96);
    const _: () = assert!(offset_of!(NvEncRcParams, multi_pass) == 100);
    const _: () = assert!(offset_of!(NvEncRcParams, cb_qp_index_offset) == 108);
    const _: () = assert!(offset_of!(NvEncRcParams, reserved) == 112);
    const _: () = assert!(size_of::<NvEncVuiParameters>() == 112);
    const _: () = assert!(size_of::<NvEncConfigH264>() == 1792);
    const _: () = assert!(offset_of!(NvEncConfigH264, idr_period) == 8);
    const _: () = assert!(offset_of!(NvEncConfigH264, entropy_coding_mode) == 44);
    const _: () = assert!(offset_of!(NvEncConfigH264, max_num_ref_frames) == 60);
    const _: () = assert!(offset_of!(NvEncConfigH264, slice_mode) == 64);
    const _: () = assert!(offset_of!(NvEncConfigH264, vui) == 72);
    const _: () = assert!(offset_of!(NvEncConfigH264, ltr_num_frames) == 184);
    const _: () = assert!(offset_of!(NvEncConfigH264, num_ref_l1) == 208);
    const _: () = assert!(offset_of!(NvEncConfigH264, reserved2) == 1280);
    const _: () = assert!(size_of::<NvEncConfigHevc>() == 1560);
    const _: () = assert!(offset_of!(NvEncConfigHevc, idr_period) == 20);
    const _: () = assert!(offset_of!(NvEncConfigHevc, slice_mode) == 52);
    const _: () = assert!(offset_of!(NvEncConfigHevc, vui) == 64);
    const _: () = assert!(offset_of!(NvEncConfigHevc, num_ref_l1) == 188);
    const _: () = assert!(offset_of!(NvEncConfigHevc, reserved2) == 1048);
    const _: () = assert!(size_of::<NvEncCodecConfig>() == 1792);
    const _: () = assert!(size_of::<NvEncConfig>() == 3584);
    const _: () = assert!(offset_of!(NvEncConfig, gop_length) == 20);
    const _: () = assert!(offset_of!(NvEncConfig, rc_params) == 40);
    const _: () = assert!(offset_of!(NvEncConfig, encode_codec_config) == 168);
    const _: () = assert!(offset_of!(NvEncConfig, reserved) == 1960);
    const _: () = assert!(offset_of!(NvEncConfig, reserved2) == 3072);
    const _: () = assert!(size_of::<NvEncInitializeParams>() == 1808);
    const _: () = assert!(offset_of!(NvEncInitializeParams, encode_width) == 36);
    const _: () = assert!(offset_of!(NvEncInitializeParams, enable_ptd) == 64);
    const _: () = assert!(offset_of!(NvEncInitializeParams, priv_data) == 80);
    const _: () = assert!(offset_of!(NvEncInitializeParams, encode_config) == 88);
    const _: () = assert!(offset_of!(NvEncInitializeParams, max_encode_width) == 96);
    const _: () = assert!(offset_of!(NvEncInitializeParams, tuning_info) == 136);
    const _: () = assert!(offset_of!(NvEncInitializeParams, reserved2) == 1296);
    const _: () = assert!(size_of::<NvEncReconfigureParams>() == 1824);
    const _: () = assert!(offset_of!(NvEncReconfigureParams, re_init_encode_params) == 8);
    const _: () = assert!(size_of::<NvEncPresetConfig>() == 5128);
    const _: () = assert!(offset_of!(NvEncPresetConfig, preset_cfg) == 8);
    const _: () = assert!(offset_of!(NvEncPresetConfig, reserved2) == 4616);
    const _: () = assert!(size_of::<NvEncPicParams>() == 3360);
    const _: () = assert!(offset_of!(NvEncPicParams, input_time_stamp) == 24);
    const _: () = assert!(offset_of!(NvEncPicParams, input_buffer) == 40);
    const _: () = assert!(offset_of!(NvEncPicParams, buffer_fmt) == 64);
    const _: () = assert!(offset_of!(NvEncPicParams, picture_type) == 72);
    const _: () = assert!(offset_of!(NvEncPicParams, codec_pic_params) == 80);
    const _: () = assert!(offset_of!(NvEncPicParams, me_hint_counts_per_block) == 1632);
    const _: () = assert!(offset_of!(NvEncPicParams, qp_delta_map) == 1712);
    const _: () = assert!(offset_of!(NvEncPicParams, me_hint_ref_pic_dist) == 1728);
    const _: () = assert!(offset_of!(NvEncPicParams, alpha_buffer) == 1736);
    const _: () = assert!(offset_of!(NvEncPicParams, reserved3) == 1756);
    const _: () = assert!(offset_of!(NvEncPicParams, reserved4) == 2896);
    const _: () = assert!(size_of::<NvEncLockBitstream>() == 1544);
    const _: () = assert!(offset_of!(NvEncLockBitstream, bitstream_size_in_bytes) == 36);
    const _: () = assert!(offset_of!(NvEncLockBitstream, bitstream_buffer_ptr) == 56);
    const _: () = assert!(offset_of!(NvEncLockBitstream, picture_type) == 64);
    const _: () = assert!(offset_of!(NvEncLockBitstream, frame_avg_qp) == 72);
    const _: () = assert!(offset_of!(NvEncLockBitstream, intra_mb_count) == 140);
    const _: () = assert!(offset_of!(NvEncLockBitstream, reserved1) == 160);
    const _: () = assert!(offset_of!(NvEncLockBitstream, reserved2) == 1032);
    const _: () = assert!(size_of::<NvEncMapInputResource>() == 1544);
    const _: () = assert!(offset_of!(NvEncMapInputResource, mapped_resource) == 24);
    const _: () = assert!(offset_of!(NvEncMapInputResource, mapped_buffer_fmt) == 32);
    const _: () = assert!(offset_of!(NvEncMapInputResource, reserved2) == 1040);
    const _: () = assert!(size_of::<NvEncRegisterResource>() == 1536);
    const _: () = assert!(offset_of!(NvEncRegisterResource, resource_to_register) == 24);
    const _: () = assert!(offset_of!(NvEncRegisterResource, buffer_format) == 40);
    const _: () = assert!(offset_of!(NvEncRegisterResource, p_input_fence_point) == 48);
    const _: () = assert!(offset_of!(NvEncRegisterResource, reserved2) == 1048);
    const _: () = assert!(size_of::<NvEncOpenEncodeSessionExParams>() == 1552);
    const _: () = assert!(offset_of!(NvEncOpenEncodeSessionExParams, api_version) == 24);
    const _: () = assert!(offset_of!(NvEncOpenEncodeSessionExParams, reserved2) == 1040);
    const _: () = assert!(size_of::<NvEncSequenceParamPayload>() == 1544);
    const _: () = assert!(offset_of!(NvEncSequenceParamPayload, out_spspps_payload_size) == 24);
    const _: () = assert!(size_of::<NvEncodeApiFunctionList>() == 2552);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, get_encode_guids) == 40);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, get_encode_caps) == 64);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, create_bitstream_buffer) == 120);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, lock_bitstream) == 144);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, map_input_resource) == 208);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, register_resource) == 248);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, initialize_encoder) == 96);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, encode_picture) == 136);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, get_sequence_params) == 184);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, destroy_encoder) == 224);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, open_encode_session_ex) == 240);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, reconfigure_encoder) == 264);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, get_last_error_string) == 304);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, get_encode_preset_config_ex) == 320);
    const _: () = assert!(offset_of!(NvEncodeApiFunctionList, reserved2) == 336);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_versions_match_the_header_macros() {
        // Values printed by the n12.0.16.1 header's own macros.
        assert_eq!(API_VERSION, 12);
        assert_eq!(NV_ENC_CAPS_PARAM_VER, 1_879_113_740);
        assert_eq!(NV_ENC_CONFIG_VER, 4_027_056_140);
        assert_eq!(NV_ENC_INITIALIZE_PARAMS_VER, 4_026_859_532);
        assert_eq!(NV_ENC_RECONFIGURE_PARAMS_VER, 4_026_597_388);
        assert_eq!(NV_ENC_PRESET_CONFIG_VER, 4_026_793_996);
        assert_eq!(NV_ENC_PIC_PARAMS_VER, 4_026_925_068);
        assert_eq!(NV_ENC_LOCK_BITSTREAM_VER, 1_879_179_276);
        assert_eq!(NV_ENC_MAP_INPUT_RESOURCE_VER, 1_879_310_348);
        assert_eq!(NV_ENC_REGISTER_RESOURCE_VER, 1_879_310_348);
        assert_eq!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, 1_879_113_740);
        assert_eq!(NV_ENCODE_API_FUNCTION_LIST_VER, 1_879_179_276);
        assert_eq!(NV_ENC_CREATE_BITSTREAM_BUFFER_VER, 1_879_113_740);
        assert_eq!(NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, 1_879_113_740);
    }

    #[test]
    fn max_version_unpacks() {
        assert_eq!(unpack_max_version(0xd0), (13, 0));
        assert_eq!(unpack_max_version(0xc2), (12, 2));
        assert_eq!(unpack_max_version(0xb1), (11, 1));
    }

    #[test]
    fn status_names() {
        assert_eq!(status_name(NV_ENC_SUCCESS), "NV_ENC_SUCCESS");
        assert_eq!(
            status_name(NV_ENC_ERR_NEED_MORE_INPUT),
            "NV_ENC_ERR_NEED_MORE_INPUT"
        );
        assert_eq!(status_name(25), "NV_ENC_ERR_RESOURCE_NOT_MAPPED");
        assert_eq!(status_name(-1), "unknown NVENCSTATUS");
        assert_eq!(status_name(99), "unknown NVENCSTATUS");
    }
}
