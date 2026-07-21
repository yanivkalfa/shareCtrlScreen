//! Hardware video encode + decode (Plan 04 §5b/§5c). The **only** crate touching
//! Media Foundation / NVENC. Encode ships on the MF HW MFT path (one code path
//! for all vendors, fully permissive, §3); an NVENC-direct path is the runtime
//! optimization when an NVIDIA GPU is present. Decode is D3D11VA (§5c).
//!
//! Codec default is **H.264 4:2:0** (universal HW encode+decode, lowest latency);
//! HEVC/AV1 are negotiated up when both peers support them (§3).
//!
//! The plan's low-latency recipe is the load-bearing part of this module and is
//! implemented concretely in [`encode`]: zero B-frames, CBR, single-slice,
//! effectively-infinite GOP, LTR recovery instead of periodic IDR (§5b). The
//! innermost sample pump and the D3D11VA decoder are structured against the same
//! shared `ID3D11Device` used by capture/render and require on-target-hardware
//! validation (§5b "verify at runtime against target hardware").

#![cfg(windows)]

mod convert;
pub mod decode;
pub mod encode;
mod variant;

pub use decode::{DecodedFrame, Decoder};
pub use encode::{Encoder, EncoderConfig};

/// Negotiated codec (contract §3.2 `caps`; Plan 04 §3). `as_caps_str` matches
/// the strings the viewer advertises (`"H264"`, `"HEVC"`, `"AV1"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn as_caps_str(self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }

    pub fn from_caps_str(s: &str) -> Option<Self> {
        match s {
            "H264" => Some(Codec::H264),
            "HEVC" => Some(Codec::Hevc),
            "AV1" => Some(Codec::Av1),
            _ => None,
        }
    }

    /// Negotiate the best codec both ends can handle. Host prefers, in order,
    /// AV1 (royalty-free) → HEVC (better text/bit) → H.264 (universal fallback),
    /// intersected with what the host can hardware-encode and the viewer can
    /// decode. H.264 is always the safe default (§3 zero-cost posture).
    pub fn negotiate(host_encodes: &[Codec], viewer_decodes: &[Codec]) -> Codec {
        for pref in [Codec::Av1, Codec::Hevc, Codec::H264] {
            if host_encodes.contains(&pref) && viewer_decodes.contains(&pref) {
                return pref;
            }
        }
        Codec::H264
    }
}

/// One encoded access unit handed to the transport for packetization (§5b→§6).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// True for IDR / LTR-mark frames the viewer can start decoding from.
    pub keyframe: bool,
    /// 100-ns MF timestamp (presentation).
    pub timestamp: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("no hardware {0:?} encoder found on this system")]
    NoEncoder(Codec),
    #[error("no hardware {0:?} decoder found on this system")]
    NoDecoder(Codec),
    #[error("encoder produced no output for this input")]
    NeedMoreInput,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_prefers_av1_then_falls_back() {
        assert_eq!(
            Codec::negotiate(&[Codec::H264, Codec::Av1], &[Codec::Av1, Codec::H264]),
            Codec::Av1
        );
        assert_eq!(
            Codec::negotiate(&[Codec::H264, Codec::Hevc], &[Codec::H264]),
            Codec::H264
        );
        // No overlap beyond the guaranteed default.
        assert_eq!(
            Codec::negotiate(&[Codec::H264], &[Codec::H264]),
            Codec::H264
        );
    }
}
