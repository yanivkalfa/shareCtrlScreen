//! The macOS hardware `CodecFactory`: VideoToolbox encode (task M2) and
//! decode (task M3). The platform wraps it in
//! `codec_openh264::WithSoftwareH264`, so H.264 is always available and a
//! Mac where VideoToolbox refuses H.264 falls back to Cisco OpenH264.
//!
//! Encode capabilities (plans/01 §4): H.264 always (the protocol guarantee,
//! the fallback covers it); HEVC when a hardware HEVC session can actually
//! be made — probed once, in the background from platform creation
//! ([`VtCodecs::start_probe`]), and logged; AV1 never — no Mac encodes it.
//!
//! Decode capabilities: H.264 always (VideoToolbox has a software decoder
//! behind the hardware one); HEVC and AV1 only where the hardware decodes
//! them (`decode::decodable`).

use std::any::Any;
use std::sync::{Arc, OnceLock};

use platform_api::{Codec, CodecError, CodecFactory, EncoderConfig, VideoDecoder, VideoEncoder};

use crate::decode::{self, VtDecoder};
use crate::vt_encoder::{self, VtEncoder};
use crate::vt_params::{even_size, validate};

pub(crate) struct VtCodecs {
    /// The HEVC probe's answer. Shared with the thread that computes it.
    hevc: Arc<OnceLock<bool>>,
}

impl VtCodecs {
    pub(crate) fn new() -> Self {
        Self {
            hevc: Arc::new(OnceLock::new()),
        }
    }

    /// Start the HEVC probe now, off the caller's thread. Building and
    /// tearing down a hardware session takes tens of milliseconds, and the
    /// first `host_encodable` runs inside the engine's codec negotiation; a
    /// probe started at platform creation is normally done by then. Should
    /// negotiation come first, `OnceLock` makes it wait for this probe
    /// rather than run a second one.
    pub(crate) fn start_probe(&self) {
        let hevc = Arc::clone(&self.hevc);
        let spawned = std::thread::Builder::new()
            .name("vt-hevc-probe".into())
            .spawn(move || {
                hevc.get_or_init(|| vt_encoder::probe(Codec::Hevc));
            });
        if let Err(e) = spawned {
            // Not fatal: the first `host_encodable` probes instead.
            tracing::warn!("VideoToolbox: could not start the HEVC probe thread ({e})");
        }
    }

    fn hevc_encodable(&self) -> bool {
        *self.hevc.get_or_init(|| vt_encoder::probe(Codec::Hevc))
    }
}

impl CodecFactory for VtCodecs {
    fn host_encodable(&self) -> Vec<Codec> {
        let mut codecs = vec![Codec::H264];
        if self.hevc_encodable() {
            codecs.push(Codec::Hevc);
        }
        codecs
    }

    fn viewer_decodable(&self) -> Vec<Codec> {
        decode::decodable()
    }

    /// `device` is unused: ScreenCaptureKit frames are IOSurface-backed and
    /// VideoToolbox reads them without a shared GPU device (`gpu` module
    /// docs), so the capture hands out none. Odd sizes are rounded down to
    /// even (`vt_params::even_size`).
    fn new_encoder(
        &self,
        _device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        if cfg.codec == Codec::Av1 {
            return Err(CodecError::NoEncoder(Codec::Av1));
        }
        let cfg = even_size(cfg);
        validate(&cfg)?;
        Ok(Box::new(VtEncoder::new(cfg)?))
    }

    /// `device` is unused: VideoToolbox needs no GPU device, and its
    /// IOSurface-backed output can be wrapped by any Metal device (the
    /// presenter's `device()` is `None` for the same reason).
    fn new_decoder(
        &self,
        _device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        match codec {
            // VideoToolbox has a software H.264 decoder behind the hardware
            // one, but the decoder builds its session only at the first
            // keyframe: open (and drop) a probe session now, so a Mac where
            // VideoToolbox refuses H.264 fails here and `WithSoftwareH264`
            // decodes with Cisco OpenH264 instead.
            Codec::H264 => decode::probe_h264()?,
            // HEVC/AV1 are only negotiated when advertised, i.e. when the
            // hardware decodes them; refuse here rather than at the first
            // keyframe so the session ends with a clear reason.
            Codec::Hevc | Codec::Av1 if !decode::hardware_decodes(codec) => {
                return Err(CodecError::NoDecoder(codec));
            }
            Codec::Hevc | Codec::Av1 => {}
        }
        Ok(Box::new(VtDecoder::new(codec, width, height)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codecs() -> VtCodecs {
        VtCodecs::new()
    }

    #[test]
    fn av1_and_invalid_configs_are_refused_before_any_session() {
        let f = codecs();
        let av1 = EncoderConfig {
            codec: Codec::Av1,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            f.new_encoder(None, av1),
            Err(CodecError::NoEncoder(Codec::Av1))
        ));
        let zero = EncoderConfig {
            bitrate_bps: 0,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            f.new_encoder(None, zero),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn advertises_h264_and_only_hardware_decoded_codecs() {
        let f = codecs();
        let decodable = f.viewer_decodable();
        assert_eq!(decodable.last(), Some(&Codec::H264));
        for codec in [Codec::Hevc, Codec::Av1] {
            assert_eq!(
                decodable.contains(&codec),
                decode::hardware_decodes(codec),
                "{codec:?}"
            );
            let made = f.new_decoder(None, codec, 1280, 720);
            if decode::hardware_decodes(codec) {
                assert!(made.is_ok(), "{codec:?}");
            } else {
                assert!(
                    matches!(made, Err(CodecError::NoDecoder(c)) if c == codec),
                    "{codec:?}"
                );
            }
        }
        let h264 = f
            .new_decoder(None, Codec::H264, 1280, 720)
            .unwrap_or_else(|e| panic!("{e}"));
        // The hint is what is reported before the first picture.
        assert_eq!(h264.dimensions(), (1280, 720));
    }

    /// Runs the real HEVC probe; its answer depends on the Mac, the shape of
    /// the list does not.
    #[test]
    fn h264_is_listed_first_and_av1_never() {
        let codecs = codecs().host_encodable();
        assert_eq!(codecs.first(), Some(&Codec::H264));
        assert!(!codecs.contains(&Codec::Av1));
    }
}
