//! Minimal runtime binding of NVDEC (`cuviddec.h` + `nvcuvid.h`, as
//! `dynlink_cuviddec.h` / `dynlink_nvcuvid.h` in FFmpeg's MIT-licensed
//! nv-codec-headers n12.0.16.1), loaded from `libnvcuvid.so.1`, which ships
//! with the proprietary driver.
//!
//! Only what the decoder uses is declared. `CUVIDPICPARAMS` (the per-codec
//! picture parameters) is deliberately opaque: the driver's own parser
//! produces it and the decoder only passes the pointer back to
//! `cuvidDecodePicture`. Sizes and offsets are pinned by `const` asserts
//! generated from those headers with gcc 13.3 on x86_64 (LP64: `tcu_ulong` is
//! `unsigned long`, 8 bytes), so a layout slip fails the build.

use std::ffi::{c_int, c_uint, c_ulong, c_void};
use std::sync::{Arc, OnceLock};

use libloading::Library;

use crate::codec::cuda::CuResult;
use crate::codec::zeroed::Zeroed;

pub(crate) type CuvidDecoder = *mut c_void;
pub(crate) type CuvidParser = *mut c_void;

// cudaVideoCodec
pub(crate) const CODEC_H264: c_int = 4;
pub(crate) const CODEC_HEVC: c_int = 8;
pub(crate) const CODEC_AV1: c_int = 11;
// cudaVideoChromaFormat
pub(crate) const CHROMA_420: c_int = 1;
// cudaVideoSurfaceFormat
pub(crate) const SURFACE_NV12: c_int = 0;
// cudaVideoDeinterlaceMode
pub(crate) const DEINTERLACE_WEAVE: c_int = 0;
pub(crate) const DEINTERLACE_ADAPTIVE: c_int = 2;
// cudaVideoCreateFlags: the dedicated decode engines.
pub(crate) const CREATE_PREFER_CUVID: c_ulong = 0x4;
// CUvideopacketflags
pub(crate) const PKT_TIMESTAMP: c_ulong = 0x2;
pub(crate) const PKT_ENDOFPICTURE: c_ulong = 0x8;
// cuvidDecodeStatus
pub(crate) const DECODE_STATUS_ERROR: c_int = 8;
pub(crate) const DECODE_STATUS_ERROR_CONCEALED: c_int = 9;

/// `CUVIDDECODECAPS`.
#[repr(C)]
pub(crate) struct CuvidDecodeCaps {
    pub codec_type: c_int,
    pub chroma_format: c_int,
    pub bit_depth_minus8: c_uint,
    pub reserved1: [c_uint; 3],
    pub is_supported: u8,
    pub num_nvdecs: u8,
    pub output_format_mask: u16,
    pub max_width: c_uint,
    pub max_height: c_uint,
    pub max_mb_count: c_uint,
    pub min_width: u16,
    pub min_height: u16,
    pub is_histogram_supported: u8,
    pub counter_bit_depth: u8,
    pub max_histogram_bins: u16,
    pub reserved3: [c_uint; 10],
}

/// The `short` rectangle of `CUVIDDECODECREATEINFO`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Rect16 {
    pub left: i16,
    pub top: i16,
    pub right: i16,
    pub bottom: i16,
}

/// `CUVIDDECODECREATEINFO`.
#[repr(C)]
pub(crate) struct CuvidDecodeCreateInfo {
    pub width: c_ulong,
    pub height: c_ulong,
    pub num_decode_surfaces: c_ulong,
    pub codec_type: c_int,
    pub chroma_format: c_int,
    pub creation_flags: c_ulong,
    pub bit_depth_minus8: c_ulong,
    pub intra_decode_only: c_ulong,
    pub max_width: c_ulong,
    pub max_height: c_ulong,
    pub reserved1: c_ulong,
    pub display_area: Rect16,
    pub output_format: c_int,
    pub deinterlace_mode: c_int,
    pub target_width: c_ulong,
    pub target_height: c_ulong,
    pub num_output_surfaces: c_ulong,
    pub vid_lock: *mut c_void,
    pub target_rect: Rect16,
    pub enable_histogram: c_ulong,
    pub reserved2: [c_ulong; 4],
}

/// The `int` rectangle of `CUVIDEOFORMAT::display_area`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rect32 {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

/// `CUVIDEOFORMAT` (what the sequence callback receives).
#[repr(C)]
pub(crate) struct CuvideoFormat {
    pub codec: c_int,
    pub frame_rate_numerator: c_uint,
    pub frame_rate_denominator: c_uint,
    pub progressive_sequence: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub min_num_decode_surfaces: u8,
    pub coded_width: c_uint,
    pub coded_height: c_uint,
    pub display_area: Rect32,
    pub chroma_format: c_int,
    pub bitrate: c_uint,
    pub display_aspect_ratio: [c_int; 2],
    /// `video_format:3, video_full_range_flag:1, reserved:4`, then colour
    /// primaries, transfer characteristics, matrix coefficients.
    pub video_signal_description: [u8; 4],
    pub seqhdr_data_length: c_uint,
}

/// `CUVIDPARSERDISPINFO`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct CuvidParserDispInfo {
    pub picture_index: c_int,
    pub progressive_frame: c_int,
    pub top_field_first: c_int,
    pub repeat_first_field: c_int,
    pub timestamp: i64,
}

pub(crate) type SequenceCallback =
    Option<unsafe extern "C" fn(*mut c_void, *mut CuvideoFormat) -> c_int>;
/// The second argument is `CUVIDPICPARAMS *`, kept opaque.
pub(crate) type DecodeCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>;
pub(crate) type DisplayCallback =
    Option<unsafe extern "C" fn(*mut c_void, *mut CuvidParserDispInfo) -> c_int>;
/// Operating-point and SEI callbacks are not used (null).
pub(crate) type UnusedCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>;

/// `CUVIDPARSERPARAMS`.
#[repr(C)]
pub(crate) struct CuvidParserParams {
    pub codec_type: c_int,
    pub max_num_decode_surfaces: c_uint,
    pub clock_rate: c_uint,
    pub error_threshold: c_uint,
    pub max_display_delay: c_uint,
    /// `bAnnexb:1` (AV1 Annex-B framing) + 31 reserved bits: 0 here.
    pub annexb_reserved: c_uint,
    pub reserved1: [c_uint; 4],
    pub user_data: *mut c_void,
    pub sequence_callback: SequenceCallback,
    pub decode_picture: DecodeCallback,
    pub display_picture: DisplayCallback,
    pub get_operating_point: UnusedCallback,
    pub get_sei_msg: UnusedCallback,
    pub reserved2: [*mut c_void; 5],
    pub ext_video_info: *mut c_void,
}

/// `CUVIDSOURCEDATAPACKET`.
#[repr(C)]
pub(crate) struct CuvidSourceDataPacket {
    pub flags: c_ulong,
    pub payload_size: c_ulong,
    pub payload: *const u8,
    pub timestamp: i64,
}

/// `CUVIDPROCPARAMS`.
#[repr(C)]
pub(crate) struct CuvidProcParams {
    pub progressive_frame: c_int,
    pub second_field: c_int,
    pub top_field_first: c_int,
    pub unpaired_field: c_int,
    pub reserved_flags: c_uint,
    pub reserved_zero: c_uint,
    pub raw_input_dptr: u64,
    pub raw_input_pitch: c_uint,
    pub raw_input_format: c_uint,
    pub raw_output_dptr: u64,
    pub raw_output_pitch: c_uint,
    pub reserved1: c_uint,
    pub output_stream: *mut c_void,
    pub reserved: [c_uint; 46],
    pub histogram_dptr: *mut u64,
    pub reserved2: [*mut c_void; 1],
}

/// `CUVIDGETDECODESTATUS`.
#[repr(C)]
pub(crate) struct CuvidGetDecodeStatus {
    pub decode_status: c_int,
    pub reserved: [c_uint; 31],
    pub p_reserved: [*mut c_void; 8],
}

// SAFETY (all impls): plain C data (integers, raw pointers, `Option` of
// function pointers); all-zero is valid and the headers' documented default
// ("set to zero" / NULL) for every field the decoder does not set.
unsafe impl Zeroed for CuvidDecodeCaps {}
unsafe impl Zeroed for CuvidDecodeCreateInfo {}
unsafe impl Zeroed for CuvidParserParams {}
unsafe impl Zeroed for CuvidProcParams {}
unsafe impl Zeroed for CuvidGetDecodeStatus {}

/// The `libnvcuvid.so.1` entry points the decoder calls.
pub(crate) struct Cuvid {
    _lib: Library,
    pub get_decoder_caps: unsafe extern "C" fn(*mut CuvidDecodeCaps) -> CuResult,
    pub create_decoder:
        unsafe extern "C" fn(*mut CuvidDecoder, *mut CuvidDecodeCreateInfo) -> CuResult,
    pub destroy_decoder: unsafe extern "C" fn(CuvidDecoder) -> CuResult,
    pub decode_picture: unsafe extern "C" fn(CuvidDecoder, *mut c_void) -> CuResult,
    pub get_decode_status:
        unsafe extern "C" fn(CuvidDecoder, c_int, *mut CuvidGetDecodeStatus) -> CuResult,
    pub map_video_frame: unsafe extern "C" fn(
        CuvidDecoder,
        c_int,
        *mut u64,
        *mut c_uint,
        *mut CuvidProcParams,
    ) -> CuResult,
    pub unmap_video_frame: unsafe extern "C" fn(CuvidDecoder, u64) -> CuResult,
    pub create_video_parser:
        unsafe extern "C" fn(*mut CuvidParser, *mut CuvidParserParams) -> CuResult,
    pub parse_video_data: unsafe extern "C" fn(CuvidParser, *mut CuvidSourceDataPacket) -> CuResult,
    pub destroy_video_parser: unsafe extern "C" fn(CuvidParser) -> CuResult,
}

impl Cuvid {
    /// The process-wide binding, loaded on first use.
    pub(crate) fn shared() -> Result<Arc<Self>, String> {
        static CUVID: OnceLock<Result<Arc<Cuvid>, String>> = OnceLock::new();
        CUVID.get_or_init(|| Self::load().map(Arc::new)).clone()
    }

    fn load() -> Result<Self, String> {
        // SAFETY: loading the driver's NVDEC library runs only its initialisers.
        let lib = unsafe { Library::new("libnvcuvid.so.1") }.map_err(|e| {
            format!("libnvcuvid.so.1 not loadable ({e}) — no NVIDIA driver with NVDEC")
        })?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol has the signature of the field it is stored
                // in (dynlink_cuviddec.h / dynlink_nvcuvid.h; the `64` map
                // variants are the LP64 ones FFmpeg's loader binds); the
                // library outlives the copy (`_lib`).
                *unsafe { lib.get($name) }.map_err(|e| {
                    format!(
                        "libnvcuvid.so.1 lacks {} ({e}) — the NVIDIA driver is too old",
                        String::from_utf8_lossy(&$name[..$name.len() - 1])
                    )
                })?
            };
        }
        Ok(Self {
            get_decoder_caps: sym!(b"cuvidGetDecoderCaps\0"),
            create_decoder: sym!(b"cuvidCreateDecoder\0"),
            destroy_decoder: sym!(b"cuvidDestroyDecoder\0"),
            decode_picture: sym!(b"cuvidDecodePicture\0"),
            get_decode_status: sym!(b"cuvidGetDecodeStatus\0"),
            map_video_frame: sym!(b"cuvidMapVideoFrame64\0"),
            unmap_video_frame: sym!(b"cuvidUnmapVideoFrame64\0"),
            create_video_parser: sym!(b"cuvidCreateVideoParser\0"),
            parse_video_data: sym!(b"cuvidParseVideoData\0"),
            destroy_video_parser: sym!(b"cuvidDestroyVideoParser\0"),
            _lib: lib,
        })
    }
}

/// Pinned against nv-codec-headers n12.0.16.1 (`sizeof`/`offsetof` from gcc
/// 13.3, x86_64 / LP64). Only LP64 targets are built for Linux.
#[cfg(target_pointer_width = "64")]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    type Caps = CuvidDecodeCaps;
    const _: () = assert!(size_of::<Caps>() == 88);
    const _: () = assert!(offset_of!(Caps, is_supported) == 24);
    const _: () = assert!(offset_of!(Caps, num_nvdecs) == 25);
    const _: () = assert!(offset_of!(Caps, output_format_mask) == 26);
    const _: () = assert!(offset_of!(Caps, max_width) == 28);
    const _: () = assert!(offset_of!(Caps, max_mb_count) == 36);
    const _: () = assert!(offset_of!(Caps, min_width) == 40);
    const _: () = assert!(offset_of!(Caps, min_height) == 42);
    const _: () = assert!(offset_of!(Caps, is_histogram_supported) == 44);
    const _: () = assert!(offset_of!(Caps, max_histogram_bins) == 46);
    const _: () = assert!(offset_of!(Caps, reserved3) == 48);
    type Create = CuvidDecodeCreateInfo;
    const _: () = assert!(size_of::<Create>() == 176);
    const _: () = assert!(offset_of!(Create, num_decode_surfaces) == 16);
    const _: () = assert!(offset_of!(Create, codec_type) == 24);
    const _: () = assert!(offset_of!(Create, chroma_format) == 28);
    const _: () = assert!(offset_of!(Create, creation_flags) == 32);
    const _: () = assert!(offset_of!(Create, max_width) == 56);
    const _: () = assert!(offset_of!(Create, reserved1) == 72);
    const _: () = assert!(offset_of!(Create, display_area) == 80);
    const _: () = assert!(offset_of!(Create, output_format) == 88);
    const _: () = assert!(offset_of!(Create, deinterlace_mode) == 92);
    const _: () = assert!(offset_of!(Create, target_width) == 96);
    const _: () = assert!(offset_of!(Create, num_output_surfaces) == 112);
    const _: () = assert!(offset_of!(Create, vid_lock) == 120);
    const _: () = assert!(offset_of!(Create, target_rect) == 128);
    const _: () = assert!(offset_of!(Create, enable_histogram) == 136);
    const _: () = assert!(offset_of!(Create, reserved2) == 144);
    type Fmt = CuvideoFormat;
    const _: () = assert!(size_of::<Fmt>() == 64);
    const _: () = assert!(offset_of!(Fmt, frame_rate_numerator) == 4);
    const _: () = assert!(offset_of!(Fmt, progressive_sequence) == 12);
    const _: () = assert!(offset_of!(Fmt, bit_depth_luma_minus8) == 13);
    const _: () = assert!(offset_of!(Fmt, min_num_decode_surfaces) == 15);
    const _: () = assert!(offset_of!(Fmt, coded_width) == 16);
    const _: () = assert!(offset_of!(Fmt, display_area) == 24);
    const _: () = assert!(offset_of!(Fmt, chroma_format) == 40);
    const _: () = assert!(offset_of!(Fmt, bitrate) == 44);
    const _: () = assert!(offset_of!(Fmt, display_aspect_ratio) == 48);
    const _: () = assert!(offset_of!(Fmt, video_signal_description) == 56);
    const _: () = assert!(offset_of!(Fmt, seqhdr_data_length) == 60);
    type Parser = CuvidParserParams;
    const _: () = assert!(size_of::<Parser>() == 136);
    const _: () = assert!(offset_of!(Parser, max_num_decode_surfaces) == 4);
    const _: () = assert!(offset_of!(Parser, max_display_delay) == 16);
    const _: () = assert!(offset_of!(Parser, reserved1) == 24);
    const _: () = assert!(offset_of!(Parser, user_data) == 40);
    const _: () = assert!(offset_of!(Parser, sequence_callback) == 48);
    const _: () = assert!(offset_of!(Parser, decode_picture) == 56);
    const _: () = assert!(offset_of!(Parser, display_picture) == 64);
    const _: () = assert!(offset_of!(Parser, get_operating_point) == 72);
    const _: () = assert!(offset_of!(Parser, get_sei_msg) == 80);
    const _: () = assert!(offset_of!(Parser, reserved2) == 88);
    const _: () = assert!(offset_of!(Parser, ext_video_info) == 128);
    type Packet = CuvidSourceDataPacket;
    const _: () = assert!(size_of::<Packet>() == 32);
    const _: () = assert!(offset_of!(Packet, payload_size) == 8);
    const _: () = assert!(offset_of!(Packet, payload) == 16);
    const _: () = assert!(offset_of!(Packet, timestamp) == 24);
    type Disp = CuvidParserDispInfo;
    const _: () = assert!(size_of::<Disp>() == 24);
    const _: () = assert!(offset_of!(Disp, progressive_frame) == 4);
    const _: () = assert!(offset_of!(Disp, top_field_first) == 8);
    const _: () = assert!(offset_of!(Disp, timestamp) == 16);
    type Proc = CuvidProcParams;
    const _: () = assert!(size_of::<Proc>() == 264);
    const _: () = assert!(offset_of!(Proc, unpaired_field) == 12);
    const _: () = assert!(offset_of!(Proc, raw_input_dptr) == 24);
    const _: () = assert!(offset_of!(Proc, raw_output_dptr) == 40);
    const _: () = assert!(offset_of!(Proc, output_stream) == 56);
    const _: () = assert!(offset_of!(Proc, reserved) == 64);
    const _: () = assert!(offset_of!(Proc, histogram_dptr) == 248);
    const _: () = assert!(offset_of!(Proc, reserved2) == 256);
    const _: () = assert!(size_of::<CuvidGetDecodeStatus>() == 192);
    const _: () = assert!(offset_of!(CuvidGetDecodeStatus, p_reserved) == 128);
}
