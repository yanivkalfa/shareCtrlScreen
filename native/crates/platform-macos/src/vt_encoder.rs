//! `VideoEncoder` on VideoToolbox (plan task M2): a hardware
//! `VTCompressionSession` per encoder, fed ScreenCaptureKit's IOSurface-backed
//! `CVPixelBuffer`s zero-copy, or CPU BGRA / NV12 frames copied into a
//! `CVPixelBufferPool` buffer.
//!
//! * **Session.** Hardware only (`RequireHardwareAcceleratedVideoEncoder`):
//!   without a hardware encoder the factory reports `NoEncoder` and the
//!   Cisco OpenH264 fallback takes over. Low-latency rate control first; an
//!   encoder that refuses it (`vt_params::low_latency_refused`) gets the
//!   classic session with the same recipe. See `vt_params` for the tuning.
//! * **Synchronous output.** Each `encode` submits one frame and then calls
//!   `VTCompressionSessionCompleteFrames` up to its timestamp, so the output
//!   callback has run before `encode` returns and the unit is handed back in
//!   the same call. No reordering means this adds no delay: the frame is
//!   finished either way before anything can be sent.
//! * **Timestamps** are real elapsed time. `DataRateLimits` and the rate
//!   controller work from them; a frame counter would tell the encoder a
//!   static screen (where the host skips frames) runs at full rate.
//! * **Keyframes** are requested with the `ForceKeyFrame` frame option and
//!   the request stays armed until a keyframe actually comes out: Apple
//!   notes the encoder may not honour every request, and a dropped frame
//!   carries no keyframe.
//! * **Size.** The session encodes at `EncoderConfig` size; a captured frame
//!   of another size (the host encodes at the viewer's window size) is
//!   scaled by the session's own pixel-transfer stage, configured for
//!   averaging (not decimating) downscales so text stays legible.
//! * **Recovery.** A session macOS tears down (sleep, GPU change, encoder
//!   fault) is dropped and rebuilt on a later frame with backoff, and the
//!   first frame of the new session is a keyframe; the host keeps calling
//!   `encode` and never sees a dead encoder.
//!
//! ## References
//! * `VTCompressionSessionCreate`, `VTCompressionSessionEncodeFrame` ("Each
//!   presentation timestamp passed to a session must be greater than the
//!   previous one… The session and/or encoder retains the image buffer as
//!   long as necessary"), `VTCompressionSessionCompleteFrames` ("frames with
//!   presentation timestamps up to and including this timestamp are emitted
//!   before the function returns"), `VTCompressionSessionInvalidate`
//!   ("ensures a deterministic, orderly teardown"):
//!   <https://developer.apple.com/documentation/videotoolbox/vtcompressionsession>
//! * `VTCompressionOutputCallback`, `VTEncodeInfoFlags.frameDropped`:
//!   <https://developer.apple.com/documentation/videotoolbox/vtcompressionoutputcallback>
//! * Compression properties — `RealTime`, `AllowFrameReordering`,
//!   `ProfileLevel`, `H264EntropyMode`, `MaxKeyFrameInterval(Duration)`,
//!   `AverageBitRate`, `DataRateLimits`, `ExpectedFrameRate`,
//!   `MaxAllowedFrameQP` (macOS 12+), `PrioritizeEncodingSpeedOverQuality`
//!   (macOS 11+), `ColorPrimaries` / `TransferFunction` / `YCbCrMatrix`,
//!   `PixelTransferProperties` ("transfer source frames from the client's
//!   image buffers to the video encoder's image buffers, if necessary"):
//!   <https://developer.apple.com/documentation/videotoolbox/compression-properties>
//! * `kVTEncodeFrameOptionKey_ForceKeyFrame` ("it may not be possible for the
//!   encoder to accommodate all requests"):
//!   <https://developer.apple.com/documentation/videotoolbox/kvtencodeframeoptionkey_forcekeyframe>
//! * Encoder specification — `EnableLowLatencyRateControl` (macOS 11.3+),
//!   `RequireHardwareAcceleratedVideoEncoder`:
//!   <https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_requirehardwareacceleratedvideoencoder>,
//!   <https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol>
//! * Pixel transfer — `ScalingMode`, `DownsamplingMode_Average`, `RealTime`:
//!   <https://developer.apple.com/documentation/videotoolbox/vtpixeltransfersession>
//! * `CMSampleBufferGetDataBuffer`, `CMBlockBufferCopyDataBytes`,
//!   `CMVideoFormatDescriptionGet{H264,HEVC}ParameterSetAtIndex` ("may only be
//!   accessed as long as a retain on videoDesc is held"):
//!   <https://developer.apple.com/documentation/coremedia/cmsamplebuffer-u71>
//! * `CVPixelBufferPoolCreate`, `CVPixelBufferLockBaseAddress`:
//!   <https://developer.apple.com/documentation/corevideo/cvpixelbufferpool-58l>
//! * WWDC21 10158 "Explore low-latency video encoding with VideoToolbox".
//! * plans/01-MACOS-FINDINGS.md §4 (recipe, `-12902` fallback).

use std::ffi::{c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};
use objc2_core_media::{
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription,
    CMSampleBuffer, CMTime, CMVideoCodecType, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPlaneCount, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_H264EntropyMode,
    kVTCompressionPropertyKey_MaxAllowedFrameQP, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PixelTransferProperties,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTCouldNotCreateInstanceErr, kVTCouldNotFindVideoEncoderErr, kVTDownsamplingMode_Average,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTH264EntropyMode_CABAC, kVTParameterErr,
    kVTPixelTransferPropertyKey_DownsamplingMode, kVTPixelTransferPropertyKey_RealTime,
    kVTPixelTransferPropertyKey_ScalingMode, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    kVTProfileLevel_H264_Main_3_0, kVTProfileLevel_H264_Main_3_1, kVTProfileLevel_H264_Main_3_2,
    kVTProfileLevel_H264_Main_4_0, kVTProfileLevel_H264_Main_4_1, kVTProfileLevel_H264_Main_4_2,
    kVTProfileLevel_H264_Main_5_0, kVTProfileLevel_H264_Main_5_1, kVTProfileLevel_H264_Main_5_2,
    kVTProfileLevel_H264_Main_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTScalingMode_Normal, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};
use platform_api::{
    Codec, CodecError, CpuPlane, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat,
    VideoEncoder,
};

use crate::avcc::Nals;
use crate::convert::{copy_plane, retry_delay};
use crate::gpu::CvPixelBufferFrame;
use crate::vt_params::{
    data_rate_limit_bytes, describe, expected_fps, h264_level, low_latency_refused, no_encoder,
    session_lost, status_name, H264Level, DATA_RATE_WINDOW_SECS, KEYFRAME_INTERVAL_FALLBACK,
    KEYFRAME_INTERVAL_FRAMES, MAX_QP,
};

/// Timestamps are in microseconds.
const TIMESCALE: i32 = 1_000_000;

/// `noErr`.
const NO_ERR: i32 = 0;

/// A failed VideoToolbox / CoreMedia call: what was being done, and its
/// result code.
#[derive(Debug, Clone, Copy)]
struct Failure {
    what: &'static str,
    status: i32,
}

impl Failure {
    /// The trait error. A missing encoder is [`CodecError::NoEncoder`] so the
    /// factory's software fallback takes over; the full wording is logged
    /// first because `NoEncoder` carries only the codec.
    fn into_error(self, codec: Codec) -> CodecError {
        let message = describe(self.what, self.status);
        if no_encoder(self.status) {
            tracing::warn!("VideoToolbox {}: {message}", codec.as_caps_str());
            CodecError::NoEncoder(codec)
        } else {
            CodecError::Backend(format!("VideoToolbox: {message}"))
        }
    }
}

fn check(what: &'static str, status: i32) -> Result<(), Failure> {
    if status == NO_ERR {
        Ok(())
    } else {
        Err(Failure { what, status })
    }
}

// ---- output callback -------------------------------------------------------

/// What the output callback learned about one submitted frame.
enum Outcome {
    Unit(EncodedUnit),
    /// The rate controller dropped the frame: nothing was encoded, so no
    /// reference picture is missing on the viewer.
    Dropped,
    /// The encoder reported an error for this frame.
    Failed(i32),
    /// The encoder produced a picture this code could not turn into an
    /// access unit — the encoder now references a picture the viewer will
    /// never have.
    Unreadable(String),
}

/// State the output callback writes and `encode` reads. The session holds a
/// raw pointer to it (the callback's refcon), so it is kept alive by
/// [`Session`] until after the session is invalidated.
struct Shared {
    codec: Codec,
    outcomes: Mutex<Vec<Outcome>>,
}

impl Shared {
    fn take(&self) -> Vec<Outcome> {
        // A poisoned lock only means a panic elsewhere while holding it; the
        // vector itself is always in a valid state.
        std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `VTCompressionOutputCallback`. Runs on a VideoToolbox thread, or inside
/// `VTCompressionSessionCompleteFrames` on the caller's.
unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is `Arc::as_ptr` of the `Shared` that the owning
    // `Session` keeps alive until after `VTCompressionSessionInvalidate`,
    // after which VideoToolbox makes no further callbacks.
    let Some(shared) = (unsafe { refcon.cast::<Shared>().as_ref() }) else {
        return;
    };
    // Nothing may unwind into VideoToolbox's frames; a panic here (only
    // possible on allocation failure) becomes an unreadable-frame outcome.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if status != NO_ERR {
            return Outcome::Failed(status);
        }
        // SAFETY: when non-null, the sample buffer is valid for the duration
        // of the callback.
        match unsafe { sample.as_ref() } {
            None => Outcome::Dropped,
            Some(_) if flags.contains(VTEncodeInfoFlags::FrameDropped) => Outcome::Dropped,
            Some(sample) => match unit_from_sample(shared.codec, sample) {
                Ok(unit) => Outcome::Unit(unit),
                Err(e) => Outcome::Unreadable(e),
            },
        }
    }))
    .unwrap_or_else(|_| Outcome::Unreadable("panic while reading the encoded sample".into()));
    shared
        .outcomes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(outcome);
}

/// One encoded sample → an Annex-B [`EncodedUnit`], with the parameter sets
/// from the format description in front of every keyframe that lacks them.
fn unit_from_sample(codec: Codec, sample: &CMSampleBuffer) -> Result<EncodedUnit, String> {
    // SAFETY: `sample` is a valid sample buffer (callback contract).
    let block =
        unsafe { sample.data_buffer() }.ok_or("the encoded sample carries no data buffer")?;
    // SAFETY: valid block buffer.
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if let Some(dst) = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()).filter(|_| len > 0) {
        // SAFETY: `dst` points at `len` writable bytes.
        let rc = unsafe { block.copy_data_bytes(0, len, dst) };
        if rc != NO_ERR {
            return Err(format!("CMBlockBufferCopyDataBytes failed ({rc})"));
        }
    }
    // SAFETY: valid sample buffer; the returned description is retained.
    let desc = unsafe { sample.format_description() }
        .ok_or("the encoded sample carries no format description")?;
    let first = parameter_set(codec, &desc, 0)
        .map_err(|rc| format!("reading the NAL length size failed ({rc})"))?;
    let length_size = usize::try_from(first.header_length)
        .map_err(|_| format!("invalid NAL length size {}", first.header_length))?;
    let nals = Nals::parse(&avcc, length_size).map_err(|e| e.to_string())?;
    let keyframe = nals.is_keyframe(codec);
    let data = if keyframe && !nals.has_parameter_sets(codec) {
        let mut sets = Vec::with_capacity(first.count);
        sets.push(first.nal);
        for index in 1..first.count {
            let set = parameter_set(codec, &desc, index)
                .map_err(|rc| format!("reading parameter set {index} failed ({rc})"))?;
            sets.push(set.nal);
        }
        nals.to_annexb(&sets)
    } else {
        nals.to_annexb(&[])
    };
    Ok(EncodedUnit { data, keyframe })
}

/// One NAL unit of the format description's decoder configuration record
/// (SPS/PPS, or VPS/SPS/PPS), plus the record's totals.
struct ParameterSet<'a> {
    nal: &'a [u8],
    count: usize,
    header_length: c_int,
}

type GetParameterSet = unsafe extern "C-unwind" fn(
    &CMFormatDescription,
    usize,
    *mut *const u8,
    *mut usize,
    *mut usize,
    *mut c_int,
) -> i32;

fn parameter_set(
    codec: Codec,
    desc: &CMFormatDescription,
    index: usize,
) -> Result<ParameterSet<'_>, i32> {
    let get: GetParameterSet = match codec {
        Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
        // Never constructed for AV1 (no Mac encodes it).
        Codec::Av1 => return Err(kVTParameterErr),
    };
    let (mut nal, mut size, mut count, mut header_length) = (ptr::null(), 0usize, 0usize, 0);
    // SAFETY: valid format description; every out-pointer is a valid local.
    let rc = unsafe {
        get(
            desc,
            index,
            &mut nal,
            &mut size,
            &mut count,
            &mut header_length,
        )
    };
    if rc != NO_ERR {
        return Err(rc);
    }
    if nal.is_null() {
        return Err(kVTParameterErr);
    }
    // SAFETY: CoreMedia returns `size` bytes of the description's own
    // storage, valid while the description is retained — the returned
    // slice borrows `desc`, so it cannot outlive that retain.
    let nal = unsafe { std::slice::from_raw_parts(nal, size) };
    Ok(ParameterSet {
        nal,
        count,
        header_length,
    })
}

// ---- the session -----------------------------------------------------------

/// A live `VTCompressionSession` and the callback state it points at.
struct Session {
    raw: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    low_latency: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: valid session. Completing first delivers any pending
        // callback while `shared` is still alive; invalidating then ends all
        // callbacks, so `shared` may be freed right after (field drop).
        // The completion status is irrelevant: a broken session has no
        // frames to deliver.
        unsafe {
            let _ = self.raw.complete_frames(kCMTimeInvalid);
            self.raw.invalidate();
        }
    }
}

fn codec_type(codec: Codec) -> Option<CMVideoCodecType> {
    match codec {
        Codec::H264 => Some(kCMVideoCodecType_H264),
        Codec::Hevc => Some(kCMVideoCodecType_HEVC),
        Codec::Av1 => None,
    }
}

/// A CFDictionary from CFString keys to values of any CF type.
fn dictionary(
    keys: &[&CFString],
    values: &[&CFType],
) -> CFRetained<CFDictionary<CFString, CFType>> {
    CFDictionary::from_slices(keys, values)
}

fn set_property(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    // SAFETY: `key` is a compression-session property key and `value` is of
    // the type its documentation names (callers pair them).
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

/// A property the recipe cannot do without: failure fails the session.
fn require(
    session: &VTCompressionSession,
    what: &'static str,
    key: &CFString,
    value: &CFType,
) -> Result<(), Failure> {
    check(what, set_property(session, key, value))
}

/// A property that improves the stream but whose absence (older macOS, an
/// encoder that does not support it) leaves a working one: logged, not
/// fatal. Returns whether it took.
fn prefer(session: &VTCompressionSession, key: &CFString, value: &CFType) -> bool {
    let rc = set_property(session, key, value);
    if rc != NO_ERR {
        tracing::warn!(
            "VideoToolbox: {key} not applied: {} ({rc})",
            status_name(rc)
        );
    }
    rc == NO_ERR
}

impl Session {
    /// Low-latency session first, the classic one if that is refused.
    fn build(cfg: &EncoderConfig) -> Result<Self, Failure> {
        match Self::create(cfg, true) {
            Err(f) if low_latency_refused(f.status) => {
                tracing::info!(
                    "VideoToolbox {}: low-latency rate control refused ({}); using the \
                     standard session",
                    cfg.codec.as_caps_str(),
                    describe(f.what, f.status)
                );
                Self::create(cfg, false)
            }
            other => other,
        }
    }

    fn create(cfg: &EncoderConfig, low_latency: bool) -> Result<Self, Failure> {
        let Some(codec_type) = codec_type(cfg.codec) else {
            return Err(Failure {
                what: "choosing the codec",
                status: kVTCouldNotFindVideoEncoderErr,
            });
        };
        // SAFETY: framework constants, valid for the process lifetime.
        let (hw_key, ll_key) = unsafe {
            (
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
            )
        };
        let yes: &CFType = CFBoolean::new(true);
        let spec = if low_latency {
            dictionary(&[hw_key, ll_key], &[yes, yes])
        } else {
            dictionary(&[hw_key], &[yes])
        };
        let shared = Arc::new(Shared {
            codec: cfg.codec,
            outcomes: Mutex::new(Vec::new()),
        });
        let mut out: *mut VTCompressionSession = ptr::null_mut();
        // `validate` refused sizes above i32::MAX.
        let (w, h) = (cfg.width as i32, cfg.height as i32);
        // SAFETY: valid dictionaries; the callback matches
        // `VTCompressionOutputCallback`; the refcon outlives the session (see
        // `on_encoded`); `out` is a valid out-pointer.
        let rc = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                codec_type,
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_encoded),
                Arc::as_ptr(&shared).cast_mut().cast::<c_void>(),
                NonNull::from(&mut out),
            )
        };
        check("VTCompressionSessionCreate", rc)?;
        let raw = NonNull::new(out).ok_or(Failure {
            what: "VTCompressionSessionCreate",
            status: kVTCouldNotCreateInstanceErr,
        })?;
        // From here on `Session`'s Drop invalidates the session on any error.
        let session = Self {
            // SAFETY: a "Create" function returns a +1 reference, now ours.
            raw: unsafe { CFRetained::from_raw(raw) },
            shared,
            low_latency,
        };
        session.configure(cfg)?;
        // SAFETY: valid session.
        check("VTCompressionSessionPrepareToEncodeFrames", unsafe {
            session.raw.prepare_to_encode_frames()
        })?;
        tracing::info!(
            "VideoToolbox {} encoder ready: {}x{} @{:.2} fps, {} kbps, {}",
            cfg.codec.as_caps_str(),
            cfg.width,
            cfg.height,
            expected_fps(cfg),
            cfg.bitrate_bps / 1000,
            if low_latency {
                "low-latency rate control"
            } else {
                "standard rate control"
            }
        );
        Ok(session)
    }

    /// The recipe (`vt_params` docs). Only what the stream cannot be correct
    /// without is required; the rest is applied where the encoder allows.
    fn configure(&self, cfg: &EncoderConfig) -> Result<(), Failure> {
        let s = &*self.raw;
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        // SAFETY (whole block): framework constants, valid for the process
        // lifetime; each is paired with the value type its documentation names.
        unsafe {
            // Required: the viewer's decoders have no reorder buffer (the
            // `VideoDecoder` contract). Low-latency sessions never reorder
            // (Apple), so there it is a formality an encoder may refuse.
            let rc = set_property(s, kVTCompressionPropertyKey_AllowFrameReordering, no);
            if rc != NO_ERR && !self.low_latency {
                return Err(Failure {
                    what: "turning off frame reordering",
                    status: rc,
                });
            }
            prefer(s, kVTCompressionPropertyKey_RealTime, yes);
            prefer(
                s,
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                yes,
            );
            let fps = CFNumber::new_f64(expected_fps(cfg));
            prefer(s, kVTCompressionPropertyKey_ExpectedFrameRate, &fps);
            self.configure_profile(cfg);

            // Keyframes only on request.
            let interval_set = [KEYFRAME_INTERVAL_FRAMES, KEYFRAME_INTERVAL_FALLBACK]
                .into_iter()
                .any(|frames| {
                    set_property(
                        s,
                        kVTCompressionPropertyKey_MaxKeyFrameInterval,
                        &CFNumber::new_i32(frames),
                    ) == NO_ERR
                });
            if !interval_set {
                tracing::warn!("VideoToolbox: the encoder chooses its own keyframe interval");
            }
            // 0 = no time-based limit.
            prefer(
                s,
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &CFNumber::new_f64(0.0),
            );

            // Quality floor.
            prefer(
                s,
                kVTCompressionPropertyKey_MaxAllowedFrameQP,
                &CFNumber::new_i32(MAX_QP),
            );

            // BT.709 limited range, as the capture configures it and every
            // presenter assumes.
            prefer(
                s,
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            );
            prefer(
                s,
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            );
            prefer(
                s,
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            );

            // Scaling a captured frame to the encode size: aspect is already
            // preserved by the host, so plain scaling; averaging keeps thin
            // text strokes that decimation would drop.
            let transfer = dictionary(
                &[
                    kVTPixelTransferPropertyKey_ScalingMode,
                    kVTPixelTransferPropertyKey_DownsamplingMode,
                    kVTPixelTransferPropertyKey_RealTime,
                ],
                &[kVTScalingMode_Normal, kVTDownsamplingMode_Average, yes],
            );
            prefer(
                s,
                kVTCompressionPropertyKey_PixelTransferProperties,
                &transfer,
            );
        }
        apply_bitrate(s, cfg.bitrate_bps)
    }

    fn configure_profile(&self, cfg: &EncoderConfig) {
        let s = &*self.raw;
        // SAFETY: framework constants, valid for the process lifetime.
        unsafe {
            match cfg.codec {
                Codec::H264 => {
                    let profile = if self.low_latency {
                        // Low latency allows only High profiles and picks the
                        // level itself; Constrained High = High without B-frames.
                        kVTProfileLevel_H264_ConstrainedHigh_AutoLevel
                    } else {
                        main_level(h264_level(cfg.width, cfg.height, cfg.fps_num, cfg.fps_den))
                    };
                    if !prefer(s, kVTCompressionPropertyKey_ProfileLevel, profile)
                        && !self.low_latency
                    {
                        prefer(
                            s,
                            kVTCompressionPropertyKey_ProfileLevel,
                            kVTProfileLevel_H264_Main_AutoLevel,
                        );
                    }
                    prefer(
                        s,
                        kVTCompressionPropertyKey_H264EntropyMode,
                        kVTH264EntropyMode_CABAC,
                    );
                }
                Codec::Hevc => {
                    prefer(
                        s,
                        kVTCompressionPropertyKey_ProfileLevel,
                        kVTProfileLevel_HEVC_Main_AutoLevel,
                    );
                }
                Codec::Av1 => {}
            }
        }
    }

    /// Encode one buffer and wait for its output.
    fn encode(
        &self,
        buffer: &CVPixelBuffer,
        pts_us: i64,
        force_keyframe: bool,
    ) -> Result<Vec<Outcome>, Failure> {
        // SAFETY: framework constants.
        let options = force_keyframe.then(|| unsafe {
            dictionary(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true)],
            )
        });
        // SAFETY: plain value constructor.
        let pts = unsafe { CMTime::new(pts_us, TIMESCALE) };
        let mut flags = VTEncodeInfoFlags(0);
        // SAFETY: valid session and buffer (VideoToolbox retains the buffer
        // for as long as it needs it); valid options dictionary or none;
        // `flags` is a valid out-pointer.
        let rc = unsafe {
            self.raw.encode_frame(
                buffer,
                pts,
                kCMTimeInvalid,
                options.as_deref().map(|o| o.as_opaque()),
                ptr::null_mut(),
                &mut flags,
            )
        };
        check("VTCompressionSessionEncodeFrame", rc)?;
        // SAFETY: valid session. Emits this frame (and anything before it)
        // through the callback before returning.
        let rc = unsafe { self.raw.complete_frames(pts) };
        let outcomes = self.shared.take();
        check("VTCompressionSessionCompleteFrames", rc)?;
        if outcomes.is_empty() && flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Ok(vec![Outcome::Dropped]);
        }
        Ok(outcomes)
    }
}

/// `AverageBitRate` (required) and the `DataRateLimits` burst cap, both
/// settable on a running session.
fn apply_bitrate(s: &VTCompressionSession, bps: u32) -> Result<(), Failure> {
    let average = CFNumber::new_i64(i64::from(bps));
    // SAFETY: framework constants with their documented value types.
    unsafe {
        require(
            s,
            "setting the bitrate",
            kVTCompressionPropertyKey_AverageBitRate,
            &average,
        )?;
        let bytes = CFNumber::new_i64(data_rate_limit_bytes(bps));
        let seconds = CFNumber::new_f64(DATA_RATE_WINDOW_SECS);
        let limits = CFArray::<CFNumber>::from_objects(&[&*bytes, &*seconds]);
        prefer(s, kVTCompressionPropertyKey_DataRateLimits, &limits);
    }
    Ok(())
}

/// The `kVTProfileLevel_H264_Main_*` constant for a level.
fn main_level(level: H264Level) -> &'static CFString {
    // SAFETY: framework constants, valid for the process lifetime.
    unsafe {
        match level {
            H264Level::L3_0 => kVTProfileLevel_H264_Main_3_0,
            H264Level::L3_1 => kVTProfileLevel_H264_Main_3_1,
            H264Level::L3_2 => kVTProfileLevel_H264_Main_3_2,
            H264Level::L4_0 => kVTProfileLevel_H264_Main_4_0,
            H264Level::L4_1 => kVTProfileLevel_H264_Main_4_1,
            H264Level::L4_2 => kVTProfileLevel_H264_Main_4_2,
            H264Level::L5_0 => kVTProfileLevel_H264_Main_5_0,
            H264Level::L5_1 => kVTProfileLevel_H264_Main_5_1,
            H264Level::L5_2 => kVTProfileLevel_H264_Main_5_2,
            H264Level::Auto => kVTProfileLevel_H264_Main_AutoLevel,
        }
    }
}

// ---- CPU frames ------------------------------------------------------------

/// IOSurface-backed buffers for CPU frames of one format and size, so a CPU
/// frame reaches the hardware the same way a captured one does.
struct UploadPool {
    pool: CFRetained<CVPixelBufferPool>,
    key: (PixelFormat, u32, u32),
}

impl UploadPool {
    fn new(format: PixelFormat, width: u32, height: u32) -> Result<Self, CodecError> {
        let os_type = match format {
            PixelFormat::Bgra8 => kCVPixelFormatType_32BGRA,
            // `PixelFormat::Nv12` is BT.709 limited range = '420v'.
            PixelFormat::Nv12 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        };
        let empty = dictionary(&[], &[]);
        // CoreVideo stores OSTypes in signed 32-bit CFNumbers.
        let fourcc = CFNumber::new_i32(os_type as i32);
        let w = CFNumber::new_i64(i64::from(width));
        let h = CFNumber::new_i64(i64::from(height));
        // SAFETY: framework constants, valid for the process lifetime.
        let attrs = unsafe {
            dictionary(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&fourcc, &w, &h, &empty],
            )
        };
        let mut out: *mut CVPixelBufferPool = ptr::null_mut();
        // SAFETY: valid attribute dictionary; `out` is a valid out-pointer.
        let rc = unsafe {
            CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut out))
        };
        let raw = NonNull::new(out)
            .filter(|_| rc == kCVReturnSuccess)
            .ok_or_else(|| cv_error("creating a pixel buffer pool for CPU frames", rc))?;
        Ok(Self {
            // SAFETY: a "Create" function returns a +1 reference, now ours.
            pool: unsafe { CFRetained::from_raw(raw) },
            key: (format, width, height),
        })
    }

    /// A pool buffer holding a copy of the CPU planes.
    fn fill(
        &self,
        plane0: CpuPlane<'_>,
        plane1: Option<CpuPlane<'_>>,
    ) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
        let (format, width, height) = self.key;
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: valid pool; `out` is a valid out-pointer.
        let rc = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out))
        };
        let raw = NonNull::new(out)
            .filter(|_| rc == kCVReturnSuccess)
            .ok_or_else(|| cv_error("taking a pixel buffer from the pool", rc))?;
        // SAFETY: a "Create" function returns a +1 reference, now ours.
        let buffer: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(raw) };
        let (w, h) = (width as usize, height as usize);
        let planes: Vec<(CpuPlane<'_>, usize, usize)> = match format {
            PixelFormat::Bgra8 => vec![(plane0, w * 4, h)],
            PixelFormat::Nv12 => {
                let uv = plane1.ok_or_else(|| {
                    CodecError::Unsupported("NV12 frame without its UV plane".into())
                })?;
                vec![(plane0, w, h), (uv, w.div_ceil(2) * 2, h.div_ceil(2))]
            }
        };
        // SAFETY: valid buffer; balanced by the unlock below.
        let rc = unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        if rc != kCVReturnSuccess {
            return Err(cv_error("locking a pixel buffer for writing", rc));
        }
        let copied = copy_planes(&buffer, &planes);
        // SAFETY: locked above with the same flags.
        let rc = unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        copied?;
        if rc != kCVReturnSuccess {
            return Err(cv_error("unlocking a pixel buffer", rc));
        }
        Ok(buffer)
    }
}

/// Copy CPU planes into a locked buffer's planes, in order.
fn copy_planes(
    buffer: &CVPixelBuffer,
    planes: &[(CpuPlane<'_>, usize, usize)],
) -> Result<(), CodecError> {
    // A non-planar buffer (BGRA) reports 0 planes and is addressed through
    // the whole-buffer accessors; a planar one ('420v') through the per-plane
    // ones.
    let planar = CVPixelBufferGetPlaneCount(buffer);
    if planar.max(1) != planes.len() {
        return Err(CodecError::Backend(format!(
            "pixel buffer has {} planes, the CPU frame {}",
            planar.max(1),
            planes.len()
        )));
    }
    for (index, (src, row_bytes, rows)) in planes.iter().enumerate() {
        let (base, stride, height) = if planar == 0 {
            (
                CVPixelBufferGetBaseAddress(buffer),
                CVPixelBufferGetBytesPerRow(buffer),
                CVPixelBufferGetHeight(buffer),
            )
        } else {
            (
                CVPixelBufferGetBaseAddressOfPlane(buffer, index),
                CVPixelBufferGetBytesPerRowOfPlane(buffer, index),
                CVPixelBufferGetHeightOfPlane(buffer, index),
            )
        };
        if base.is_null() {
            return Err(CodecError::Backend(format!(
                "pixel buffer plane {index} has no CPU address"
            )));
        }
        // SAFETY: the buffer is locked; CoreVideo guarantees `stride ×
        // height` addressable bytes for the plane.
        let dst = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), stride * height) };
        copy_plane(dst, stride, src.data, src.stride, *row_bytes, *rows)
            .map_err(CodecError::Unsupported)?;
    }
    Ok(())
}

fn cv_error(what: &str, rc: i32) -> CodecError {
    CodecError::Backend(format!(
        "CoreVideo: {what} failed ({rc}) — macOS may be out of video memory; close apps \
         that record or stream the screen and reconnect"
    ))
}

// ---- the encoder -----------------------------------------------------------

/// The VideoToolbox [`VideoEncoder`].
pub(crate) struct VtEncoder {
    /// What the next session is built with; the bitrate follows `set_bitrate`.
    cfg: EncoderConfig,
    session: Option<Session>,
    /// Consecutive failed rebuilds, for the backoff.
    failures: u32,
    retry_at: Instant,
    /// Armed by `force_keyframe` and by anything that leaves the viewer
    /// without a reference picture; cleared only by an emitted keyframe.
    force_key: bool,
    epoch: Instant,
    last_pts_us: i64,
    upload: Option<UploadPool>,
}

impl VtEncoder {
    /// Build the session now, so a Mac without a usable hardware encoder is
    /// known at construction and the factory can fall back to software.
    pub(crate) fn new(cfg: EncoderConfig) -> Result<Self, CodecError> {
        let session = Session::build(&cfg).map_err(|f| f.into_error(cfg.codec))?;
        let now = Instant::now();
        Ok(Self {
            cfg,
            session: Some(session),
            failures: 0,
            retry_at: now,
            // The contract: the first unit is an IDR. A new session starts
            // with one anyway; asking costs nothing.
            force_key: true,
            epoch: now,
            last_pts_us: -1,
            upload: None,
        })
    }

    /// Strictly increasing microseconds since the encoder was made.
    fn next_pts(&mut self) -> i64 {
        let now = i64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(i64::MAX);
        self.last_pts_us = now.max(self.last_pts_us.saturating_add(1));
        self.last_pts_us
    }

    /// The buffer to encode: the capture's own (zero-copy) or a pool copy of
    /// a CPU frame.
    fn source(&mut self, frame: &Frame<'_>) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
        match frame.data {
            FrameData::Native(any) => {
                let native = any.downcast_ref::<CvPixelBufferFrame>().ok_or_else(|| {
                    CodecError::Unsupported(
                        "the VideoToolbox encoder takes macOS capture frames \
                         (CvPixelBufferFrame) or CPU BGRA/NV12 frames"
                            .into(),
                    )
                })?;
                // A reference for the duration of this call only (dropped
                // when `encode` returns): VideoToolbox takes its own, and
                // ScreenCaptureKit needs the surface back promptly.
                Ok(native.pixel_buffer().retain())
            }
            FrameData::Cpu {
                format,
                plane0,
                plane1,
            } => {
                let key = (format, frame.width, frame.height);
                if frame.width == 0 || frame.height == 0 {
                    return Err(CodecError::Unsupported(format!(
                        "empty CPU frame ({}x{})",
                        frame.width, frame.height
                    )));
                }
                // One pool per format and size; a change replaces it.
                let pool = match self.upload.take() {
                    Some(pool) if pool.key == key => pool,
                    _ => UploadPool::new(format, frame.width, frame.height)?,
                };
                let filled = pool.fill(plane0, plane1);
                self.upload = Some(pool);
                filled
            }
        }
    }

    /// The running session, rebuilding a lost one (the caller has checked
    /// the backoff).
    fn session(&mut self) -> Result<&Session, CodecError> {
        if self.session.is_none() {
            let now = Instant::now();
            match Session::build(&self.cfg) {
                Ok(s) => {
                    tracing::info!("VideoToolbox: encoder session rebuilt");
                    self.session = Some(s);
                    self.failures = 0;
                    // New session, new parameter sets: the viewer restarts here.
                    self.force_key = true;
                }
                Err(f) => {
                    self.failures = self.failures.saturating_add(1);
                    self.retry_at = now + retry_delay(self.failures);
                    return Err(f.into_error(self.cfg.codec));
                }
            }
        }
        self.session
            .as_ref()
            .ok_or_else(|| CodecError::Backend("VideoToolbox: no encoder session".into()))
    }

    /// A failure that may have cost the viewer a reference picture.
    fn failed(&mut self, f: Failure) -> CodecError {
        self.force_key = true;
        if session_lost(f.status) {
            self.session = None;
            self.retry_at = Instant::now();
        }
        f.into_error(self.cfg.codec)
    }
}

impl VideoEncoder for VtEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        if self.session.is_none() && Instant::now() < self.retry_at {
            // Backing off after a failed rebuild: skip the frame (and its
            // upload) quietly; the failure was reported when it happened.
            return Ok(Vec::new());
        }
        let buffer = self.source(frame)?;
        let pts_us = self.next_pts();
        let force_key = self.force_key;
        let result = self.session()?.encode(&buffer, pts_us, force_key);
        let outcomes = result.map_err(|f| self.failed(f))?;
        let mut units = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            match outcome {
                Outcome::Unit(unit) => {
                    if unit.keyframe {
                        self.force_key = false;
                    }
                    units.push(unit);
                }
                Outcome::Dropped => {}
                Outcome::Failed(status) => {
                    return Err(self.failed(Failure {
                        what: "encoding a frame",
                        status,
                    }))
                }
                Outcome::Unreadable(why) => {
                    self.force_key = true;
                    return Err(CodecError::Backend(format!(
                        "VideoToolbox produced a frame that could not be read ({why}); the \
                         next frame is a keyframe"
                    )));
                }
            }
        }
        Ok(units)
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 {
            return Err(CodecError::Unsupported("VideoToolbox: bitrate 0".into()));
        }
        if bps == self.cfg.bitrate_bps {
            return Ok(());
        }
        // Stored first: a session that is (or turns out to be) gone is
        // rebuilt from `cfg`, and must come back at the new rate.
        let previous = self.cfg.bitrate_bps;
        self.cfg.bitrate_bps = bps;
        let Some(session) = &self.session else {
            return Ok(());
        };
        // Live: VideoToolbox applies it from the next frame, no keyframe.
        match apply_bitrate(&session.raw, bps) {
            Ok(()) => Ok(()),
            Err(f) if session_lost(f.status) => Err(self.failed(f)),
            Err(f) => {
                // The session still runs at the old rate; keep `cfg` saying so,
                // so the next change (or a retry of this one) is applied.
                self.cfg.bitrate_bps = previous;
                Err(f.into_error(self.cfg.codec))
            }
        }
    }

    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
}

/// Whether this Mac has a hardware encoder for `codec`: a real session at a
/// common size, built and torn down once.
pub(crate) fn probe(codec: Codec) -> bool {
    let cfg = EncoderConfig {
        codec,
        ..EncoderConfig::default()
    };
    match VtEncoder::new(cfg) {
        Ok(_) => true,
        Err(e) => {
            tracing::info!(
                "VideoToolbox: no hardware {} encoder ({e})",
                codec.as_caps_str()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    //! Need a Mac with a hardware video encoder (any Apple-silicon Mac; most
    //! Intel Macs with a T2 chip or Quick Sync). Each probes first and skips
    //! with a printed line without one (`tests/support/hardware.rs`):
    //!
    //! ```text
    //! cargo test -p platform-macos --lib vt_encoder::tests -- --ignored --nocapture
    //! ```
    //!
    //! The macOS CI step runs the same command with
    //! `SCT_MAC_HW_TESTS_REQUIRED=1`, so a skip there fails.

    use objc2_core_video::CVPixelBufferGetWidth;

    use super::*;

    /// False, after a printed skip, when this Mac has no hardware H.264
    /// encoder: the probe every hardware test in this module runs first.
    fn hardware_present() -> bool {
        crate::hardware::encoder_gate(crate::h264_hardware_encoder())
    }

    /// A `420v` IOSurface buffer with a horizontal luma ramp, like a
    /// ScreenCaptureKit frame.
    fn capture_like(w: u32, h: u32) -> CvPixelBufferFrame {
        let pool = UploadPool::new(PixelFormat::Nv12, w, h).expect("pool");
        let (wu, hu) = (w as usize, h as usize);
        let y: Vec<u8> = (0..wu * hu)
            .map(|i| (16 + (i % wu) * 219 / wu) as u8)
            .collect();
        let uv = vec![128u8; wu * hu.div_ceil(2)];
        let buffer = pool
            .fill(
                CpuPlane {
                    data: &y,
                    stride: wu,
                },
                Some(CpuPlane {
                    data: &uv,
                    stride: wu,
                }),
            )
            .expect("fill");
        CvPixelBufferFrame::new(buffer)
    }

    #[test]
    #[ignore = "needs a Mac with a hardware H.264 encoder (skips without one)"]
    fn native_frames_encode_zero_copy_and_start_with_an_idr() {
        if !hardware_present() {
            return;
        }
        let mut enc = VtEncoder::new(EncoderConfig::default()).expect("hardware H.264 encoder");
        let frame = capture_like(1920, 1080);
        let native = Frame {
            width: 1920,
            height: 1080,
            data: FrameData::Native(&frame),
        };
        let mut units = Vec::new();
        for _ in 0..10 {
            units.extend(enc.encode(&native).expect("encode"));
        }
        let first = units.first().expect("at least one unit");
        assert!(first.keyframe, "first unit must be an IDR");
        assert_eq!(&first.data[..4], &[0, 0, 0, 1], "Annex-B start code");
        // SPS (type 7) leads the keyframe.
        assert_eq!(first.data[4] & 0x1f, 7, "SPS first on the keyframe");
        assert!(
            units.iter().skip(1).all(|u| !u.keyframe),
            "no unrequested IDR"
        );
    }

    #[test]
    #[ignore = "needs a Mac with a hardware H.264 encoder (skips without one)"]
    fn a_larger_capture_is_scaled_to_the_encode_size() {
        if !hardware_present() {
            return;
        }
        let cfg = EncoderConfig {
            width: 1280,
            height: 720,
            ..EncoderConfig::default()
        };
        let mut enc = VtEncoder::new(cfg).expect("hardware H.264 encoder");
        let frame = capture_like(2560, 1440);
        assert_eq!(
            (
                CVPixelBufferGetWidth(frame.pixel_buffer()),
                CVPixelBufferGetHeight(frame.pixel_buffer())
            ),
            (2560, 1440)
        );
        let units = enc
            .encode(&Frame {
                width: 2560,
                height: 1440,
                data: FrameData::Native(&frame),
            })
            .expect("VideoToolbox scales a mismatched source");
        assert!(units.first().is_some_and(|u| u.keyframe));
    }

    #[test]
    #[ignore = "needs a Mac with a hardware H.264 encoder (skips without one)"]
    fn forced_keyframes_and_live_bitrate_changes() {
        if !hardware_present() {
            return;
        }
        let mut enc = VtEncoder::new(EncoderConfig::default()).expect("hardware H.264 encoder");
        let frame = capture_like(1920, 1080);
        let native = Frame {
            width: 1920,
            height: 1080,
            data: FrameData::Native(&frame),
        };
        for _ in 0..5 {
            enc.encode(&native).expect("encode");
        }
        enc.set_bitrate(2_000_000).expect("live bitrate change");
        enc.force_keyframe();
        let key = (0..5)
            .flat_map(|_| enc.encode(&native).expect("encode"))
            .next()
            .expect("a unit");
        assert!(key.keyframe, "forced keyframe");
        assert!(enc.set_bitrate(0).is_err());
    }
}
