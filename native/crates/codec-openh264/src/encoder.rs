//! `VideoEncoder` over Cisco OpenH264, configured with the same low-latency
//! recipe as the Media Foundation encoder (`codec::encode::apply_low_latency_recipe`).

use std::os::raw::{c_int, c_void};
use std::time::Instant;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType,
    IntraFramePeriod, QpRange, RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::Timestamp;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE,
    SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
};
use platform_api::{
    Codec, CodecError, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat, VideoEncoder,
};

use crate::binary::OpenH264;
use crate::convert::{self, I420};
use crate::FrameReadback;

/// The quality floor, identical to the Media Foundation encoder's `MAX_QP`: the
/// encoder may not blur past this, so on a starved link it overshoots the
/// bitrate and the host's pacing drops frames instead (sharp but slower).
pub const MAX_QP: u8 = 32;

/// OpenH264 supports up to level 5.2 (its README: "Encoder errors when
/// resolution exceeds 3840x2160 or 2160x3840"). Larger screens are encoded
/// scaled down to fit; the viewer adapts to whatever size arrives.
const MAX_LONG_SIDE: u32 = 3840;
const MAX_SHORT_SIDE: u32 = 2160;

/// Headroom of the rate controller's ceiling over its target. OpenH264 rejects
/// a target above the ceiling, and logs that ceiling == target "will make the
/// actual bit rate lower than SpatialBitrate" (encoder_ext.cpp,
/// `WelsBitRateVerification`).
fn ceiling(bps: u32) -> u32 {
    bps.saturating_add(bps / 2)
}

/// The encode size for `cfg`: the configured size when OpenH264 can take it,
/// otherwise the largest even size with the same aspect that it can.
fn encode_size(width: u32, height: u32) -> (u32, u32) {
    let (long, short) = (width.max(height), width.min(height));
    let scale = f64::min(
        1.0,
        f64::min(
            MAX_LONG_SIDE as f64 / long as f64,
            MAX_SHORT_SIDE as f64 / short as f64,
        ),
    );
    let fit = |v: u32| (((v as f64 * scale) as u32) & !1).max(2);
    (fit(width), fit(height))
}

pub struct SoftwareEncoder {
    inner: Encoder,
    /// The converted picture handed to OpenH264, reused every frame.
    picture: I420,
    /// Resampling scratch (only used when the input size differs).
    scratch: Vec<u8>,
    /// Turns the capture's GPU frames into CPU ones, when the platform has one.
    readback: Option<Box<dyn FrameReadback>>,
    started: Instant,
    /// OpenH264 initialises itself on the first `EncodeFrame`; options set
    /// before that are rejected, so bitrate changes wait until then.
    initialized: bool,
    bitrate: u32,
    pending_bitrate: Option<u32>,
}

impl SoftwareEncoder {
    pub fn new(
        openh264: &OpenH264,
        cfg: EncoderConfig,
        readback: Option<Box<dyn FrameReadback>>,
    ) -> Result<Self, CodecError> {
        if cfg.codec != Codec::H264 {
            return Err(CodecError::NoEncoder(cfg.codec));
        }
        if cfg.width < 2 || cfg.height < 2 || cfg.fps_den == 0 || cfg.fps_num == 0 {
            return Err(CodecError::Unsupported(format!(
                "OpenH264 encoder: invalid configuration {}x{} @ {}/{}",
                cfg.width, cfg.height, cfg.fps_num, cfg.fps_den
            )));
        }
        let api = openh264
            .load()
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let (w, h) = encode_size(cfg.width, cfg.height);
        let fps = cfg.fps_num as f32 / cfg.fps_den as f32;
        // codec_app_def.h names in brackets.
        let config = OhConfig::new()
            // [iUsageType = SCREEN_CONTENT_REAL_TIME] screen-content tools
            // (palette-like intra, static-region skipping) and real-time pacing.
            .usage_type(UsageType::ScreenContentRealTime)
            // [iRCMode = RC_BITRATE_MODE] steered live by the transport's BWE.
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(cfg.bitrate_bps))
            // [fMaxFrameRate] must be the rate actually fed (see EncoderConfig).
            .max_frame_rate(FrameRate::from_hz(fps))
            // [bEnableFrameSkip = false] never drop a frame inside the encoder:
            // the host's pacing decides which frames exist, and a skipped frame
            // here would be a captured frame the viewer never sees. OpenH264
            // warns that the bitrate then "can't be controlled" — intended: with
            // the QP floor it overshoots, and the host's token bucket pays for
            // that in frame rate, exactly as on the Media Foundation path.
            .skip_frames(false)
            // [iMaxQp] quality floor (see MAX_QP).
            .qp(QpRange::new(0, MAX_QP))
            // [uiIntraPeriod = 0] no periodic IDR; keyframes on demand
            // (ForceIntraFrame).
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            // [bEnableSceneChangeDetect] OpenH264 forces this on for screen
            // content (encoder_ext.cpp ParamValidation), so a whole-screen change
            // can still yield an encoder-initiated IDR — the same behaviour as
            // Media Foundation, and what the host's keyframe attribution counts.
            // Saying so here keeps the configuration honest.
            .scene_change_detect(true)
            // [bEnableAdaptiveQuant / bEnableBackgroundDetection] unsupported for
            // screen content (OpenH264 turns them off with a warning).
            .adaptive_quantization(false)
            .background_detection(false)
            // [eSpsPpsIdStrategy = CONSTANT_ID] the same SPS/PPS ids every IDR,
            // so a viewer that joins on any keyframe decodes it stand-alone.
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // [bEnableLongTermReference = false] LTR would turn scene changes
            // into references the viewer must have kept; without loss feedback
            // from the viewer that is not safe on an unreliable link.
            .long_term_reference(false)
            // [iComplexityMode = LOW_COMPLEXITY] speed over quality: this is the
            // fallback for machines without a hardware encoder, i.e. the slow
            // ones; the QP floor already guards legibility.
            .complexity(Complexity::Low)
            // [iMultipleThreadIdc = 1] threads only split slices, and the recipe
            // is one slice per picture (the wrapper sets SM_SINGLE_SLICE).
            .num_threads(1)
            // VUI: BT.709 limited range — what the converter produces.
            .vui(VuiConfig::bt709());
        let inner = Encoder::with_api_config(api, config)
            .map_err(|e| CodecError::Backend(format!("OpenH264 encoder: {e}")))?;
        tracing::info!(
            "openh264: encoder {}x{} (asked {}x{}) @{fps}fps {} kbps",
            w,
            h,
            cfg.width,
            cfg.height,
            cfg.bitrate_bps / 1000
        );
        Ok(Self {
            inner,
            picture: I420::new(w as usize, h as usize),
            scratch: Vec::new(),
            readback,
            started: Instant::now(),
            initialized: false,
            bitrate: cfg.bitrate_bps,
            pending_bitrate: None,
        })
    }

    fn set_option(&mut self, option: ENCODER_OPTION, layer: c_int, bps: u32) -> c_int {
        let mut info = SBitrateInfo {
            iLayer: layer,
            iBitrate: bps.min(c_int::MAX as u32) as c_int,
        };
        // SAFETY: the encoder is initialised (`initialized`), and both bitrate
        // options take a pointer to an `SBitrateInfo` that lives for the call.
        unsafe {
            self.inner
                .raw_api()
                .set_option(option, (&mut info as *mut SBitrateInfo).cast::<c_void>())
        }
    }

    fn apply_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        // Order matters: OpenH264 rejects a target above the ceiling, so raise
        // the ceiling first when going up and lower it last when going down.
        let target = |s: &mut Self| s.set_option(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bps);
        let max =
            |s: &mut Self| s.set_option(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, ceiling(bps));
        let rc = if bps >= self.bitrate {
            max(self).max(target(self))
        } else {
            target(self).max(max(self))
        };
        if rc != 0 {
            return Err(CodecError::Backend(format!(
                "OpenH264 rejected bitrate {bps} bps (error {rc})"
            )));
        }
        self.bitrate = bps;
        Ok(())
    }
}

/// Fill `pic` from a CPU frame: crop by at most one pixel (odd screen sizes),
/// otherwise resample.
fn fill(pic: &mut I420, scratch: &mut Vec<u8>, frame: &Frame<'_>) -> Result<(), CodecError> {
    let (w, h) = pic.size();
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let crop = fw >= w && fh >= h && fw - w <= 1 && fh - h <= 1;
    let short = |what: &str| CodecError::Unsupported(format!("{what} too small for {fw}x{fh}"));
    match frame.data {
        FrameData::Cpu {
            format: PixelFormat::Bgra8,
            plane0,
            ..
        } => {
            if fw == 0
                || fh == 0
                || plane0.stride < fw * 4
                || plane0.data.len() < plane0.stride * (fh - 1) + fw * 4
            {
                return Err(short("BGRA buffer"));
            }
            if crop {
                convert::bgra_to_i420(plane0.data, plane0.stride, pic);
            } else {
                scratch.resize(w * h * 4, 0);
                convert::scale_bilinear::<4>(plane0.data, fw, fh, plane0.stride, scratch, w, h);
                convert::bgra_to_i420(scratch, w * 4, pic);
            }
            Ok(())
        }
        FrameData::Cpu {
            format: PixelFormat::Nv12,
            plane0: y,
            plane1: Some(uv),
        } => {
            let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
            if fw == 0
                || fh == 0
                || y.stride < fw
                || y.data.len() < y.stride * (fh - 1) + fw
                || uv.stride < cw * 2
                || uv.data.len() < uv.stride * (ch - 1) + cw * 2
            {
                return Err(short("NV12 planes"));
            }
            if crop {
                convert::nv12_to_i420(y.data, y.stride, uv.data, uv.stride, pic);
            } else {
                convert::scale_bilinear::<1>(y.data, fw, fh, y.stride, &mut pic.y, w, h);
                scratch.resize(w * h / 2, 0);
                convert::scale_bilinear::<2>(uv.data, cw, ch, uv.stride, scratch, w / 2, h / 2);
                convert::split_uv(scratch, w, w / 2, h / 2, &mut pic.u, &mut pic.v);
            }
            Ok(())
        }
        FrameData::Cpu { format, .. } => Err(CodecError::Unsupported(format!(
            "OpenH264 encoder: {format:?} frame without its chroma plane"
        ))),
        FrameData::Native(_) => Err(CodecError::Unsupported(
            "OpenH264 encoder needs CPU frames and this platform has no GPU readback".into(),
        )),
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        match (&frame.data, self.readback.as_mut()) {
            (FrameData::Native(_), Some(readback)) => {
                let cpu = readback.read(frame)?;
                fill(&mut self.picture, &mut self.scratch, &cpu)?;
            }
            _ => fill(&mut self.picture, &mut self.scratch, frame)?,
        }
        // Real capture times: the rate controller uses them to pace bits.
        let ts = Timestamp::from_millis(self.started.elapsed().as_millis() as u64);
        let (data, frame_type) = {
            let bs = self
                .inner
                .encode_at(&self.picture, ts)
                .map_err(|e| CodecError::Backend(format!("OpenH264 encode: {e}")))?;
            (bs.to_vec(), bs.frame_type())
        };
        if !self.initialized {
            self.initialized = true;
            if let Some(bps) = self.pending_bitrate.take() {
                self.apply_bitrate(bps)?;
            }
        }
        // Frame skipping is off, but a skip/invalid result is still "no
        // picture this time", which the contract allows.
        if data.is_empty() || matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(Vec::new());
        }
        Ok(vec![EncodedUnit {
            // Every NAL already carries its 4-byte start code (Annex-B), and an
            // IDR access unit begins with SPS + PPS.
            data,
            keyframe: frame_type == FrameType::IDR,
        }])
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 {
            return Err(CodecError::Unsupported("bitrate 0".into()));
        }
        if !self.initialized {
            // Applied right after the first frame; that one frame uses the
            // construction bitrate.
            self.pending_bitrate = Some(bps);
            return Ok(());
        }
        self.apply_bitrate(bps)
    }

    fn force_keyframe(&mut self) {
        // Before initialisation the first frame is an IDR anyway.
        if self.initialized {
            self.inner.force_intra_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_screens_fit_openh264_limits_keeping_aspect() {
        assert_eq!(encode_size(1920, 1080), (1920, 1080));
        assert_eq!(encode_size(3840, 2160), (3840, 2160));
        assert_eq!(encode_size(5120, 2880), (3840, 2160));
        assert_eq!(encode_size(2160, 3840), (2160, 3840));
        let (w, h) = encode_size(6016, 3384);
        assert!(w <= 3840 && h <= 2160 && w.is_multiple_of(2) && h.is_multiple_of(2));
    }

    #[test]
    fn ceiling_has_headroom_and_never_overflows() {
        assert_eq!(ceiling(8_000_000), 12_000_000);
        assert_eq!(ceiling(u32::MAX), u32::MAX);
    }
}
