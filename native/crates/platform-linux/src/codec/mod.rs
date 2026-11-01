//! Linux codec factory: hardware H.264/HEVC encode through NVENC (NVIDIA) and
//! VA-API (Intel/AMD), both loaded at run time (task L3).
//!
//! Discovery is a real probe, run once on first use and logged:
//! * NVENC — `dlopen` the driver's `libnvidia-encode.so.1` + `libcuda.so.1`,
//!   `NvEncodeAPIGetMaxSupportedVersion`, open a session, list codec GUIDs.
//! * VA-API — `dlopen` `libva.so.2` + `libva-drm.so.2`, `vaInitialize` each
//!   `/dev/dri/renderD*` node (libva ≥ 2.20 only, decision #7), then
//!   `vaQueryConfigProfiles` / `vaQueryConfigEntrypoints` for H.264 encode.
//!
//! Selection: NVENC first when present (better quality per bit at this latency
//! than VDEnc/VCN, and it is the only HEVC encoder here), VA-API next. A backend
//! that probes fine but fails to build an encoder (sessions exhausted, size out
//! of range) falls through to the next one.
//!
//! The contract says `host_encodable` always contains H.264 (the protocol's
//! guaranteed fallback), so it does even with no hardware; `new_encoder` then
//! returns [`CodecError::NoEncoder`] — the seam where the software encoder
//! (task D2) slots in. Decoding is task L4; until it lands the decoder side is
//! the scaffold's fail-safe [`NoCodecs`].

#[cfg(any(feature = "vaapi", feature = "nvenc"))]
mod annexb;
// Only VA-API writes headers; NVENC's tests reuse its SPS reader.
#[cfg(any(feature = "vaapi", all(test, feature = "nvenc")))]
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
mod h264;
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
mod level;
#[cfg(feature = "nvenc")]
mod nvenc;
#[cfg(feature = "nvenc")]
mod scale;
#[cfg(feature = "vaapi")]
mod vaapi;
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
mod zeroed;

use std::any::Any;
use std::sync::OnceLock;

use platform_api::unavailable::NoCodecs;
use platform_api::{Codec, CodecError, CodecFactory, EncoderConfig, VideoDecoder, VideoEncoder};

/// Quality floor: the worst quantizer any picture may use — the same value and
/// reasoning as the Windows encoder (`codec::encode::MAX_QP`): past ~32 text
/// stops being legible, so the encoder overshoots the bitrate instead and the
/// engine's source pacing drops frames rather than detail.
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
pub(crate) const MAX_QP: u32 = 32;

/// Refuse configurations no backend can honour, before any GPU object is made
/// (a zero rate would divide by zero in the VBV maths; zero sizes make
/// drivers fail in ways that do not name the cause).
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
pub(crate) fn validate(cfg: &EncoderConfig) -> Result<(), CodecError> {
    let problem = if cfg.width < 2 || cfg.height < 2 {
        "a width or height below 2"
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

/// 4:2:0 chroma is subsampled 2×2, so the hardware encoders (and the SPS
/// cropping maths) want even sizes. The engine already asks for even sizes;
/// anything odd is rounded down by one pixel here rather than refused.
/// Sizes below 2 are left for [`validate`] to refuse.
fn even_size(mut cfg: EncoderConfig) -> EncoderConfig {
    if cfg.width >= 2 {
        cfg.width &= !1;
    }
    if cfg.height >= 2 {
        cfg.height &= !1;
    }
    cfg
}

/// One frame's worth of the CBR budget: the single-frame VBV/HRD buffer the
/// NVENC low-latency guidance recommends, used by both backends so a keyframe
/// burst never queues more than about a frame interval on the link. Never 0
/// (0 means "driver default", which is seconds of buffering).
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
pub(crate) fn vbv_bits(bitrate_bps: u32, fps_num: u32, fps_den: u32) -> u32 {
    let bits = bitrate_bps as u64 * fps_den.max(1) as u64 / fps_num.max(1) as u64;
    bits.clamp(1, u32::MAX as u64) as u32
}

/// A hardware encoder the probe found.
#[derive(Debug, Clone)]
enum Backend {
    #[cfg(feature = "nvenc")]
    Nvenc(nvenc::Probe),
    #[cfg(feature = "vaapi")]
    Vaapi(vaapi::Probe),
}

impl Backend {
    fn encodes(&self, codec: Codec) -> bool {
        match (self, codec) {
            #[cfg(feature = "nvenc")]
            (Backend::Nvenc(p), Codec::H264) => p.h264,
            #[cfg(feature = "nvenc")]
            (Backend::Nvenc(p), Codec::Hevc) => p.hevc,
            #[cfg(feature = "vaapi")]
            (Backend::Vaapi(_), Codec::H264) => true,
            _ => false,
        }
    }

    // With no backend compiled in, `Backend` has no variants and `cfg` is
    // never read.
    #[cfg_attr(
        not(any(feature = "vaapi", feature = "nvenc")),
        allow(unused_variables)
    )]
    fn create(&self, cfg: EncoderConfig) -> Result<Box<dyn VideoEncoder>, CodecError> {
        // `*self` so the match is exhaustive even when no backend is compiled in.
        match *self {
            #[cfg(feature = "nvenc")]
            Backend::Nvenc(_) => Ok(Box::new(nvenc::NvencEncoder::new(cfg)?)),
            #[cfg(feature = "vaapi")]
            Backend::Vaapi(ref p) => Ok(Box::new(vaapi::VaapiEncoder::new(p, cfg)?)),
        }
    }

    fn describe(&self) -> String {
        match *self {
            #[cfg(feature = "nvenc")]
            Backend::Nvenc(ref p) => format!(
                "NVENC on {} (driver API {}.{}; H.264 {}, HEVC {})",
                p.device,
                p.api.0,
                p.api.1,
                yes_no(p.h264),
                yes_no(p.hevc)
            ),
            #[cfg(feature = "vaapi")]
            Backend::Vaapi(ref p) => p.describe(),
        }
    }
}

#[cfg(feature = "nvenc")]
fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Probe results: the usable backends in preference order, and why each
/// missing one is missing (for the log and for "no encoder" errors).
#[derive(Debug, Default)]
struct Capabilities {
    found: Vec<Backend>,
    missing: Vec<String>,
}

impl Capabilities {
    fn detect() -> Self {
        let mut caps = Capabilities::default();
        #[cfg(feature = "nvenc")]
        match nvenc::probe() {
            Ok(p) => caps.found.push(Backend::Nvenc(p)),
            Err(e) => caps.missing.push(format!("NVENC: {e}")),
        }
        #[cfg(feature = "vaapi")]
        match vaapi::probe() {
            Ok(p) => caps.found.push(Backend::Vaapi(p)),
            Err(e) => caps.missing.push(format!("VA-API: {e}")),
        }
        if cfg!(not(any(feature = "vaapi", feature = "nvenc"))) {
            caps.missing
                .push("built without hardware encoders (features `vaapi`, `nvenc`)".into());
        }
        caps
    }

    fn log(&self) {
        for b in &self.found {
            tracing::info!("hardware encoder: {}", b.describe());
        }
        for m in &self.missing {
            tracing::info!("hardware encoder unavailable — {m}");
        }
        if self.found.is_empty() {
            tracing::warn!("no hardware video encoder; only a software codec can host");
        }
    }

    fn encodable(&self) -> Vec<Codec> {
        let mut out = Vec::new();
        if self.found.iter().any(|b| b.encodes(Codec::Hevc)) {
            out.push(Codec::Hevc);
        }
        // Always: the protocol's guaranteed fallback (see the module docs).
        out.push(Codec::H264);
        out
    }
}

/// The Linux [`CodecFactory`].
pub struct LinuxCodecs {
    caps: OnceLock<Capabilities>,
    decode: NoCodecs,
}

impl LinuxCodecs {
    pub fn new() -> Self {
        Self {
            caps: OnceLock::new(),
            decode: NoCodecs {
                reason: "video decoding is not available on Linux in this version",
            },
        }
    }

    /// Probe on first use (opening GPU devices is not free, and many processes
    /// — the viewer side, tests — never encode).
    fn caps(&self) -> &Capabilities {
        self.caps.get_or_init(|| {
            let caps = Capabilities::detect();
            caps.log();
            caps
        })
    }
}

impl Default for LinuxCodecs {
    fn default() -> Self {
        Self::new()
    }
}

impl CodecFactory for LinuxCodecs {
    fn host_encodable(&self) -> Vec<Codec> {
        self.caps().encodable()
    }

    fn viewer_decodable(&self) -> Vec<Codec> {
        self.decode.viewer_decodable()
    }

    /// `device` is unused: v1 takes CPU frames on every backend. A capture
    /// backend that hands out DMA-BUFs (task L3b) passes its DRM device here.
    /// Odd sizes are rounded down to even (see `even_size`); the encoder then
    /// scales the frame to that size like any other.
    fn new_encoder(
        &self,
        _device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        let cfg = even_size(cfg);
        let caps = self.caps();
        let mut last_err = None;
        for backend in caps.found.iter().filter(|b| b.encodes(cfg.codec)) {
            match backend.create(cfg) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::warn!("{} could not start: {e}", backend.describe());
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            tracing::warn!(
                "no hardware {} encoder ({})",
                cfg.codec.as_caps_str(),
                caps.missing.join("; ")
            );
            CodecError::NoEncoder(cfg.codec)
        }))
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

    #[cfg(any(feature = "vaapi", feature = "nvenc"))]
    #[test]
    fn vbv_is_one_frame_and_never_zero() {
        assert_eq!(vbv_bits(6_000_000, 60, 1), 100_000);
        assert_eq!(vbv_bits(6_000_000, 30_000, 1001), 200_200);
        assert_eq!(vbv_bits(10, 60, 1), 1);
        assert_eq!(vbv_bits(1_000_000, 0, 0), 1_000_000);
    }

    #[cfg(any(feature = "vaapi", feature = "nvenc"))]
    #[test]
    fn invalid_configs_are_refused() {
        let ok = EncoderConfig::default();
        assert!(validate(&ok).is_ok());
        for bad in [
            EncoderConfig { width: 0, ..ok },
            EncoderConfig { height: 1, ..ok },
            EncoderConfig { fps_num: 0, ..ok },
            EncoderConfig { fps_den: 0, ..ok },
            EncoderConfig {
                bitrate_bps: 0,
                ..ok
            },
        ] {
            assert!(
                matches!(validate(&bad), Err(CodecError::Unsupported(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let cfg = even_size(EncoderConfig {
            width: 1921,
            height: 1081,
            ..EncoderConfig::default()
        });
        assert_eq!((cfg.width, cfg.height), (1920, 1080));
        // Too small to round: left for `validate` to refuse.
        let cfg = even_size(EncoderConfig {
            width: 1,
            height: 0,
            ..EncoderConfig::default()
        });
        assert_eq!((cfg.width, cfg.height), (1, 0));
    }

    #[test]
    fn h264_is_always_advertised() {
        let none = Capabilities::default();
        assert_eq!(none.encodable(), vec![Codec::H264]);
    }

    #[cfg(feature = "nvenc")]
    #[test]
    fn hevc_only_when_a_backend_encodes_it() {
        let probe = nvenc::Probe {
            device: "test".into(),
            api: (12, 0),
            h264: true,
            hevc: true,
        };
        let caps = Capabilities {
            found: vec![Backend::Nvenc(probe.clone())],
            missing: vec![],
        };
        assert_eq!(caps.encodable(), vec![Codec::Hevc, Codec::H264]);
        let caps = Capabilities {
            found: vec![Backend::Nvenc(nvenc::Probe {
                hevc: false,
                ..probe
            })],
            missing: vec![],
        };
        assert_eq!(caps.encodable(), vec![Codec::H264]);
    }

    /// The real probe on whatever machine runs the tests (CI: no GPU, no libva
    /// devices; WSL2: NVENC, no /dev/dri). It must report, never fail hard.
    #[test]
    fn real_probe_degrades_gracefully() {
        let caps = Capabilities::detect();
        for b in &caps.found {
            eprintln!("found: {}", b.describe());
        }
        for m in &caps.missing {
            eprintln!("missing: {m}");
        }
        assert_eq!(
            caps.found.len() + caps.missing.len(),
            backends_compiled_in()
        );
        assert!(caps.encodable().contains(&Codec::H264));
    }

    fn backends_compiled_in() -> usize {
        let n = cfg!(feature = "nvenc") as usize + cfg!(feature = "vaapi") as usize;
        n.max(1)
    }

    #[test]
    fn no_backend_means_no_encoder() {
        let f = LinuxCodecs::new();
        assert!(f.caps.set(Capabilities::default()).is_ok());
        let err = f
            .new_encoder(None, EncoderConfig::default())
            .err()
            .map(|e| e.to_string());
        assert_eq!(err.as_deref(), Some("no H264 encoder on this system"));
        assert_eq!(f.host_encodable(), vec![Codec::H264]);
        assert_eq!(f.viewer_decodable(), vec![Codec::H264]);
    }
}
