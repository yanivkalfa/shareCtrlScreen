//! Video encode / decode. `Codec`, `negotiate`, `EncoderConfig` are copied from
//! the `codec` crate (which is Windows-only); task W1 makes `codec` re-export
//! these and deletes its copies.

use std::any::Any;

use crate::frame::Frame;

/// Negotiated codec (contract §3.2 `caps`). Caps strings: `"H264"`, `"HEVC"`, `"AV1"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Host preference AV1 → HEVC → H.264, intersected with what both sides can
    /// do. H.264 is the guaranteed fallback even without overlap — every backend
    /// must be able to decode it (software fallback if need be).
    pub fn negotiate(host_encodes: &[Codec], viewer_decodes: &[Codec]) -> Codec {
        for pref in [Codec::Av1, Codec::Hevc, Codec::H264] {
            if host_encodes.contains(&pref) && viewer_decodes.contains(&pref) {
                return pref;
            }
        }
        Codec::H264
    }
}

/// Encoder parameters. Bitrate is the live BWE target from the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// MUST equal the rate the capture loop actually feeds. CBR rate control
    /// divides the bitrate by the declared rate; declaring 60 while feeding 30
    /// halved the delivered bits on Windows.
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    pub codec: Codec,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            codec: Codec::H264,
        }
    }
}

/// One encoded access unit in **Annex-B** byte-stream format (start codes).
/// Keyframes carry their parameter sets (SPS/PPS/VPS or the AV1 sequence header)
/// in-band, because the viewer may join on any keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("no {0:?} encoder on this system")]
    NoEncoder(Codec),
    #[error("no {0:?} decoder on this system")]
    NoDecoder(Codec),
    /// Input this encoder cannot take (wrong native frame type, format).
    #[error("unsupported input: {0}")]
    Unsupported(String),
    #[error("codec backend: {0}")]
    Backend(String),
}

/// Low-latency encoder. Recipe every backend reproduces (from the Media
/// Foundation one): CBR at the live bitrate, no B-frames / no reordering,
/// effectively infinite GOP with keyframes only on demand, a max-QP quality
/// floor (32), single slice, speed over quality.
pub trait VideoEncoder {
    /// Encode one frame. May return zero units (encoder warming up); that is not
    /// an error. Units are returned in decode order.
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError>;
    /// Change the target bitrate live.
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError>;
    /// Make the next encoded frame an IDR. Best effort, never fails.
    fn force_keyframe(&mut self);
}

/// Low-latency decoder (no reorder buffer: the encoder never reorders).
pub trait VideoDecoder {
    /// Decode one complete access unit. `Ok(None)` = no picture yet (needs more
    /// input, or a delta before the first keyframe). The returned frame is valid
    /// until the next `decode` call.
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError>;
    /// Current coded-and-cropped picture size; may change mid-stream, so re-query
    /// after each `Some`.
    fn dimensions(&self) -> (u32, u32);
}

/// Per-backend codec discovery and construction.
pub trait CodecFactory: Send + Sync {
    /// What this host can encode. Must always include [`Codec::H264`].
    fn host_encodable(&self) -> Vec<Codec>;
    /// What this viewer can decode. AV1/HEVC only when hardware-decodable;
    /// [`Codec::H264`] always (software fallback).
    fn viewer_decodable(&self) -> Vec<Codec>;
    /// `device` is the capture device (`ScreenCapture::device`) for zero-copy.
    fn new_encoder(
        &self,
        device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError>;
    /// `device` is the presenter device (`Presenter::device`) so decoded frames
    /// are presentable without a copy. `width`/`height` are a hint only.
    fn new_decoder(
        &self,
        device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError>;
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
        assert_eq!(
            Codec::negotiate(&[Codec::H264], &[Codec::H264]),
            Codec::H264
        );
        assert_eq!(Codec::negotiate(&[], &[]), Codec::H264);
    }

    #[test]
    fn caps_strings_roundtrip() {
        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
            assert_eq!(Codec::from_caps_str(c.as_caps_str()), Some(c));
        }
        assert_eq!(Codec::from_caps_str("VP9"), None);
    }
}
