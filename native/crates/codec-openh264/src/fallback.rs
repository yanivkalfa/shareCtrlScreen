//! The `CodecFactory` adapter: hardware first, Cisco OpenH264 as the guaranteed
//! H.264.

use std::any::Any;
use std::sync::Arc;

use platform_api::{
    Codec, CodecError, CodecFactory, EncoderConfig, Frame, VideoDecoder, VideoEncoder,
};

use crate::binary::{Availability, OpenH264};
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
    native_probe: Option<NativeProbe>,
}

/// What the operating system itself can do with H.264, without OpenH264.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeH264 {
    pub encode: bool,
    pub decode: bool,
}

/// Asks the platform whether its own H.264 encoder/decoder exist (hardware or
/// the OS's software one). Cheap and side-effect free: it runs when the
/// Settings page is drawn.
pub type NativeProbe = Box<dyn Fn() -> NativeH264 + Send + Sync>;

impl<H: CodecFactory> WithSoftwareH264<H> {
    pub fn new(hardware: H, openh264: Arc<OpenH264>) -> Self {
        Self {
            hardware,
            openh264,
            readback: None,
            native_probe: None,
        }
    }

    /// For platforms that can tell, ahead of any session, whether the OS has
    /// its own H.264 — enables [`h264_problem`](Self::h264_problem).
    pub fn with_native_probe(mut self, probe: NativeProbe) -> Self {
        self.native_probe = Some(probe);
        self
    }

    /// `Some(advice)` when this computer would fail to host and/or view for lack
    /// of any H.264 codec: the OS has none for that direction **and** OpenH264
    /// cannot step in (turned off, no Cisco binary for this system, or its last
    /// download failed). `None` when everything works — or will work once the
    /// first-use download runs — and always `None` without a probe, because
    /// then the platform's own availability is unknown and a guess would
    /// either cry wolf or hide a real problem.
    pub fn h264_problem(&self) -> Option<String> {
        let native = (self.native_probe.as_ref()?)();
        let what = match (native.encode, native.decode) {
            (true, true) => return None,
            (false, true) => "share its screen",
            (true, false) => "show another computer's screen",
            (false, false) => "share its screen or show another computer's screen",
        };
        let fix = match self.openh264.availability() {
            Availability::Ready | Availability::NotDownloaded => return None,
            Availability::Disabled => {
                "Turn on \"Use Cisco OpenH264\" in Settings → Advanced to fix this.".to_string()
            }
            Availability::Unsupported => {
                "Cisco provides no software codec for this system; install the \
                 operating system's media codecs (on Windows \"N\" editions, the \
                 Media Feature Pack)."
                    .to_string()
            }
            Availability::Failed(msg) => format!(
                "The software codec could not be downloaded: {msg} You can also install \
                 the operating system's media codecs (on Windows \"N\" editions, the \
                 Media Feature Pack)."
            ),
        };
        Some(format!(
            "This computer has no H.264 video codec, so it cannot {what}. {fix}"
        ))
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

    fn probed(native: NativeH264, openh264: Arc<OpenH264>) -> WithSoftwareH264<FakeHardware> {
        WithSoftwareH264::new(FakeHardware::new(vec![], vec![], false), openh264)
            .with_native_probe(Box::new(move || native))
    }

    const NOTHING: NativeH264 = NativeH264 {
        encode: false,
        decode: false,
    };
    const EVERYTHING: NativeH264 = NativeH264 {
        encode: true,
        decode: true,
    };

    /// A handle in a fresh, empty directory: enabled, nothing downloaded.
    fn not_downloaded(tag: &str) -> Arc<OpenH264> {
        let dir = std::env::temp_dir().join(format!("sct-openh264-policy-{tag}"));
        Arc::new(OpenH264::new(dir))
    }

    #[test]
    fn no_problem_when_the_os_has_h264() {
        // Even with OpenH264 off: the OS codec is enough, so say nothing.
        assert_eq!(probed(EVERYTHING, disabled_openh264()).h264_problem(), None);
    }

    #[test]
    fn no_problem_while_the_download_can_still_happen() {
        let f = probed(NOTHING, not_downloaded("pending"));
        if f.openh264().availability() == Availability::NotDownloaded {
            assert_eq!(f.h264_problem(), None);
        }
    }

    #[test]
    fn problem_names_the_missing_direction_and_the_fix_when_openh264_is_off() {
        let both = probed(NOTHING, disabled_openh264())
            .h264_problem()
            .unwrap_or_default();
        assert!(
            both.contains("share its screen or show another computer's screen")
                && both.contains("Settings → Advanced"),
            "{both}"
        );
        let host_only = probed(
            NativeH264 {
                encode: false,
                decode: true,
            },
            disabled_openh264(),
        )
        .h264_problem()
        .unwrap_or_default();
        assert!(
            host_only.contains("cannot share its screen.") && !host_only.contains("show another"),
            "{host_only}"
        );
        let viewer_only = probed(
            NativeH264 {
                encode: true,
                decode: false,
            },
            disabled_openh264(),
        )
        .h264_problem()
        .unwrap_or_default();
        assert!(
            viewer_only.contains("cannot show another computer's screen.")
                && !viewer_only.contains("share its screen"),
            "{viewer_only}"
        );
    }

    #[test]
    fn turning_it_off_reports_instead_of_downloading() {
        let handle = not_downloaded("off-no-download");
        handle.set_enabled(false);
        let f = probed(NOTHING, handle);
        assert!(f.h264_problem().is_some());
        // Neither the report nor a codec attempt may touch the network or disk.
        assert!(f.new_decoder(None, Codec::H264, 64, 64).is_err());
        assert!(f.new_encoder(None, EncoderConfig::default()).is_err());
        assert!(f.openh264().library_path().is_none_or(|p| !p.exists()));
    }

    #[test]
    fn no_probe_means_no_claim() {
        let f = WithSoftwareH264::new(
            FakeHardware::new(vec![], vec![], false),
            disabled_openh264(),
        );
        assert_eq!(f.h264_problem(), None);
    }

    #[test]
    fn downloaded_library_clears_the_problem() {
        let handle = not_downloaded("ready");
        let Some(path) = handle.library_path() else {
            return; // Cisco ships nothing for this target: nothing to pin.
        };
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() || std::fs::write(&path, b"x").is_err() {
            return;
        }
        let f = probed(NOTHING, handle);
        assert_eq!(f.openh264().availability(), Availability::Ready);
        assert_eq!(f.h264_problem(), None);
        let _ = std::fs::remove_file(&path);
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
