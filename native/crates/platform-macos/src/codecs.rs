//! The macOS hardware `CodecFactory`: VideoToolbox encode (task M2). The
//! platform wraps it in `codec_openh264::WithSoftwareH264`, so H.264 is
//! always available and a Mac without a usable hardware H.264 encoder falls
//! back to Cisco OpenH264.
//!
//! Capabilities (plans/01 §4): H.264 always (the protocol guarantee, the
//! fallback covers it); HEVC when a hardware HEVC session can actually be
//! made — probed once, in the background from platform creation
//! ([`VtCodecs::start_probe`]), and logged; AV1 never — no Mac encodes it.
//! Decoding is task M3; until it lands the decoder side is the fail-safe
//! [`NoCodecs`], which the wrapper backs with OpenH264.

use std::any::Any;
use std::sync::{Arc, OnceLock};

use platform_api::unavailable::NoCodecs;
use platform_api::{Codec, CodecError, CodecFactory, EncoderConfig, VideoDecoder, VideoEncoder};

use crate::vt_encoder::{self, VtEncoder};
use crate::vt_params::{even_size, validate};

pub(crate) struct VtCodecs {
    /// The HEVC probe's answer. Shared with the thread that computes it.
    hevc: Arc<OnceLock<bool>>,
    decode: NoCodecs,
}

impl VtCodecs {
    pub(crate) fn new(decode: NoCodecs) -> Self {
        Self {
            hevc: Arc::new(OnceLock::new()),
            decode,
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
        self.decode.viewer_decodable()
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

    fn new_decoder(
        &self,
        device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.decode.new_decoder(device, codec, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codecs() -> VtCodecs {
        VtCodecs::new(NoCodecs {
            reason: "no decoder in this test",
        })
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
    fn decoding_is_left_to_the_fallback_until_m3() {
        let f = codecs();
        assert_eq!(f.viewer_decodable(), vec![Codec::H264]);
        assert!(f.new_decoder(None, Codec::H264, 64, 64).is_err());
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
