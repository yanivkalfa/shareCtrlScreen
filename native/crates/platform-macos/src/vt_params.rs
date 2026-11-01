//! The VideoToolbox encoder's tuning rules and error wording, kept free of OS
//! calls so each rule is unit-tested on its own (`vt_encoder` applies them).
//!
//! The recipe is the one every backend reproduces (`platform_api::VideoEncoder`
//! docs; Media Foundation in `codec/src/encode.rs`, NVENC/VA-API in
//! `platform-linux/src/codec`): rate control at the live bitrate, no
//! reordering, keyframes only on request, a max-QP quality floor of 32, levels
//! from size and frame rate, speed over quality.
//!
//! Where VideoToolbox differs:
//! * **Low-latency rate control** (`EnableLowLatencyRateControl`) is the
//!   preferred session: Apple documents it as infinite GOP, no reordering or
//!   look-ahead, "only High profiles" with the level set by the encoder — so
//!   it runs H.264 *Constrained* High (High without B-frames) at auto level.
//!   Encoders that refuse it get the classic session with the plan's Main
//!   profile and a level chosen here exactly as L3 chooses it.
//! * **Burst limit.** VideoToolbox's `DataRateLimits` is a hard cap over a
//!   sliding window, not a VBV size. A one-frame window (L3's single-frame
//!   VBV) would make the encoder drop or degrade every keyframe; WebRTC's
//!   VideoToolbox encoder caps at 1.5 × the target over one second, which
//!   bounds a burst to well under a second of link time while leaving the
//!   QP floor in charge of legibility. When the floor and the budget collide
//!   the encoder drops frames (WWDC21 10158) — the same "frame rate gives,
//!   sharpness stays" trade the host's source pacing makes.
//!
//! ## References
//! * `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`:
//!   <https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol>
//! * WWDC21 10158 "Explore low-latency video encoding with VideoToolbox"
//!   (Constrained High, `MaxAllowedFrameQP` 1–51, frame drops at the QP cap):
//!   <https://developer.apple.com/videos/play/wwdc2021/10158/>
//! * `kVTCompressionPropertyKey_DataRateLimits` ("zero, one, or two hard
//!   limits… alternating between bytes and seconds"):
//!   <https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_dataratelimits>
//! * `kVTCompressionPropertyKey_MaxKeyFrameInterval` (default 0 = "the video
//!   encoder should choose where to place all key frames"):
//!   <https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_maxkeyframeinterval>
//! * VideoToolbox result codes (`VTErrors.h`):
//!   <https://developer.apple.com/documentation/videotoolbox/videotoolbox-constants>
//! * WebRTC `sdk/objc/components/video_codec/RTCVideoEncoderH264.mm`
//!   (`kLimitToAverageBitRateFactor = 1.5`, one-second `DataRateLimits`
//!   window, `ForceKeyFrame` frame option).
//! * ITU-T H.264 (08/2021) Annex A, Table A-1 and §A.3.1 — same table and
//!   rules as `platform-linux/src/codec/level.rs`.

use objc2_video_toolbox::{
    kVTAllocationFailedErr, kVTCouldNotCreateInstanceErr, kVTCouldNotFindVideoEncoderErr,
    kVTInvalidSessionErr, kVTParameterErr, kVTPixelTransferNotPermittedErr,
    kVTPixelTransferNotSupportedErr, kVTPropertyNotSupportedErr, kVTPropertyReadOnlyErr,
    kVTSessionMalfunctionErr, kVTVideoEncoderAuthorizationErr, kVTVideoEncoderMalfunctionErr,
    kVTVideoEncoderNeedsRosettaErr, kVTVideoEncoderNotAvailableNowErr,
};
use platform_api::{CodecError, EncoderConfig};

/// Quality floor: the worst quantizer any picture may use — the same value
/// and reasoning as Windows (`codec::encode::MAX_QP`) and Linux: past ~32
/// text stops being legible.
pub(crate) const MAX_QP: i32 = 32;

/// The burst cap over [`DATA_RATE_WINDOW_SECS`], as a multiple of the target
/// (numerator / denominator) — WebRTC's `kLimitToAverageBitRateFactor`.
const PEAK_NUM: u64 = 3;
const PEAK_DEN: u64 = 2;

/// The `DataRateLimits` window.
pub(crate) const DATA_RATE_WINDOW_SECS: f64 = 1.0;

/// "No periodic keyframes": VideoToolbox's default of 0 lets the encoder
/// place keyframes wherever it likes, so the interval is set to the largest
/// value the property can hold. The engine forces every keyframe it needs.
pub(crate) const KEYFRAME_INTERVAL_FRAMES: i32 = i32::MAX;
/// Fallback when an encoder refuses [`KEYFRAME_INTERVAL_FRAMES`]: WebRTC's
/// 7200 frames (two minutes at 60 fps) — still rare enough that the
/// periodic keyframe is noise next to the ones the engine forces.
pub(crate) const KEYFRAME_INTERVAL_FALLBACK: i32 = 7200;

/// The bitrate every declared level must allow (as `platform-linux`'s
/// `BITRATE_CEILING_BPS`): the live bitrate moves without a new SPS, so the
/// level is chosen for the most the stream will ever carry, not the start.
const BITRATE_CEILING_BPS: u64 = 50_000_000;

/// Refuse configurations no encoder can honour, before a session exists
/// (a zero rate divides by zero in the rate maths; zero sizes make
/// VideoToolbox fail with a bare `kVTParameterErr`).
pub(crate) fn validate(cfg: &EncoderConfig) -> Result<(), CodecError> {
    let problem = if cfg.width < 2 || cfg.height < 2 {
        "a width or height below 2"
    } else if cfg.width > i32::MAX as u32 || cfg.height > i32::MAX as u32 {
        "a width or height too large for VideoToolbox"
    } else if cfg.fps_num == 0 || cfg.fps_den == 0 {
        "a zero frame rate numerator or denominator"
    } else if cfg.bitrate_bps == 0 {
        "a zero bitrate"
    } else {
        return Ok(());
    };
    Err(CodecError::Unsupported(format!(
        "encoder config has {problem}: {}x{} @{}/{} fps, {} bps",
        cfg.width, cfg.height, cfg.fps_num, cfg.fps_den, cfg.bitrate_bps
    )))
}

/// 4:2:0 chroma is subsampled 2×2: odd sizes are rounded down by one pixel
/// (as on Linux) rather than refused. Sizes below 2 are left for [`validate`].
pub(crate) fn even_size(mut cfg: EncoderConfig) -> EncoderConfig {
    if cfg.width >= 2 {
        cfg.width &= !1;
    }
    if cfg.height >= 2 {
        cfg.height &= !1;
    }
    cfg
}

/// `ExpectedFrameRate`: a hint, in frames per second.
pub(crate) fn expected_fps(cfg: &EncoderConfig) -> f64 {
    cfg.fps_num as f64 / cfg.fps_den.max(1) as f64
}

/// Bytes allowed per [`DATA_RATE_WINDOW_SECS`] at `bitrate_bps`. Never 0
/// (which the encoder could only meet by dropping everything).
pub(crate) fn data_rate_limit_bytes(bitrate_bps: u32) -> i64 {
    let bytes = bitrate_bps as u64 * PEAK_NUM / PEAK_DEN / 8;
    bytes.clamp(1, i64::MAX as u64) as i64
}

/// An H.264 Main level VideoToolbox has a constant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H264Level {
    L3_0,
    L3_1,
    L3_2,
    L4_0,
    L4_1,
    L4_2,
    L5_0,
    L5_1,
    L5_2,
    /// Beyond 5.2 (VideoToolbox has no Main 6.x constant): let the encoder pick.
    Auto,
}

/// One row of H.264 Table A-1: `(level, MaxMBPS, MaxFS, MaxBR kbit/s)`;
/// MaxBR is the VCL figure for Main (cpbBrVclFactor 1000). Levels below 3.0
/// are absent: VideoToolbox has no Main constant for them, and declaring 3.0
/// for a smaller stream is still conforming (every limit is looser).
const H264_LEVELS: &[(H264Level, u64, u64, u64)] = &[
    (H264Level::L3_0, 40_500, 1_620, 10_000),
    (H264Level::L3_1, 108_000, 3_600, 14_000),
    (H264Level::L3_2, 216_000, 5_120, 20_000),
    (H264Level::L4_0, 245_760, 8_192, 20_000),
    (H264Level::L4_1, 245_760, 8_192, 50_000),
    (H264Level::L4_2, 522_240, 8_704, 50_000),
    (H264Level::L5_0, 589_824, 22_080, 135_000),
    (H264Level::L5_1, 983_040, 36_864, 240_000),
    (H264Level::L5_2, 2_073_600, 36_864, 240_000),
];

/// The Main level for the classic (non-low-latency) session: the lowest one
/// whose frame size (MaxFS, and each dimension ≤ √(8·MaxFS) macroblocks),
/// macroblock rate and bitrate hold this stream at [`BITRATE_CEILING_BPS`].
pub(crate) fn h264_level(width: u32, height: u32, fps_num: u32, fps_den: u32) -> H264Level {
    h264_level_at(width, height, fps_num, fps_den, BITRATE_CEILING_BPS)
}

fn h264_level_at(
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    bitrate_bps: u64,
) -> H264Level {
    let (w_mbs, h_mbs) = (width.div_ceil(16) as u64, height.div_ceil(16) as u64);
    let fs = w_mbs * h_mbs;
    let mbps = (fs * fps_num as u64).div_ceil(fps_den.max(1) as u64);
    let longest = w_mbs.max(h_mbs);
    H264_LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs, max_br_kbps)| {
            fs <= max_fs
                && longest * longest <= 8 * max_fs
                && mbps <= max_mbps
                && bitrate_bps <= max_br_kbps * 1000
        })
        .map_or(H264Level::Auto, |&(level, ..)| level)
}

// ---- result codes ----------------------------------------------------------

/// The `VTErrors.h` name of a result code, for logs and bug reports.
pub(crate) fn status_name(status: i32) -> &'static str {
    match status {
        kVTPropertyNotSupportedErr => "kVTPropertyNotSupportedErr",
        kVTPropertyReadOnlyErr => "kVTPropertyReadOnlyErr",
        kVTParameterErr => "kVTParameterErr",
        kVTInvalidSessionErr => "kVTInvalidSessionErr",
        kVTAllocationFailedErr => "kVTAllocationFailedErr",
        kVTPixelTransferNotSupportedErr => "kVTPixelTransferNotSupportedErr",
        kVTPixelTransferNotPermittedErr => "kVTPixelTransferNotPermittedErr",
        kVTCouldNotCreateInstanceErr => "kVTCouldNotCreateInstanceErr",
        kVTCouldNotFindVideoEncoderErr => "kVTCouldNotFindVideoEncoderErr",
        kVTVideoEncoderMalfunctionErr => "kVTVideoEncoderMalfunctionErr",
        kVTVideoEncoderNotAvailableNowErr => "kVTVideoEncoderNotAvailableNowErr",
        kVTVideoEncoderAuthorizationErr => "kVTVideoEncoderAuthorizationErr",
        kVTSessionMalfunctionErr => "kVTSessionMalfunctionErr",
        kVTVideoEncoderNeedsRosettaErr => "kVTVideoEncoderNeedsRosettaErr",
        _ => "unknown VideoToolbox error",
    }
}

/// What the user can do about a result code.
fn status_advice(status: i32) -> &'static str {
    match status {
        kVTCouldNotFindVideoEncoderErr | kVTCouldNotCreateInstanceErr => {
            "this Mac has no hardware encoder for this codec at this size; the software \
             H.264 encoder is used instead when it is turned on (Settings → Advanced)"
        }
        kVTVideoEncoderNotAvailableNowErr => {
            "the hardware encoder is busy (another app may be recording or streaming the \
             screen) or the Mac is waking from sleep; it is retried automatically"
        }
        kVTInvalidSessionErr | kVTSessionMalfunctionErr | kVTVideoEncoderMalfunctionErr => {
            "macOS reset the hardware encoder (sleep, display or GPU change); it is \
             restarted automatically — if this keeps happening, restart the Mac"
        }
        kVTAllocationFailedErr => {
            "macOS ran out of memory for video buffers; close apps that record or stream \
             the screen, or lower the display resolution"
        }
        kVTParameterErr => {
            "VideoToolbox refused the encoder settings for this size or frame rate; try a \
             smaller session window or a lower display resolution"
        }
        kVTPixelTransferNotSupportedErr | kVTPixelTransferNotPermittedErr => {
            "the captured picture cannot be converted for the encoder; report this with \
             your macOS version and display setup"
        }
        kVTVideoEncoderAuthorizationErr => {
            "macOS refused this app access to the video encoder; reinstall the app from \
             the official download"
        }
        kVTVideoEncoderNeedsRosettaErr => {
            "the encoder needs Rosetta 2; install it (softwareupdate --install-rosetta) or \
             use the Apple-silicon build"
        }
        kVTPropertyNotSupportedErr | kVTPropertyReadOnlyErr => {
            "this macOS version does not support the setting"
        }
        _ => "unexpected VideoToolbox failure; reconnect, and report the code if it repeats",
    }
}

/// `"<what> failed: <name> (<code>) — <advice>"`.
pub(crate) fn describe(what: &str, status: i32) -> String {
    format!(
        "{what} failed: {} ({status}) — {}",
        status_name(status),
        status_advice(status)
    )
}

/// The session is gone (sleep, GPU change, encoder crash): drop it and build
/// a new one rather than failing every following frame.
pub(crate) fn session_lost(status: i32) -> bool {
    matches!(
        status,
        kVTInvalidSessionErr
            | kVTSessionMalfunctionErr
            | kVTVideoEncoderMalfunctionErr
            | kVTVideoEncoderNotAvailableNowErr
    )
}

/// The low-latency session was refused, so the classic one is worth trying:
/// `kVTParameterErr` is what Intel Macs return for low-latency HEVC (plans/01
/// §4); `kVTCouldNotFindVideoEncoderErr` means no encoder with low-latency
/// support, which does not rule out a plain hardware one.
pub(crate) fn low_latency_refused(status: i32) -> bool {
    matches!(status, kVTParameterErr | kVTCouldNotFindVideoEncoderErr)
}

/// No hardware encoder for this codec (as opposed to a failure of one that
/// exists): the factory reports [`CodecError::NoEncoder`] so the software
/// fallback takes over.
pub(crate) fn no_encoder(status: i32) -> bool {
    matches!(
        status,
        kVTCouldNotFindVideoEncoderErr | kVTCouldNotCreateInstanceErr
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::Codec;

    fn cfg(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            ..EncoderConfig::default()
        }
    }

    #[test]
    fn invalid_configs_are_refused_with_the_reason() {
        assert!(validate(&EncoderConfig::default()).is_ok());
        let msg = |c: EncoderConfig| validate(&c).err().map(|e| e.to_string());
        assert!(msg(cfg(0, 1080)).is_some_and(|m| m.contains("below 2")));
        assert!(msg(cfg(u32::MAX, 1080)).is_some_and(|m| m.contains("too large")));
        assert!(msg(EncoderConfig {
            fps_den: 0,
            ..EncoderConfig::default()
        })
        .is_some_and(|m| m.contains("frame rate")));
        assert!(msg(EncoderConfig {
            bitrate_bps: 0,
            ..EncoderConfig::default()
        })
        .is_some_and(|m| m.contains("zero bitrate")));
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let c = even_size(cfg(1921, 1081));
        assert_eq!((c.width, c.height), (1920, 1080));
        let c = even_size(cfg(1, 1));
        assert_eq!((c.width, c.height), (1, 1));
        assert_eq!(even_size(EncoderConfig::default()).codec, Codec::H264);
    }

    #[test]
    fn data_rate_limit_is_one_and_a_half_times_the_target_per_second() {
        assert_eq!(data_rate_limit_bytes(8_000_000), 1_500_000);
        assert_eq!(data_rate_limit_bytes(1), 1);
        assert_eq!(data_rate_limit_bytes(u32::MAX), 805_306_367);
        assert_eq!(DATA_RATE_WINDOW_SECS, 1.0);
    }

    #[test]
    fn expected_fps_is_the_declared_rate() {
        let c = EncoderConfig {
            fps_num: 60_000,
            fps_den: 1001,
            ..EncoderConfig::default()
        };
        assert!((expected_fps(&c) - 59.94).abs() < 0.01);
    }

    #[test]
    fn levels_follow_size_and_rate_at_the_ceiling() {
        // The 50 Mbit/s ceiling alone needs 4.1.
        assert_eq!(h264_level(640, 360, 30, 1), H264Level::L4_1);
        assert_eq!(h264_level(1920, 1080, 30, 1), H264Level::L4_1);
        assert_eq!(h264_level(1920, 1080, 60, 1), H264Level::L4_2);
        assert_eq!(h264_level(2560, 1440, 60, 1), H264Level::L5_1);
        assert_eq!(h264_level(3840, 2160, 30, 1), H264Level::L5_1);
        assert_eq!(h264_level(3840, 2160, 60, 1), H264Level::L5_2);
        // 5K at 60 is past 5.2: no Main constant, the encoder chooses.
        assert_eq!(h264_level(5120, 2880, 60, 1), H264Level::Auto);
        // A very long, thin picture: tiny MaxFS need, but 512 MBs wide needs
        // √(8·MaxFS) ≥ 512, i.e. MaxFS ≥ 32768 → 5.1.
        assert_eq!(h264_level(8192, 16, 30, 1), H264Level::L5_1);
    }

    #[test]
    fn levels_without_the_ceiling_use_the_whole_table() {
        assert_eq!(h264_level_at(640, 360, 30, 1, 2_000_000), H264Level::L3_0);
        assert_eq!(h264_level_at(1280, 720, 30, 1, 8_000_000), H264Level::L3_1);
        assert_eq!(h264_level_at(1280, 720, 60, 1, 8_000_000), H264Level::L3_2);
        // A zero denominator is treated as 1, not a division by zero.
        assert_eq!(h264_level_at(640, 360, 30, 0, 2_000_000), H264Level::L3_0);
    }

    #[test]
    fn every_status_has_a_name_and_actionable_advice() {
        let msg = describe("VTCompressionSessionCreate", kVTCouldNotFindVideoEncoderErr);
        assert!(msg.contains("kVTCouldNotFindVideoEncoderErr (-12908)"), "{msg}");
        assert!(msg.contains("Settings → Advanced"), "{msg}");
        let msg = describe("encode", -1);
        assert!(msg.contains("unknown VideoToolbox error (-1)"), "{msg}");
        assert!(msg.contains("report the code"), "{msg}");
        assert!(describe("x", kVTVideoEncoderNeedsRosettaErr).contains("Rosetta"));
    }

    #[test]
    fn status_classes() {
        assert!(session_lost(kVTInvalidSessionErr));
        assert!(session_lost(kVTVideoEncoderNotAvailableNowErr));
        assert!(!session_lost(kVTParameterErr));
        assert!(low_latency_refused(kVTParameterErr));
        assert!(low_latency_refused(kVTCouldNotFindVideoEncoderErr));
        assert!(!low_latency_refused(kVTAllocationFailedErr));
        assert!(no_encoder(kVTCouldNotFindVideoEncoderErr));
        assert!(!no_encoder(kVTParameterErr));
    }
}
