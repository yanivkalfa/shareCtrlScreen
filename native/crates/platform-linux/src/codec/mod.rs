//! Linux codec factory: hardware H.264/HEVC encode through NVENC (NVIDIA) and
//! VA-API (Intel/AMD) (task L3), hardware decode through NVDEC and VA-API
//! (task L4) — every library loaded at run time.
//!
//! Discovery is a real probe, run once on first use and logged; encode and
//! decode are probed separately (a viewer never opens an encoder session):
//! * NVENC — `dlopen` the driver's `libnvidia-encode.so.1` + `libcuda.so.1`,
//!   `NvEncodeAPIGetMaxSupportedVersion`, open a session, list codec GUIDs.
//! * NVDEC — `dlopen` `libnvcuvid.so.1` + `libcuda.so.1`,
//!   `cuvidGetDecoderCaps` for 8-bit 4:2:0 H.264, HEVC and AV1.
//! * VA-API — `dlopen` `libva.so.2` + `libva-drm.so.2`, `vaInitialize` each
//!   `/dev/dri/renderD*` node (libva ≥ 2.20 only, decision #7), then
//!   `vaQueryConfigProfiles` / `vaQueryConfigEntrypoints` for H.264 encode
//!   (`EncSlice[LP]`) or decode (`VLD`).
//!
//! Selection: the NVIDIA engine first when present (for encode, better quality
//! per bit at this latency than VDEnc/VCN and the only HEVC encoder here; for
//! decode, the only HEVC/AV1 decoder here, and direct rather than through a
//! VA-API shim), VA-API next. A backend that probes fine but fails to build a
//! codec (sessions exhausted, size out of range) falls through to the next.
//!
//! The contract says `host_encodable` and `viewer_decodable` always contain
//! H.264 (the protocol's guaranteed fallback), so they do even with no
//! hardware; `new_encoder` / `new_decoder` then return
//! [`CodecError::NoEncoder`] / [`CodecError::NoDecoder`] — the seam where Cisco
//! OpenH264 (task D2, `WithSoftwareH264`) takes over. HEVC and AV1 are
//! advertised for decoding only when the hardware decodes them.

#[cfg(any(feature = "vaapi", feature = "nvenc", feature = "nvdec"))]
#[cfg_attr(not(any(feature = "vaapi", feature = "nvenc")), allow(dead_code))]
mod annexb;
// The encoder half (NVENC) and the decoder half (NVDEC) share the loader; a
// build with only one of them leaves the other half's helpers unused.
#[cfg(any(feature = "nvenc", feature = "nvdec"))]
#[cfg_attr(not(all(feature = "nvenc", feature = "nvdec")), allow(dead_code))]
mod cuda;
#[cfg(all(test, any(feature = "vaapi", feature = "nvdec")))]
mod golden;
// Only VA-API writes headers; NVENC's tests reuse its SPS reader.
#[cfg(any(feature = "vaapi", all(test, feature = "nvenc")))]
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
mod h264;
#[cfg(feature = "vaapi")]
mod h264dec;
#[cfg(any(feature = "vaapi", feature = "nvenc"))]
mod level;
// VA-API copies mapped images, NVDEC writes the planes from the device.
#[cfg(any(feature = "vaapi", feature = "nvdec"))]
#[cfg_attr(not(all(feature = "vaapi", feature = "nvdec")), allow(dead_code))]
mod nv12;
#[cfg(feature = "nvdec")]
mod nvdec;
#[cfg(feature = "nvenc")]
mod nvenc;
#[cfg(feature = "nvenc")]
mod scale;
#[cfg(feature = "vaapi")]
mod vaapi;
#[cfg(any(feature = "vaapi", feature = "nvenc", feature = "nvdec"))]
mod zeroed;

use std::any::Any;
use std::sync::OnceLock;

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

/// Whether a viewer advertises AV1 when NVDEC reports it can decode it.
/// Off until the device check has run: AV1 through NVDEC is implemented
/// but has never decoded a real host stream (no Ampere-or-newer GPU on the
/// test machines). Flip to `true` once plans/checklists/L4.md step 2b
/// passes. While off, a Windows host that encodes AV1 negotiates HEVC or
/// H.264 with this viewer instead.
const ADVERTISE_AV1_DECODE: bool = false;

/// A hardware decoder the probe found.
#[derive(Debug, Clone)]
enum DecodeBackend {
    #[cfg(feature = "nvdec")]
    Nvdec(nvdec::Probe),
    #[cfg(feature = "vaapi")]
    Vaapi(vaapi::DecodeProbe),
}

impl DecodeBackend {
    // With no backend compiled in, `DecodeBackend` has no variants and the
    // arguments are never read.
    #[cfg_attr(
        not(any(feature = "vaapi", feature = "nvdec")),
        allow(unused_variables)
    )]
    fn decodes(&self, codec: Codec) -> bool {
        match *self {
            #[cfg(feature = "nvdec")]
            DecodeBackend::Nvdec(ref p) => p.limits(codec).is_some(),
            // VA-API decoding is H.264 only in this version.
            #[cfg(feature = "vaapi")]
            DecodeBackend::Vaapi(_) => codec == Codec::H264,
        }
    }

    #[cfg_attr(
        not(any(feature = "vaapi", feature = "nvdec")),
        allow(unused_variables)
    )]
    fn create(
        &self,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        match *self {
            #[cfg(feature = "nvdec")]
            DecodeBackend::Nvdec(ref p) => {
                Ok(Box::new(nvdec::NvdecDecoder::new(p, codec, width, height)?))
            }
            #[cfg(feature = "vaapi")]
            DecodeBackend::Vaapi(ref p) => {
                Ok(Box::new(vaapi::VaapiDecoder::new(p, codec, width, height)?))
            }
        }
    }

    fn describe(&self) -> String {
        match *self {
            #[cfg(feature = "nvdec")]
            DecodeBackend::Nvdec(ref p) => p.describe(),
            #[cfg(feature = "vaapi")]
            DecodeBackend::Vaapi(ref p) => p.describe(),
        }
    }
}

/// Decode probe results, in preference order, and why each missing backend
/// is missing.
#[derive(Debug, Default)]
struct DecodeCapabilities {
    found: Vec<DecodeBackend>,
    missing: Vec<String>,
}

impl DecodeCapabilities {
    fn detect() -> Self {
        let mut caps = DecodeCapabilities::default();
        #[cfg(feature = "nvdec")]
        match nvdec::probe() {
            Ok(p) => caps.found.push(DecodeBackend::Nvdec(p)),
            Err(e) => caps.missing.push(format!("NVDEC: {e}")),
        }
        #[cfg(feature = "vaapi")]
        match vaapi::decode_probe() {
            Ok(p) => caps.found.push(DecodeBackend::Vaapi(p)),
            Err(e) => caps.missing.push(format!("VA-API: {e}")),
        }
        if cfg!(not(any(feature = "vaapi", feature = "nvdec"))) {
            caps.missing
                .push("built without hardware decoders (features `vaapi`, `nvdec`)".into());
        }
        caps
    }

    fn log(&self) {
        for b in &self.found {
            tracing::info!("hardware decoder: {}", b.describe());
        }
        for m in &self.missing {
            tracing::info!("hardware decoder unavailable — {m}");
        }
        if self.found.is_empty() {
            tracing::warn!(
                "no hardware video decoder; the viewer decodes H.264 with Cisco OpenH264 \
                 (enable it under Settings → Advanced if it is off)"
            );
        }
    }

    fn decodable(&self) -> Vec<Codec> {
        let mut out: Vec<Codec> = [Codec::Av1, Codec::Hevc]
            .into_iter()
            .filter(|&c| match c {
                Codec::Av1 => ADVERTISE_AV1_DECODE,
                Codec::Hevc | Codec::H264 => true,
            })
            .filter(|&c| self.found.iter().any(|b| b.decodes(c)))
            .collect();
        // Always: the protocol's guaranteed fallback (see the module docs).
        out.push(Codec::H264);
        out
    }
}

/// The Linux [`CodecFactory`].
pub struct LinuxCodecs {
    caps: OnceLock<Capabilities>,
    decode_caps: OnceLock<DecodeCapabilities>,
}

impl LinuxCodecs {
    pub fn new() -> Self {
        Self {
            caps: OnceLock::new(),
            decode_caps: OnceLock::new(),
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

    /// The decode probe, likewise on first use (a host never decodes).
    fn decode_caps(&self) -> &DecodeCapabilities {
        self.decode_caps.get_or_init(|| {
            let caps = DecodeCapabilities::detect();
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
        self.decode_caps().decodable()
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

    /// `device` is unused: v1 decoders hand out CPU NV12 frames, which every
    /// presenter uploads. A presenter that imports DMA-BUFs or CUDA memory
    /// (task L4b) passes its device here for zero-copy.
    fn new_decoder(
        &self,
        _device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        let caps = self.decode_caps();
        let mut last_err = None;
        for backend in caps.found.iter().filter(|b| b.decodes(codec)) {
            match backend.create(codec, width, height) {
                Ok(dec) => return Ok(dec),
                Err(e) => {
                    tracing::warn!("{} could not start: {e}", backend.describe());
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            tracing::warn!(
                "no hardware {} decoder ({})",
                codec.as_caps_str(),
                caps.missing.join("; ")
            );
            CodecError::NoDecoder(codec)
        }))
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
    }

    /// A machine with no usable decoder (no /dev/dri, no NVIDIA driver):
    /// H.264 is still advertised and `new_decoder` says "no decoder", which
    /// is what makes `WithSoftwareH264` engage Cisco OpenH264.
    #[test]
    fn no_backend_means_no_decoder() {
        let f = LinuxCodecs::new();
        assert!(f.decode_caps.set(DecodeCapabilities::default()).is_ok());
        assert_eq!(f.viewer_decodable(), vec![Codec::H264]);
        let err = f
            .new_decoder(None, Codec::H264, 1920, 1080)
            .err()
            .map(|e| e.to_string());
        assert_eq!(err.as_deref(), Some("no H264 decoder on this system"));
        assert!(matches!(
            f.new_decoder(None, Codec::Hevc, 1920, 1080),
            Err(CodecError::NoDecoder(Codec::Hevc))
        ));
    }

    #[cfg(feature = "vaapi")]
    #[test]
    fn vaapi_decodes_h264_only() {
        let probe = vaapi::DecodeProbe {
            node: "/dev/dri/renderD128".into(),
            vendor: "test".into(),
            version: (1, 20),
            profiles: vec![7],
        };
        let caps = DecodeCapabilities {
            found: vec![DecodeBackend::Vaapi(probe)],
            missing: vec![],
        };
        assert_eq!(caps.decodable(), vec![Codec::H264]);
    }

    #[cfg(feature = "nvdec")]
    #[test]
    fn hevc_and_av1_only_when_nvdec_decodes_them() {
        let limits = nvdec::Limits {
            max_width: 4096,
            max_height: 4096,
            max_mb_count: 65536,
            min_width: 48,
            min_height: 16,
        };
        let probe = nvdec::Probe {
            device: "test".into(),
            h264: Some(limits),
            hevc: Some(limits),
            av1: None,
        };
        let caps = DecodeCapabilities {
            found: vec![DecodeBackend::Nvdec(probe.clone())],
            missing: vec![],
        };
        assert_eq!(caps.decodable(), vec![Codec::Hevc, Codec::H264]);
        let caps = DecodeCapabilities {
            found: vec![DecodeBackend::Nvdec(nvdec::Probe {
                av1: Some(limits),
                ..probe
            })],
            missing: vec![],
        };
        // AV1 only once the device check has switched the advertisement on.
        let want = if ADVERTISE_AV1_DECODE {
            vec![Codec::Av1, Codec::Hevc, Codec::H264]
        } else {
            vec![Codec::Hevc, Codec::H264]
        };
        assert_eq!(caps.decodable(), want);
        // The decoder itself still builds when asked (negotiation never asks
        // while the advertisement is off).
        assert!(caps.found[0].decodes(Codec::Av1));
    }

    /// The real decode probe on whatever machine runs the tests (CI: nothing;
    /// WSL2: NVDEC, no /dev/dri). It must report, never fail hard.
    #[test]
    fn real_decode_probe_degrades_gracefully() {
        let caps = DecodeCapabilities::detect();
        for b in &caps.found {
            eprintln!("found: {}", b.describe());
        }
        for m in &caps.missing {
            eprintln!("missing: {m}");
        }
        let compiled = cfg!(feature = "nvdec") as usize + cfg!(feature = "vaapi") as usize;
        assert_eq!(caps.found.len() + caps.missing.len(), compiled.max(1));
        assert!(caps.decodable().contains(&Codec::H264));
    }

    /// A smooth moving BGRA picture (gradients plus a moving block), so the
    /// decoded luma can be compared with the source within coding error.
    #[cfg(any(feature = "vaapi", all(feature = "nvenc", feature = "nvdec")))]
    fn gradient(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let block = (x / 32 + t).is_multiple_of(8) && y / 32 == 2;
                px[i] = (x * 255 / w) as u8;
                px[i + 1] = (y * 255 / h) as u8;
                px[i + 2] = if block {
                    240
                } else {
                    ((x + y) * 255 / (w + h)) as u8
                };
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Mean absolute difference between a decoded frame's luma and the
    /// BT.709 limited-range luma of the BGRA source.
    #[cfg(any(feature = "vaapi", all(feature = "nvenc", feature = "nvdec")))]
    fn luma_error(frame: &platform_api::Frame<'_>, bgra: &[u8]) -> f64 {
        let platform_api::FrameData::Cpu { plane0, .. } = frame.data else {
            panic!("CPU frame expected");
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let mut sum = 0f64;
        for y in 0..h {
            for x in 0..w {
                let p = &bgra[(y * w + x) * 4..];
                let l = 0.2126 * p[2] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[0] as f64;
                let want = 16.0 + l * 219.0 / 255.0;
                sum += (plane0.data[y * plane0.stride + x] as f64 - want).abs();
            }
        }
        sum / (w * h) as f64
    }

    /// NVENC (task L3) → NVDEC on this machine's GPU: every access unit yields
    /// its picture immediately, the pixels match the source within coding
    /// error, a delta before any keyframe yields nothing, HEVC works where
    /// both engines do it, and a new resolution on the same decoder reports
    /// the new size. Skips (passes) where the driver libraries are absent.
    #[cfg(all(feature = "nvenc", feature = "nvdec"))]
    #[test]
    fn nvenc_to_nvdec_round_trip() {
        let (enc_probe, dec_probe) = match (nvenc::probe(), nvdec::probe()) {
            (Ok(e), Ok(d)) => (e, d),
            (Err(e), _) | (_, Err(e)) if e.contains("not loadable") => {
                eprintln!("skipping NVENC → NVDEC round trip: {e}");
                return;
            }
            (Err(e), _) | (_, Err(e)) => panic!("NVIDIA libraries load but a probe failed: {e}"),
        };
        eprintln!("{} / {}", dec_probe.describe(), enc_probe.device);
        let mut codecs = vec![Codec::H264];
        if enc_probe.hevc && dec_probe.hevc.is_some() {
            codecs.push(Codec::Hevc);
        }
        for codec in codecs {
            round_trip(
                codec,
                |cfg| Box::new(nvenc::NvencEncoder::new(cfg).expect("NVENC encoder")),
                |codec, w, h| {
                    Box::new(nvdec::NvdecDecoder::new(&dec_probe, codec, w, h).expect("decoder"))
                },
            );
        }
    }

    /// The encoder → decoder round trip both hardware pairs run: every access
    /// unit yields its picture immediately and within coding error of the
    /// source, a forced IDR is a keyframe, a fresh decoder fed a delta first
    /// yields `Ok(None)`, and a new resolution on the same decoder reports the
    /// new size.
    #[cfg(any(feature = "vaapi", all(feature = "nvenc", feature = "nvdec")))]
    fn round_trip(
        codec: Codec,
        make_enc: impl Fn(EncoderConfig) -> Box<dyn VideoEncoder>,
        make_dec: impl Fn(Codec, u32, u32) -> Box<dyn VideoDecoder>,
    ) {
        let cfg = EncoderConfig {
            width: 640,
            height: 360,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 4_000_000,
            codec,
        };
        let mut enc = make_enc(cfg);
        let mut dec = make_dec(codec, 640, 360);
        let mut first_delta = None;
        for t in 0..12 {
            if t == 6 {
                enc.force_keyframe();
            }
            let src = gradient(640, 360, t);
            let units = enc
                .encode(&platform_api::Frame::bgra(640, 360, &src, 640 * 4))
                .expect("encode");
            assert_eq!(units.len(), 1);
            let u = &units[0];
            assert_eq!(u.keyframe, t == 0 || t == 6, "{codec:?} frame {t}");
            if t == 1 {
                first_delta = Some(u.data.clone());
            }
            let frame = dec
                .decode(&u.data, u.keyframe, t as i64 * 333_333)
                .expect("decode")
                .unwrap_or_else(|| panic!("{codec:?} AU {t} gave no picture"));
            assert_eq!((frame.width, frame.height), (640, 360));
            let err = luma_error(&frame, &src);
            assert!(err < 4.0, "{codec:?} frame {t}: mean luma error {err:.2}");
            assert_eq!(dec.dimensions(), (640, 360));
        }
        // A fresh decoder fed a delta first has nothing to show.
        let mut fresh = make_dec(codec, 640, 360);
        let delta = first_delta.expect("a delta frame");
        assert!(fresh.decode(&delta, false, 0).expect("decode").is_none());

        // Resolution change on the same decoder.
        let mut small = make_enc(EncoderConfig {
            width: 320,
            height: 240,
            ..cfg
        });
        for t in 0..3 {
            let src = gradient(320, 240, t);
            let units = small
                .encode(&platform_api::Frame::bgra(320, 240, &src, 320 * 4))
                .expect("encode");
            let frame = dec
                .decode(&units[0].data, units[0].keyframe, 0)
                .expect("decode after the size change")
                .expect("picture after the size change");
            assert_eq!((frame.width, frame.height), (320, 240));
            assert!(luma_error(&frame, &src) < 4.0);
            assert_eq!(dec.dimensions(), (320, 240));
        }
    }

    /// VA-API encoder (task L3) → VA-API decoder on an Intel/AMD GPU: the
    /// round trip above. Run on the Linux test machine (checklists/L4.md).
    #[cfg(feature = "vaapi")]
    #[test]
    #[ignore = "needs VA-API device"]
    fn vaapi_encode_to_vaapi_decode_round_trip() {
        let enc_probe = vaapi::probe().expect("VA-API encode probe");
        let dec_probe = vaapi::decode_probe().expect("VA-API decode probe");
        eprintln!("{} / {}", enc_probe.describe(), dec_probe.describe());
        round_trip(
            Codec::H264,
            |cfg| Box::new(vaapi::VaapiEncoder::new(&enc_probe, cfg).expect("VA-API encoder")),
            |codec, w, h| {
                Box::new(vaapi::VaapiDecoder::new(&dec_probe, codec, w, h).expect("decoder"))
            },
        );
    }

    /// The L4 acceptance budget: decoding 1080p (decode + read-back into CPU
    /// NV12, i.e. one `decode` call) takes at most 4 ms per frame on average
    /// on an Intel iGPU. 120 frames from the L3 VA-API encoder at 8 Mbit/s;
    /// the first picture (session and surface creation) is not counted.
    #[cfg(feature = "vaapi")]
    #[test]
    #[ignore = "needs VA-API device"]
    fn vaapi_decode_1080p_within_4ms() {
        let enc_probe = vaapi::probe().expect("VA-API encode probe");
        let dec_probe = vaapi::decode_probe().expect("VA-API decode probe");
        let cfg = EncoderConfig {
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            codec: Codec::H264,
        };
        let mut enc = vaapi::VaapiEncoder::new(&enc_probe, cfg).expect("VA-API encoder");
        let mut units = Vec::new();
        for t in 0..120 {
            let src = gradient(1920, 1080, t);
            units.extend(
                enc.encode(&platform_api::Frame::bgra(1920, 1080, &src, 1920 * 4))
                    .expect("encode"),
            );
        }
        let mut dec =
            vaapi::VaapiDecoder::new(&dec_probe, Codec::H264, 1920, 1080).expect("decoder");
        let mut ms = Vec::with_capacity(units.len());
        for (i, u) in units.iter().enumerate() {
            let start = std::time::Instant::now();
            let frame = dec.decode(&u.data, u.keyframe, 0).expect("decode");
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert!(frame.is_some(), "AU {i} gave no picture");
            if i > 0 {
                ms.push(elapsed);
            }
        }
        ms.sort_by(f64::total_cmp);
        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        let p95 = ms[ms.len() * 95 / 100];
        eprintln!(
            "VA-API 1080p decode+read-back over {} frames: mean {mean:.2} ms, p95 {p95:.2} ms, max {:.2} ms",
            ms.len(),
            ms[ms.len() - 1]
        );
        assert!(
            mean <= 4.0,
            "mean {mean:.2} ms/frame exceeds the 4 ms budget"
        );
    }

    /// Writes the golden streams of the encoders on this machine into
    /// `native/testdata/golden` (stream + `.aus` index); FFmpeg then writes
    /// the `.sha256` reference (see `testdata/golden/README.md`). Needs the
    /// NVIDIA driver and network access (Cisco's OpenH264 download).
    #[cfg(all(feature = "nvenc", feature = "nvdec"))]
    #[test]
    #[ignore = "regenerates testdata/golden; run by hand"]
    fn regenerate_golden_streams() {
        use codec_openh264::{OpenH264, SoftwareEncoder};
        use std::fmt::Write as _;

        fn write(name: &str, ext: &str, units: &[platform_api::EncodedUnit]) {
            let dir = golden::dir();
            let mut stream = Vec::new();
            let mut index = String::new();
            for u in units {
                let _ = writeln!(
                    index,
                    "{} {} {}",
                    stream.len(),
                    u.data.len(),
                    u.keyframe as u8
                );
                stream.extend_from_slice(&u.data);
            }
            std::fs::write(dir.join(format!("{name}.{ext}")), &stream).expect("write stream");
            std::fs::write(dir.join(format!("{name}.aus")), index).expect("write index");
            eprintln!(
                "wrote {name}.{ext}: {} AUs, {} bytes",
                units.len(),
                stream.len()
            );
        }

        fn run(
            enc: &mut dyn VideoEncoder,
            w: usize,
            h: usize,
            frames: usize,
        ) -> Vec<platform_api::EncodedUnit> {
            let mut out = Vec::new();
            for t in 0..frames {
                if t == frames / 2 {
                    enc.force_keyframe();
                }
                if t == frames * 3 / 4 {
                    enc.set_bitrate(1_000_000).expect("bitrate");
                }
                let src = gradient(w, h, t);
                out.extend(
                    enc.encode(&platform_api::Frame::bgra(w as u32, h as u32, &src, w * 4))
                        .expect("encode"),
                );
            }
            out
        }

        let cfg = EncoderConfig {
            width: 640,
            height: 360,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 3_000_000,
            codec: Codec::H264,
        };
        let mut enc = nvenc::NvencEncoder::new(cfg).expect("NVENC H.264");
        write(
            "nvenc-h264-main-640x360",
            "h264",
            &run(&mut enc, 640, 360, 30),
        );
        let mut enc = nvenc::NvencEncoder::new(EncoderConfig {
            codec: Codec::Hevc,
            ..cfg
        })
        .expect("NVENC HEVC");
        write(
            "nvenc-hevc-main-640x360",
            "hevc",
            &run(&mut enc, 640, 360, 30),
        );

        let openh264 = OpenH264::new(std::env::temp_dir().join("sct-golden-openh264"));
        let mut enc = SoftwareEncoder::new(
            &openh264,
            EncoderConfig {
                width: 320,
                height: 240,
                ..cfg
            },
            None,
        )
        .expect("OpenH264 (needs network for the Cisco download)");
        write("openh264-cb-320x240", "h264", &run(&mut enc, 320, 240, 30));
    }
}
