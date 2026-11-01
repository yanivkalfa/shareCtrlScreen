//! H.264 bitstream state for stateless hardware decoders (VA-API).
//!
//! A stateless decode API (VA-API's `VAEntrypointVLD`) decodes the slice data
//! but leaves everything else to the application: parsing SPS/PPS/slice
//! headers, picture order counts, the reference picture buffer and each
//! slice's reference list. This module is that application half, kept free
//! of any driver call so it is tested on real encoder output.
//!
//! * [`bits`] — RBSP extraction and a bounds-checked bit reader.
//! * [`params`] — SPS/PPS, scaling matrices.
//! * [`slice`] — slice headers.
//! * [`refs`] — POC, reference marking, reference lists.
//! * [`stream`] — the per-access-unit state machine tying them together.
//!
//! ## References
//! * ITU-T H.264 (08/2021): §7.3/§7.4 (syntax and semantics), §8.2.1 (picture
//!   order count), §8.2.4 (reference picture lists), §8.2.5 (reference
//!   marking), Tables 7-2…7-4 (scaling lists), 8-13/8-14 (zigzag scans).
//! * FFmpeg `libavcodec/h264_ps.c`, `h264_refs.c`, `h264_slice.c` and
//!   Chromium `media/gpu/h264_decoder.cc` (cross-checked behaviour of the
//!   reference model on streams in the wild).

pub(crate) mod bits;
pub(crate) mod params;
pub(crate) mod refs;
pub(crate) mod slice;
pub(crate) mod stream;

use platform_api::CodecError;

/// Why an access unit cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bad {
    /// The bytes break the syntax or semantics (corrupt or hostile input).
    Malformed(String),
    /// Valid H.264 using a tool outside this decoder's scope.
    Unsupported(String),
}

impl From<Bad> for CodecError {
    fn from(b: Bad) -> Self {
        match b {
            Bad::Malformed(m) => CodecError::Backend(format!("malformed H.264: {m}")),
            Bad::Unsupported(m) => CodecError::Unsupported(format!(
                "H.264 stream uses {m}, which the VA-API decoder does not support"
            )),
        }
    }
}
