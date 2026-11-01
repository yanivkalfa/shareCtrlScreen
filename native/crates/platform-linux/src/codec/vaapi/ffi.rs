//! Minimal runtime binding of libva (`va.h`, `va_drm.h`, `va_enc_h264.h`,
//! `va_vpp.h`; libva is MIT-licensed): the encoder (task L3) and the H.264
//! decoder (task L4, `VAEntrypointVLD` + the `va.h` H.264 decode buffers).
//!
//! Loaded with `dlopen("libva.so.2")` + `dlopen("libva-drm.so.2")` instead of
//! linking, so the binary has no `DT_NEEDED` on libva and starts on machines
//! without it (decision #7); the probe then reports VA-API absent.
//!
//! Structs mirror the headers field for field. Their sizes and the offsets the
//! encoder writes are pinned by `const` asserts generated from the libva 2.20
//! (VA-API 1.20) headers with `gcc` on x86_64 — the version floor the probe
//! enforces — so a layout slip fails the build instead of corrupting a driver
//! call. Bitfield unions are one `u32` plus the shift/width constants the
//! encoder uses (GCC packs them LSB-first on every Linux ABI).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::{Arc, OnceLock};

use libloading::Library;

use crate::codec::zeroed::Zeroed;

pub(crate) type VADisplay = *mut c_void;
pub(crate) type VAStatus = c_int;
pub(crate) type VAProfile = c_int;
pub(crate) type VAEntrypoint = c_int;
pub(crate) type VAConfigID = c_uint;
pub(crate) type VAContextID = c_uint;
pub(crate) type VASurfaceID = c_uint;
pub(crate) type VABufferID = c_uint;
pub(crate) type VAImageID = c_uint;
pub(crate) type VABufferType = c_int;
/// `void (*)(void *user_context, const char *message)`.
pub(crate) type VAMessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

pub(crate) const VA_STATUS_SUCCESS: VAStatus = 0;
pub(crate) const VA_INVALID_ID: c_uint = 0xffff_ffff;
pub(crate) const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

// VAProfile
pub(crate) const VA_PROFILE_NONE: VAProfile = -1;
pub(crate) const VA_PROFILE_H264_MAIN: VAProfile = 6;
pub(crate) const VA_PROFILE_H264_HIGH: VAProfile = 7;
pub(crate) const VA_PROFILE_H264_CONSTRAINED_BASELINE: VAProfile = 13;
// VAEntrypoint
pub(crate) const VA_ENTRYPOINT_VLD: VAEntrypoint = 1;
pub(crate) const VA_ENTRYPOINT_ENC_SLICE: VAEntrypoint = 6;
pub(crate) const VA_ENTRYPOINT_ENC_SLICE_LP: VAEntrypoint = 8;
pub(crate) const VA_ENTRYPOINT_VIDEO_PROC: VAEntrypoint = 10;
// VAConfigAttribType
pub(crate) const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
pub(crate) const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
pub(crate) const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
pub(crate) const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
pub(crate) const VA_RT_FORMAT_YUV420: u32 = 0x1;
pub(crate) const VA_RT_FORMAT_RGB32: u32 = 0x2_0000;
pub(crate) const VA_RC_CBR: u32 = 0x2;
pub(crate) const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x1;
pub(crate) const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x2;
/// `vaCreateContext` flag.
pub(crate) const VA_PROGRESSIVE: c_int = 0x1;
// VASurfaceAttribType / flags / VAGenericValueType
pub(crate) const VA_SURFACE_ATTRIB_PIXEL_FORMAT: u32 = 1;
pub(crate) const VA_SURFACE_ATTRIB_MAX_WIDTH: u32 = 3;
pub(crate) const VA_SURFACE_ATTRIB_MAX_HEIGHT: u32 = 5;
pub(crate) const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x2;
pub(crate) const VA_GENERIC_VALUE_TYPE_INTEGER: u32 = 1;
// VABufferType
pub(crate) const VA_PICTURE_PARAMETER_BUFFER_TYPE: VABufferType = 0;
pub(crate) const VA_IQ_MATRIX_BUFFER_TYPE: VABufferType = 1;
pub(crate) const VA_SLICE_PARAMETER_BUFFER_TYPE: VABufferType = 4;
pub(crate) const VA_SLICE_DATA_BUFFER_TYPE: VABufferType = 5;
pub(crate) const VA_ENC_CODED_BUFFER_TYPE: VABufferType = 21;
pub(crate) const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: VABufferType = 22;
pub(crate) const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: VABufferType = 23;
pub(crate) const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: VABufferType = 24;
pub(crate) const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: VABufferType = 25;
pub(crate) const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: VABufferType = 26;
pub(crate) const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: VABufferType = 27;
pub(crate) const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: VABufferType = 41;
// VAEncMiscParameterType
pub(crate) const VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE: u32 = 0;
pub(crate) const VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL: u32 = 1;
pub(crate) const VA_ENC_MISC_PARAMETER_TYPE_HRD: u32 = 5;
// VAEncPackedHeaderType
pub(crate) const VA_ENC_PACKED_HEADER_TYPE_SEQUENCE: u32 = 1;
pub(crate) const VA_ENC_PACKED_HEADER_TYPE_PICTURE: u32 = 2;
// VAPictureH264 flags
pub(crate) const VA_PICTURE_H264_INVALID: u32 = 0x1;
pub(crate) const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x8;
pub(crate) const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x10;
/// `VASliceParameterBufferH264::slice_data_flag`: the whole slice is in the
/// buffer.
pub(crate) const VA_SLICE_DATA_FLAG_ALL: u32 = 0x0;
// Coded buffer status
pub(crate) const VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK: u32 = 0x200;
// VPP
pub(crate) const VA_PROC_COLOR_STANDARD_BT709: u32 = 2;
pub(crate) const VA_PROC_COLOR_STANDARD_SRGB: u32 = 8;
pub(crate) const VA_FILTER_SCALING_HQ: u32 = 0x200;
pub(crate) const VA_SOURCE_RANGE_REDUCED: u8 = 1;
pub(crate) const VA_SOURCE_RANGE_FULL: u8 = 2;
// Fourcc (memory byte order, e.g. BGRA = bytes B,G,R,A)
pub(crate) const VA_FOURCC_NV12: u32 = 0x3231_564e;
pub(crate) const VA_FOURCC_BGRA: u32 = 0x4152_4742;
pub(crate) const VA_FOURCC_BGRX: u32 = 0x5852_4742;

// VAEncSequenceParameterBufferH264.seq_fields
pub(crate) const SEQ_CHROMA_FORMAT_IDC_SHIFT: u32 = 0;
pub(crate) const SEQ_FRAME_MBS_ONLY: u32 = 1 << 2;
pub(crate) const SEQ_DIRECT_8X8_INFERENCE: u32 = 1 << 5;
pub(crate) const SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT: u32 = 6;
pub(crate) const SEQ_PIC_ORDER_CNT_TYPE_SHIFT: u32 = 10;
// VAEncSequenceParameterBufferH264.vui_fields
pub(crate) const VUI_TIMING_INFO_PRESENT: u32 = 1 << 1;
pub(crate) const VUI_BITSTREAM_RESTRICTION: u32 = 1 << 2;
pub(crate) const VUI_LOG2_MAX_MV_LENGTH_HORIZONTAL_SHIFT: u32 = 3;
pub(crate) const VUI_LOG2_MAX_MV_LENGTH_VERTICAL_SHIFT: u32 = 8;
pub(crate) const VUI_MOTION_VECTORS_OVER_PIC_BOUNDARIES: u32 = 1 << 15;
// VAEncPictureParameterBufferH264.pic_fields
pub(crate) const PIC_IDR: u32 = 1 << 0;
pub(crate) const PIC_REFERENCE_SHIFT: u32 = 1;
pub(crate) const PIC_ENTROPY_CODING_MODE: u32 = 1 << 3;
pub(crate) const PIC_DEBLOCKING_FILTER_CONTROL_PRESENT: u32 = 1 << 9;
// VAPictureParameterBufferH264.seq_fields
pub(crate) const DEC_SEQ_CHROMA_FORMAT_IDC_SHIFT: u32 = 0;
pub(crate) const DEC_SEQ_GAPS_IN_FRAME_NUM_ALLOWED: u32 = 1 << 3;
pub(crate) const DEC_SEQ_FRAME_MBS_ONLY: u32 = 1 << 4;
pub(crate) const DEC_SEQ_DIRECT_8X8_INFERENCE: u32 = 1 << 6;
pub(crate) const DEC_SEQ_MIN_LUMA_BI_PRED_SIZE_8X8: u32 = 1 << 7;
pub(crate) const DEC_SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT: u32 = 8;
pub(crate) const DEC_SEQ_PIC_ORDER_CNT_TYPE_SHIFT: u32 = 12;
pub(crate) const DEC_SEQ_LOG2_MAX_POC_LSB_MINUS4_SHIFT: u32 = 14;
pub(crate) const DEC_SEQ_DELTA_PIC_ORDER_ALWAYS_ZERO: u32 = 1 << 18;
// VAPictureParameterBufferH264.pic_fields
pub(crate) const DEC_PIC_ENTROPY_CODING_MODE: u32 = 1 << 0;
pub(crate) const DEC_PIC_WEIGHTED_PRED: u32 = 1 << 1;
pub(crate) const DEC_PIC_WEIGHTED_BIPRED_IDC_SHIFT: u32 = 2;
pub(crate) const DEC_PIC_TRANSFORM_8X8_MODE: u32 = 1 << 4;
pub(crate) const DEC_PIC_CONSTRAINED_INTRA_PRED: u32 = 1 << 6;
pub(crate) const DEC_PIC_PIC_ORDER_PRESENT: u32 = 1 << 7;
pub(crate) const DEC_PIC_DEBLOCKING_FILTER_CONTROL_PRESENT: u32 = 1 << 8;
pub(crate) const DEC_PIC_REDUNDANT_PIC_CNT_PRESENT: u32 = 1 << 9;
pub(crate) const DEC_PIC_REFERENCE: u32 = 1 << 10;
// VAEncMiscParameterRateControl.rc_flags
pub(crate) const RC_DISABLE_FRAME_SKIP: u32 = 1 << 1;
pub(crate) const RC_DISABLE_BIT_STUFFING: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAConfigAttrib {
    pub type_: c_int,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union VAGenericValueUnion {
    pub i: i32,
    pub f: f32,
    pub p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAGenericValue {
    pub type_: u32,
    pub value: VAGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VASurfaceAttrib {
    pub type_: u32,
    pub flags: u32,
    pub value: VAGenericValue,
}

impl VASurfaceAttrib {
    /// A settable `VASurfaceAttribPixelFormat` for `vaCreateSurfaces`.
    pub(crate) fn pixel_format(fourcc: u32) -> Self {
        Self {
            type_: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                type_: VA_GENERIC_VALUE_TYPE_INTEGER,
                value: VAGenericValueUnion { i: fourcc as i32 },
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; 4],
}

impl VAPictureH264 {
    pub(crate) const INVALID: Self = Self {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
pub(crate) struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VAEncPictureParameterBufferH264 {
    pub curr_pic: VAPictureH264,
    pub reference_frames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureH264; 32],
    pub ref_pic_list1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub icq_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VAEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

/// `VAEncMiscParameterBuffer` header (`type`) followed by its payload, as one
/// buffer — the C struct ends in a flexible `uint32_t data[]`.
#[repr(C)]
pub(crate) struct Misc<T> {
    pub type_: u32,
    pub data: T,
}

#[repr(C)]
pub(crate) struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VARectangle {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAProcColorProperties {
    pub chroma_sample_location: u8,
    pub color_range: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
pub(crate) struct VAProcPipelineParameterBuffer {
    pub surface: VASurfaceID,
    pub surface_region: *const VARectangle,
    pub surface_color_standard: u32,
    pub output_region: *const VARectangle,
    pub output_background_color: u32,
    pub output_color_standard: u32,
    pub pipeline_flags: u32,
    pub filter_flags: u32,
    pub filters: *mut VABufferID,
    pub num_filters: u32,
    pub forward_references: *mut VASurfaceID,
    pub num_forward_references: u32,
    pub backward_references: *mut VASurfaceID,
    pub num_backward_references: u32,
    pub rotation_state: u32,
    pub blend_state: *const c_void,
    pub mirror_state: u32,
    pub additional_outputs: *mut VASurfaceID,
    pub num_additional_outputs: u32,
    pub input_surface_flag: u32,
    pub output_surface_flag: u32,
    pub input_color_properties: VAProcColorProperties,
    pub output_color_properties: VAProcColorProperties,
    pub processing_mode: u32,
    pub output_hdr_metadata: *mut c_void,
    pub va_reserved: [u32; 16],
}

#[repr(C)]
pub(crate) struct VAPictureParameterBufferH264 {
    pub curr_pic: VAPictureH264,
    pub reference_frames: [VAPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    pub seq_fields: u32,
    /// Deprecated FMO fields (`va_deprecated`); always 0 here.
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub frame_num: u16,
    /// `VA_PADDING_MEDIUM`.
    pub va_reserved: [u32; 8],
}

#[repr(C)]
pub(crate) struct VAIQMatrixBufferH264 {
    /// Raster order (the header's documentation).
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub(crate) struct VASliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub ref_pic_list0: [VAPictureH264; 32],
    pub ref_pic_list1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; 4],
}

// SAFETY (all impls): plain C data; all-zero is valid and is the documented
// default ("must be zero") for every field the encoder does not set.
unsafe impl Zeroed for VAImage {}
unsafe impl Zeroed for VAPictureParameterBufferH264 {}
unsafe impl Zeroed for VAIQMatrixBufferH264 {}
unsafe impl Zeroed for VASliceParameterBufferH264 {}
unsafe impl Zeroed for VAEncSequenceParameterBufferH264 {}
unsafe impl Zeroed for VAEncPictureParameterBufferH264 {}
unsafe impl Zeroed for VAEncSliceParameterBufferH264 {}
unsafe impl Zeroed for VAEncMiscParameterRateControl {}
unsafe impl Zeroed for VAEncMiscParameterHRD {}
unsafe impl Zeroed for VAEncMiscParameterFrameRate {}
unsafe impl Zeroed for VAEncPackedHeaderParameterBuffer {}
unsafe impl Zeroed for VAProcPipelineParameterBuffer {}

/// The libva entry points the encoder calls.
pub(crate) struct Va {
    _va: Library,
    _drm: Library,
    pub get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
    pub initialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    pub terminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    pub error_str: unsafe extern "C" fn(VAStatus) -> *const c_char,
    pub query_vendor_string: unsafe extern "C" fn(VADisplay) -> *const c_char,
    pub set_error_callback:
        unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback,
    pub set_info_callback:
        unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback,
    pub max_num_profiles: unsafe extern "C" fn(VADisplay) -> c_int,
    pub max_num_entrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    pub query_config_profiles:
        unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus,
    pub query_config_entrypoints:
        unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus,
    pub get_config_attributes: unsafe extern "C" fn(
        VADisplay,
        VAProfile,
        VAEntrypoint,
        *mut VAConfigAttrib,
        c_int,
    ) -> VAStatus,
    pub create_config: unsafe extern "C" fn(
        VADisplay,
        VAProfile,
        VAEntrypoint,
        *mut VAConfigAttrib,
        c_int,
        *mut VAConfigID,
    ) -> VAStatus,
    pub destroy_config: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    pub query_surface_attributes:
        unsafe extern "C" fn(VADisplay, VAConfigID, *mut VASurfaceAttrib, *mut c_uint) -> VAStatus,
    pub create_surfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VASurfaceID,
        c_uint,
        *mut VASurfaceAttrib,
        c_uint,
    ) -> VAStatus,
    pub destroy_surfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    pub create_context: unsafe extern "C" fn(
        VADisplay,
        VAConfigID,
        c_int,
        c_int,
        c_int,
        *mut VASurfaceID,
        c_int,
        *mut VAContextID,
    ) -> VAStatus,
    pub destroy_context: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub create_buffer: unsafe extern "C" fn(
        VADisplay,
        VAContextID,
        VABufferType,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VABufferID,
    ) -> VAStatus,
    pub destroy_buffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    pub map_buffer: unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus,
    pub unmap_buffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    pub begin_picture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    pub render_picture:
        unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    pub end_picture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub sync_surface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
    pub derive_image: unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus,
    pub create_image:
        unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    pub destroy_image: unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus,
    pub get_image: unsafe extern "C" fn(
        VADisplay,
        VASurfaceID,
        c_int,
        c_int,
        c_uint,
        c_uint,
        VAImageID,
    ) -> VAStatus,
    pub put_image: unsafe extern "C" fn(
        VADisplay,
        VASurfaceID,
        VAImageID,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> VAStatus,
}

impl Va {
    /// The process-wide binding, loaded on first use.
    pub(crate) fn shared() -> Result<Arc<Self>, String> {
        static VA: OnceLock<Result<Arc<Va>, String>> = OnceLock::new();
        VA.get_or_init(|| Self::load().map(Arc::new)).clone()
    }

    fn load() -> Result<Self, String> {
        // SAFETY: loading libva runs only its initialisers.
        let va = unsafe { Library::new("libva.so.2") }
            .map_err(|e| format!("libva.so.2 not loadable ({e}) — install libva2 / libva-drm2"))?;
        // SAFETY: as above.
        let drm = unsafe { Library::new("libva-drm.so.2") }
            .map_err(|e| format!("libva-drm.so.2 not loadable ({e}) — install libva-drm2"))?;
        macro_rules! sym {
            ($lib:ident, $name:literal) => {
                // SAFETY: the symbol has the signature of the field it is stored
                // in (va.h / va_drm.h); the library outlives the copy.
                *unsafe { $lib.get($name) }.map_err(|e| {
                    format!(
                        "libva lacks {} ({e})",
                        String::from_utf8_lossy(&$name[..$name.len() - 1])
                    )
                })?
            };
        }
        Ok(Self {
            get_display_drm: sym!(drm, b"vaGetDisplayDRM\0"),
            initialize: sym!(va, b"vaInitialize\0"),
            terminate: sym!(va, b"vaTerminate\0"),
            error_str: sym!(va, b"vaErrorStr\0"),
            query_vendor_string: sym!(va, b"vaQueryVendorString\0"),
            set_error_callback: sym!(va, b"vaSetErrorCallback\0"),
            set_info_callback: sym!(va, b"vaSetInfoCallback\0"),
            max_num_profiles: sym!(va, b"vaMaxNumProfiles\0"),
            max_num_entrypoints: sym!(va, b"vaMaxNumEntrypoints\0"),
            query_config_profiles: sym!(va, b"vaQueryConfigProfiles\0"),
            query_config_entrypoints: sym!(va, b"vaQueryConfigEntrypoints\0"),
            get_config_attributes: sym!(va, b"vaGetConfigAttributes\0"),
            create_config: sym!(va, b"vaCreateConfig\0"),
            destroy_config: sym!(va, b"vaDestroyConfig\0"),
            query_surface_attributes: sym!(va, b"vaQuerySurfaceAttributes\0"),
            create_surfaces: sym!(va, b"vaCreateSurfaces\0"),
            destroy_surfaces: sym!(va, b"vaDestroySurfaces\0"),
            create_context: sym!(va, b"vaCreateContext\0"),
            destroy_context: sym!(va, b"vaDestroyContext\0"),
            create_buffer: sym!(va, b"vaCreateBuffer\0"),
            destroy_buffer: sym!(va, b"vaDestroyBuffer\0"),
            map_buffer: sym!(va, b"vaMapBuffer\0"),
            unmap_buffer: sym!(va, b"vaUnmapBuffer\0"),
            begin_picture: sym!(va, b"vaBeginPicture\0"),
            render_picture: sym!(va, b"vaRenderPicture\0"),
            end_picture: sym!(va, b"vaEndPicture\0"),
            sync_surface: sym!(va, b"vaSyncSurface\0"),
            derive_image: sym!(va, b"vaDeriveImage\0"),
            create_image: sym!(va, b"vaCreateImage\0"),
            destroy_image: sym!(va, b"vaDestroyImage\0"),
            get_image: sym!(va, b"vaGetImage\0"),
            put_image: sym!(va, b"vaPutImage\0"),
            _va: va,
            _drm: drm,
        })
    }
}

/// Pinned against libva 2.20 / VA-API 1.20 (`sizeof`/`offsetof` from gcc,
/// x86_64 / LP64). Only LP64 targets are built for Linux.
#[cfg(target_pointer_width = "64")]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    const _: () = assert!(size_of::<VAConfigAttrib>() == 8);
    const _: () = assert!(size_of::<VAGenericValue>() == 16);
    const _: () = assert!(size_of::<VASurfaceAttrib>() == 24);
    const _: () = assert!(offset_of!(VASurfaceAttrib, value) == 8);
    const _: () = assert!(size_of::<VAImageFormat>() == 48);
    const _: () = assert!(size_of::<VAImage>() == 120);
    const _: () = assert!(offset_of!(VAImage, buf) == 52);
    const _: () = assert!(offset_of!(VAImage, width) == 56);
    const _: () = assert!(offset_of!(VAImage, data_size) == 60);
    const _: () = assert!(offset_of!(VAImage, pitches) == 68);
    const _: () = assert!(offset_of!(VAImage, offsets) == 80);
    const _: () = assert!(offset_of!(VAImage, va_reserved) == 104);
    const _: () = assert!(size_of::<VACodedBufferSegment>() == 48);
    const _: () = assert!(offset_of!(VACodedBufferSegment, buf) == 16);
    const _: () = assert!(offset_of!(VACodedBufferSegment, next) == 24);
    const _: () = assert!(size_of::<VAPictureH264>() == 36);
    type Seq = VAEncSequenceParameterBufferH264;
    const _: () = assert!(size_of::<Seq>() == 1132);
    const _: () = assert!(offset_of!(Seq, intra_period) == 4);
    const _: () = assert!(offset_of!(Seq, picture_width_in_mbs) == 24);
    const _: () = assert!(offset_of!(Seq, seq_fields) == 28);
    const _: () = assert!(offset_of!(Seq, bit_depth_luma_minus8) == 32);
    const _: () = assert!(offset_of!(Seq, offset_for_non_ref_pic) == 36);
    const _: () = assert!(offset_of!(Seq, offset_for_ref_frame) == 44);
    const _: () = assert!(offset_of!(Seq, frame_cropping_flag) == 1068);
    const _: () = assert!(offset_of!(Seq, frame_crop_left_offset) == 1072);
    const _: () = assert!(offset_of!(Seq, vui_parameters_present_flag) == 1088);
    const _: () = assert!(offset_of!(Seq, vui_fields) == 1092);
    const _: () = assert!(offset_of!(Seq, aspect_ratio_idc) == 1096);
    const _: () = assert!(offset_of!(Seq, sar_width) == 1100);
    const _: () = assert!(offset_of!(Seq, time_scale) == 1112);
    const _: () = assert!(offset_of!(Seq, va_reserved) == 1116);
    type Pic = VAEncPictureParameterBufferH264;
    const _: () = assert!(size_of::<Pic>() == 648);
    const _: () = assert!(offset_of!(Pic, reference_frames) == 36);
    const _: () = assert!(offset_of!(Pic, coded_buf) == 612);
    const _: () = assert!(offset_of!(Pic, pic_parameter_set_id) == 616);
    const _: () = assert!(offset_of!(Pic, frame_num) == 620);
    const _: () = assert!(offset_of!(Pic, pic_init_qp) == 622);
    const _: () = assert!(offset_of!(Pic, pic_fields) == 628);
    const _: () = assert!(offset_of!(Pic, va_reserved) == 632);
    type Slice = VAEncSliceParameterBufferH264;
    const _: () = assert!(size_of::<Slice>() == 3140);
    const _: () = assert!(offset_of!(Slice, slice_type) == 12);
    const _: () = assert!(offset_of!(Slice, idr_pic_id) == 14);
    const _: () = assert!(offset_of!(Slice, pic_order_cnt_lsb) == 16);
    const _: () = assert!(offset_of!(Slice, delta_pic_order_cnt_bottom) == 20);
    const _: () = assert!(offset_of!(Slice, direct_spatial_mv_pred_flag) == 32);
    const _: () = assert!(offset_of!(Slice, ref_pic_list0) == 36);
    const _: () = assert!(offset_of!(Slice, ref_pic_list1) == 1188);
    const _: () = assert!(offset_of!(Slice, luma_log2_weight_denom) == 2340);
    const _: () = assert!(offset_of!(Slice, luma_weight_l0) == 2344);
    const _: () = assert!(offset_of!(Slice, chroma_weight_l0_flag) == 2472);
    const _: () = assert!(offset_of!(Slice, chroma_weight_l0) == 2474);
    const _: () = assert!(offset_of!(Slice, luma_weight_l1_flag) == 2730);
    const _: () = assert!(offset_of!(Slice, chroma_weight_l1_flag) == 2860);
    const _: () = assert!(offset_of!(Slice, chroma_offset_l1) == 2990);
    const _: () = assert!(offset_of!(Slice, cabac_init_idc) == 3118);
    const _: () = assert!(offset_of!(Slice, va_reserved) == 3124);
    const _: () = assert!(size_of::<VAEncMiscParameterRateControl>() == 60);
    const _: () = assert!(offset_of!(VAEncMiscParameterRateControl, rc_flags) == 24);
    const _: () = assert!(offset_of!(VAEncMiscParameterRateControl, max_qp) == 32);
    const _: () = assert!(size_of::<VAEncMiscParameterHRD>() == 24);
    const _: () = assert!(size_of::<VAEncMiscParameterFrameRate>() == 24);
    // Header + payload with no padding between them (payload is 4-aligned).
    const _: () = assert!(offset_of!(Misc<VAEncMiscParameterRateControl>, data) == 4);
    const _: () = assert!(size_of::<VAEncPackedHeaderParameterBuffer>() == 28);
    const _: () = assert!(offset_of!(VAEncPackedHeaderParameterBuffer, va_reserved) == 12);
    const _: () = assert!(size_of::<VARectangle>() == 8);
    const _: () = assert!(size_of::<VAProcColorProperties>() == 8);
    type Proc = VAProcPipelineParameterBuffer;
    const _: () = assert!(size_of::<Proc>() == 224);
    const _: () = assert!(offset_of!(Proc, surface_region) == 8);
    const _: () = assert!(offset_of!(Proc, surface_color_standard) == 16);
    const _: () = assert!(offset_of!(Proc, output_region) == 24);
    const _: () = assert!(offset_of!(Proc, output_background_color) == 32);
    const _: () = assert!(offset_of!(Proc, output_color_standard) == 36);
    const _: () = assert!(offset_of!(Proc, pipeline_flags) == 40);
    const _: () = assert!(offset_of!(Proc, filters) == 48);
    const _: () = assert!(offset_of!(Proc, num_filters) == 56);
    const _: () = assert!(offset_of!(Proc, forward_references) == 64);
    const _: () = assert!(offset_of!(Proc, backward_references) == 80);
    const _: () = assert!(offset_of!(Proc, rotation_state) == 92);
    const _: () = assert!(offset_of!(Proc, blend_state) == 96);
    const _: () = assert!(offset_of!(Proc, mirror_state) == 104);
    const _: () = assert!(offset_of!(Proc, additional_outputs) == 112);
    const _: () = assert!(offset_of!(Proc, num_additional_outputs) == 120);
    const _: () = assert!(offset_of!(Proc, input_surface_flag) == 124);
    const _: () = assert!(offset_of!(Proc, input_color_properties) == 132);
    const _: () = assert!(offset_of!(Proc, output_color_properties) == 140);
    const _: () = assert!(offset_of!(Proc, processing_mode) == 148);
    const _: () = assert!(offset_of!(Proc, output_hdr_metadata) == 152);
    const _: () = assert!(offset_of!(Proc, va_reserved) == 160);
    // Decode (task L4): same method, libva 2.20 headers, gcc 13.3 x86_64.
    type DecPic = VAPictureParameterBufferH264;
    const _: () = assert!(size_of::<DecPic>() == 672);
    const _: () = assert!(offset_of!(DecPic, reference_frames) == 36);
    const _: () = assert!(offset_of!(DecPic, picture_width_in_mbs_minus1) == 612);
    const _: () = assert!(offset_of!(DecPic, bit_depth_luma_minus8) == 616);
    const _: () = assert!(offset_of!(DecPic, num_ref_frames) == 618);
    const _: () = assert!(offset_of!(DecPic, seq_fields) == 620);
    const _: () = assert!(offset_of!(DecPic, num_slice_groups_minus1) == 624);
    const _: () = assert!(offset_of!(DecPic, slice_group_change_rate_minus1) == 626);
    const _: () = assert!(offset_of!(DecPic, pic_init_qp_minus26) == 628);
    const _: () = assert!(offset_of!(DecPic, second_chroma_qp_index_offset) == 631);
    const _: () = assert!(offset_of!(DecPic, pic_fields) == 632);
    const _: () = assert!(offset_of!(DecPic, frame_num) == 636);
    const _: () = assert!(offset_of!(DecPic, va_reserved) == 640);
    const _: () = assert!(size_of::<VAIQMatrixBufferH264>() == 240);
    const _: () = assert!(offset_of!(VAIQMatrixBufferH264, scaling_list_8x8) == 96);
    const _: () = assert!(offset_of!(VAIQMatrixBufferH264, va_reserved) == 224);
    type DecSlice = VASliceParameterBufferH264;
    const _: () = assert!(size_of::<DecSlice>() == 3128);
    const _: () = assert!(offset_of!(DecSlice, slice_data_flag) == 8);
    const _: () = assert!(offset_of!(DecSlice, slice_data_bit_offset) == 12);
    const _: () = assert!(offset_of!(DecSlice, first_mb_in_slice) == 14);
    const _: () = assert!(offset_of!(DecSlice, slice_type) == 16);
    const _: () = assert!(offset_of!(DecSlice, num_ref_idx_l0_active_minus1) == 18);
    const _: () = assert!(offset_of!(DecSlice, cabac_init_idc) == 20);
    const _: () = assert!(offset_of!(DecSlice, slice_beta_offset_div2) == 24);
    const _: () = assert!(offset_of!(DecSlice, ref_pic_list0) == 28);
    const _: () = assert!(offset_of!(DecSlice, ref_pic_list1) == 1180);
    const _: () = assert!(offset_of!(DecSlice, luma_log2_weight_denom) == 2332);
    const _: () = assert!(offset_of!(DecSlice, luma_weight_l0_flag) == 2334);
    const _: () = assert!(offset_of!(DecSlice, luma_weight_l0) == 2336);
    const _: () = assert!(offset_of!(DecSlice, luma_offset_l0) == 2400);
    const _: () = assert!(offset_of!(DecSlice, chroma_weight_l0_flag) == 2464);
    const _: () = assert!(offset_of!(DecSlice, chroma_weight_l0) == 2466);
    const _: () = assert!(offset_of!(DecSlice, chroma_offset_l0) == 2594);
    const _: () = assert!(offset_of!(DecSlice, luma_weight_l1_flag) == 2722);
    const _: () = assert!(offset_of!(DecSlice, luma_weight_l1) == 2724);
    const _: () = assert!(offset_of!(DecSlice, luma_offset_l1) == 2788);
    const _: () = assert!(offset_of!(DecSlice, chroma_weight_l1_flag) == 2852);
    const _: () = assert!(offset_of!(DecSlice, chroma_weight_l1) == 2854);
    const _: () = assert!(offset_of!(DecSlice, chroma_offset_l1) == 2982);
    const _: () = assert!(offset_of!(DecSlice, va_reserved) == 3112);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitfield_positions_match_gcc() {
        // Values printed by gcc for the same fields in the real headers.
        assert_eq!(RC_DISABLE_FRAME_SKIP | RC_DISABLE_BIT_STUFFING, 0x6);
        assert_eq!(0xf << SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT, 0x3c0);
        assert_eq!(0x3 << SEQ_PIC_ORDER_CNT_TYPE_SHIFT, 0xc00);
        assert_eq!(0x1f << VUI_LOG2_MAX_MV_LENGTH_VERTICAL_SHIFT, 0x1f00);
        assert_eq!(0x3 << PIC_REFERENCE_SHIFT, 0x6);
        assert_eq!(PIC_DEBLOCKING_FILTER_CONTROL_PRESENT, 0x200);
        assert_eq!(PIC_ENTROPY_CODING_MODE, 0x8);
    }

    #[test]
    fn decode_bitfield_positions_match_gcc() {
        // gcc's value of `seq_fields.value` / `pic_fields.value` with each
        // field set to its maximum in the real VAPictureParameterBufferH264.
        assert_eq!(3 << DEC_SEQ_CHROMA_FORMAT_IDC_SHIFT, 0x3);
        assert_eq!(DEC_SEQ_GAPS_IN_FRAME_NUM_ALLOWED, 0x8);
        assert_eq!(DEC_SEQ_FRAME_MBS_ONLY, 0x10);
        assert_eq!(DEC_SEQ_DIRECT_8X8_INFERENCE, 0x40);
        assert_eq!(DEC_SEQ_MIN_LUMA_BI_PRED_SIZE_8X8, 0x80);
        assert_eq!(0xf << DEC_SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT, 0xf00);
        assert_eq!(0x3 << DEC_SEQ_PIC_ORDER_CNT_TYPE_SHIFT, 0x3000);
        assert_eq!(0xf << DEC_SEQ_LOG2_MAX_POC_LSB_MINUS4_SHIFT, 0x3c000);
        assert_eq!(DEC_SEQ_DELTA_PIC_ORDER_ALWAYS_ZERO, 0x40000);
        assert_eq!(DEC_PIC_ENTROPY_CODING_MODE, 0x1);
        assert_eq!(DEC_PIC_WEIGHTED_PRED, 0x2);
        assert_eq!(0x3 << DEC_PIC_WEIGHTED_BIPRED_IDC_SHIFT, 0xc);
        assert_eq!(DEC_PIC_TRANSFORM_8X8_MODE, 0x10);
        assert_eq!(DEC_PIC_CONSTRAINED_INTRA_PRED, 0x40);
        assert_eq!(DEC_PIC_PIC_ORDER_PRESENT, 0x80);
        assert_eq!(DEC_PIC_DEBLOCKING_FILTER_CONTROL_PRESENT, 0x100);
        assert_eq!(DEC_PIC_REDUNDANT_PIC_CNT_PRESENT, 0x200);
        assert_eq!(DEC_PIC_REFERENCE, 0x400);
    }

    #[test]
    fn fourccs_are_memory_order() {
        assert_eq!(&VA_FOURCC_BGRA.to_le_bytes(), b"BGRA");
        assert_eq!(&VA_FOURCC_BGRX.to_le_bytes(), b"BGRX");
        assert_eq!(&VA_FOURCC_NV12.to_le_bytes(), b"NV12");
    }
}
