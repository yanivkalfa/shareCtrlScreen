//! The `CodecFactory` adapter: hardware first, Cisco OpenH264 as the guaranteed
//! H.264.

use std::any::Any;
use std::sync::Arc;

use platform_api::{
    Codec, CodecError, CodecFactory, EncoderConfig, Frame, VideoDecoder, VideoEncoder,
};

use crate::binary::OpenH264;
use crate::decoder::SoftwareDecoder;
use crate::encoder::SoftwareEncoder;

/// Copies a platform's native (GPU) capture frame into CPU memory so the
/// software encoder can read it. The returned frame borrows the reader and is
/// valid until the next `read`.
pub trait FrameReadback {
    fn read<'a>(&'a mut self, frame: &Frame<'_>) -> Result<Frame<'a>, CodecError>;
}

/// Makes a [`FrameReadback`] bound to the capture device, on the host media
/// thread (like every other media object).
pub trait ReadbackFactory: Send + Sync {
    fn new_readback(&self, device: Option<&dyn Any>) -> Result<Box<dyn FrameReadback>, CodecError>;
}

/// Wraps a hardware [`CodecFactory`] and appends software H.264.
///
/// * Capabilities: the hardware lists, plus [`Codec::H264`] exactly once — the
///   protocol guarantees H.264, and this is what makes that true everywhere.
/// * Construction: hardware first (zero-copy, no CPU cost). Only an **H.264**
///   request the hardware refuses falls back to OpenH264; HEVC/AV1 failures are
///   returned as they are, because negotiation only picks those when the
///   hardware advertised them.
pub struct WithSoftwareH264<H> {
    hardware: H,
    openh264: Arc<OpenH264>,
    readback: Option<Box<dyn ReadbackFactory>>,
}

impl<H: CodecFactory> WithSoftwareH264<H> {
    pub fn new(hardware: H, openh264: Arc<OpenH264>) -> Self {
        Self {
            hardware,
            openh264,
            readback: None,
        }
    }

    /// For platforms whose capture produces only GPU frames.
    pub fn with_readback(mut self, readback: Box<dyn ReadbackFactory>) -> Self {
        self.readback = Some(readback);
        self
    }

    /// The shared Cisco-binary handle (enable flag, status) for the Settings page.
    pub fn openh264(&self) -> &Arc<OpenH264> {
        &self.openh264
    }

    pub fn hardware(&self) -> &H {
        &self.hardware
    }
}

fn with_h264(mut codecs: Vec<Codec>) -> Vec<Codec> {
    if !codecs.contains(&Codec::H264) {
        codecs.push(Codec::H264);
    }
    codecs
}

/// Both failures, so the session's error says why hardware was skipped *and*
/// what to do about the software codec.
fn both(hardware: CodecError, software: CodecError) -> CodecError {
    CodecError::Backend(format!(
        "no usable H.264 codec — hardware: {hardware}; software: {software}"
    ))
}

impl<H: CodecFactory> CodecFactory for WithSoftwareH264<H> {
    fn host_encodable(&self) -> Vec<Codec> {
        with_h264(self.hardware.host_encodable())
    }

    fn viewer_decodable(&self) -> Vec<Codec> {
        with_h264(self.hardware.viewer_decodable())
    }

    fn new_encoder(
        &self,
        device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        let hw_err = match self.hardware.new_encoder(device, cfg) {
            Ok(e) => return Ok(e),
            Err(e) if cfg.codec == Codec::H264 => e,
            Err(e) => return Err(e),
        };
        tracing::warn!("hardware H.264 encoder unavailable ({hw_err}) — trying Cisco OpenH264");
        // A capture without the GPU device the readback needs (a CPU capture,
        // a test mock) still encodes: only GPU frames need a readback, and
        // without one they fail per frame with a message saying so.
        let readback = self
            .readback
            .as_ref()
            .and_then(|f| match f.new_readback(device) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::info!("openh264: no GPU readback ({e}); CPU frames only");
                    None
                }
            });
        SoftwareEncoder::new(&self.openh264, cfg, readback)
            .map(|e| Box::new(e) as Box<dyn VideoEncoder>)
            .map_err(|sw| both(hw_err, sw))
    }

    fn new_decoder(
        &self,
        device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        let hw_err = match self.hardware.new_decoder(device, codec, width, height) {
            Ok(d) => return Ok(d),
            Err(e) if codec == Codec::H264 => e,
            Err(e) => return Err(e),
        };
        tracing::warn!("hardware H.264 decoder unavailable ({hw_err}) — trying Cisco OpenH264");
        SoftwareDecoder::new(&self.openh264, codec, width, height)
            .map(|d| Box::new(d) as Box<dyn VideoDecoder>)
            .map_err(|sw| both(hw_err, sw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockCodecFactory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A hardware factory with chosen capabilities whose construction either
    /// delegates to the mock codec or fails, counting calls.
    struct FakeHardware {
        encodes: Vec<Codec>,
        decodes: Vec<Codec>,
        works: bool,
        calls: AtomicUsize,
    }

    impl FakeHardware {
        fn new(encodes: Vec<Codec>, decodes: Vec<Codec>, works: bool) -> Self {
            Self {
                encodes,
                decodes,
                works,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl CodecFactory for FakeHardware {
        fn host_encodable(&self) -> Vec<Codec> {
            self.encodes.clone()
        }
        fn viewer_decodable(&self) -> Vec<Codec> {
            self.decodes.clone()
        }
        fn new_encoder(
            &self,
            device: Option<&dyn Any>,
            cfg: EncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CodecError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.works {
                MockCodecFactory.new_encoder(device, cfg)
            } else {
                Err(CodecError::NoEncoder(cfg.codec))
            }
        }
        fn new_decoder(
            &self,
            device: Option<&dyn Any>,
            codec: Codec,
            w: u32,
            h: u32,
        ) -> Result<Box<dyn VideoDecoder>, CodecError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.works {
                MockCodecFactory.new_decoder(device, codec, w, h)
            } else {
                Err(CodecError::NoDecoder(codec))
            }
        }
    }

    /// Never downloads: disabled, so a fallback attempt fails deterministically
    /// with the "turned off" message.
    fn disabled_openh264() -> Arc<OpenH264> {
        let h = OpenH264::new(std::env::temp_dir().join("sct-openh264-adapter-unused"));
        h.set_enabled(false);
        Arc::new(h)
    }

    #[test]
    fn appends_h264_exactly_once() {
        let none = WithSoftwareH264::new(
            FakeHardware::new(vec![], vec![], false),
            disabled_openh264(),
        );
        assert_eq!(none.host_encodable(), vec![Codec::H264]);
        assert_eq!(none.viewer_decodable(), vec![Codec::H264]);

        let hevc_only = WithSoftwareH264::new(
            FakeHardware::new(vec![Codec::Hevc], vec![Codec::Av1, Codec::Hevc], false),
            disabled_openh264(),
        );
        assert_eq!(hevc_only.host_encodable(), vec![Codec::Hevc, Codec::H264]);
        assert_eq!(
            hevc_only.viewer_decodable(),
            vec![Codec::Av1, Codec::Hevc, Codec::H264]
        );

        let has_it = WithSoftwareH264::new(
            FakeHardware::new(vec![Codec::H264, Codec::Hevc], vec![Codec::H264], true),
            disabled_openh264(),
        );
        assert_eq!(has_it.host_encodable(), vec![Codec::H264, Codec::Hevc]);
        assert_eq!(has_it.viewer_decodable(), vec![Codec::H264]);
    }

    #[test]
    fn prefers_working_hardware_and_never_touches_openh264() {
        // Disabled OpenH264 would fail; success proves the hardware path ran.
        let f = WithSoftwareH264::new(
            FakeHardware::new(vec![Codec::H264], vec![Codec::H264], true),
            disabled_openh264(),
        );
        assert!(f.new_encoder(None, EncoderConfig::default()).is_ok());
        assert!(f.new_decoder(None, Codec::H264, 64, 64).is_ok());
        assert_eq!(f.hardware().calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn falls_back_only_for_h264_and_reports_both_reasons() {
        let f = WithSoftwareH264::new(
            FakeHardware::new(vec![Codec::H264], vec![Codec::H264], false),
            disabled_openh264(),
        );
        let msg = f
            .new_encoder(None, EncoderConfig::default())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            msg.contains("hardware:") && msg.contains("Settings → Advanced"),
            "{msg}"
        );
        let msg = f
            .new_decoder(None, Codec::H264, 64, 64)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            msg.contains("software:") && msg.contains("turned off"),
            "{msg}"
        );

        // HEVC: the hardware error comes back untouched — no software attempt.
        let hevc = EncoderConfig {
            codec: Codec::Hevc,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            f.new_encoder(None, hevc),
            Err(CodecError::NoEncoder(Codec::Hevc))
        ));
        assert!(matches!(
            f.new_decoder(None, Codec::Av1, 64, 64),
            Err(CodecError::NoDecoder(Codec::Av1))
        ));
    }
}
