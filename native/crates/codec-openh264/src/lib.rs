//! Software H.264 for every OS: Cisco's OpenH264 **binary**, downloaded from
//! Cisco at first use and loaded at runtime (plan 04 §5, decision #1).
//!
//! * [`OpenH264`] — the user-controlled binary: pinned per OS/arch, SHA-256
//!   verified, stored under `Paths::config_dir/codecs/`, enable/disable flag.
//! * [`SoftwareEncoder`] / [`SoftwareDecoder`] — `VideoEncoder` / `VideoDecoder`
//!   with the Media Foundation low-latency recipe; CPU BGRA or NV12 in, CPU NV12
//!   out.
//! * [`WithSoftwareH264`] — wraps any hardware `CodecFactory`; hardware stays
//!   preferred, OpenH264 is the guaranteed H.264.
//!
//! ## Licence conditions (these are code, not documentation)
//!
//! Cisco's patent coverage applies only if (BINARY_LICENSE.txt, "AVC/H.264
//! Patent Portfolio License Conditions"):
//! 1. the binary is separately downloaded to the user's device — it is never in
//!    the repository or an installer, and this crate's `openh264` dependencies
//!    have `default-features = false` (their `source` feature would compile the
//!    codec in; `deny.toml` bans the `nasm-rs` build tool it pulls in);
//! 2. the user can enable, disable and re-enable it — [`OpenH264::set_enabled`],
//!    wired to Settings → Advanced, default on;
//! 3. the place where the user controls it shows [`NOTICE`];
//! 4. the full licence text is reproduced where licensing information is shown —
//!    [`LICENSE_TEXT`].
//!
//! ## References
//!
//! * Cisco binary licence: <https://www.openh264.org/BINARY_LICENSE.txt>
//!   (v1.0, copied verbatim into `CISCO_BINARY_LICENSE.txt`), FAQ
//!   <https://www.openh264.org/faq.html>.
//! * Binary host and file names: <https://ciscobinary.openh264.org/>
//!   (`openh264-2.6.0-win64.dll.bz2`, `libopenh264-2.6.0-linux64.8.so.bz2`,
//!   `libopenh264-2.6.0-mac-arm64.dylib.bz2`, …); library hashes cross-checked
//!   against `openh264-sys2` 0.9.9 `src/blobs/hashes.txt`.
//! * Rust bindings: `openh264` / `openh264-sys2` 0.9.9
//!   (<https://docs.rs/openh264/0.9.9>), `DynamicAPI::from_blob_path`,
//!   `encoder::EncoderConfig`, `decoder::DecoderConfig`.
//! * OpenH264 C API (`codec/api/wels/codec_app_def.h`, `codec_api.h`):
//!   `SEncParamExt::{iUsageType = SCREEN_CONTENT_REAL_TIME, iRCMode =
//!   RC_BITRATE_MODE, iMaxQp, uiIntraPeriod, bEnableFrameSkip,
//!   bEnableSceneChangeDetect, bEnableLongTermReference, iComplexityMode,
//!   eSpsPpsIdStrategy = CONSTANT_ID, iMultipleThreadIdc}`; screen-content
//!   parameter rules in `codec/encoder/core/src/encoder_ext.cpp`
//!   (`ParamValidation`) and IDR decisions in `encoder.cpp`
//!   (`DecideFrameType`),
//!   `SSliceArgument::uiSliceMode = SM_SINGLE_SLICE`,
//!   `ISVCEncoder::{ForceIntraFrame, SetOption(ENCODER_OPTION_BITRATE /
//!   ENCODER_OPTION_MAX_BITRATE, SBitrateInfo)}`, `ISVCDecoder::DecodeFrameNoDelay`;
//!   bitrate validation in `codec/encoder/core/src/encoder_ext.cpp`
//!   (`WelsBitRateVerification`).
//! * Colour: ITU-R BT.709-6 (Kr = 0.2126, Kb = 0.0722), 8-bit limited range.

mod binary;
mod convert;
mod decoder;
mod encoder;
mod fallback;

pub use binary::{
    spec_for_this_target, Availability, BinarySpec, OpenH264, OpenH264Error, DOWNLOAD_BASE, VERSION,
};
pub use decoder::SoftwareDecoder;
pub use encoder::{SoftwareEncoder, MAX_QP};
pub use fallback::{FrameReadback, ReadbackFactory, WithSoftwareH264};

/// The attribution Cisco requires wherever the user controls the codec
/// (licence condition 3). Shown verbatim next to the Settings toggle.
pub const NOTICE: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Cisco's `BINARY_LICENSE.txt`, reproduced in full where licensing information
/// is presented to the user (licence condition 4).
pub const LICENSE_TEXT: &str = include_str!("../CISCO_BINARY_LICENSE.txt");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_and_licence_are_the_required_text() {
        assert_eq!(
            NOTICE,
            "OpenH264 Video Codec provided by Cisco Systems, Inc."
        );
        // The licence reproduces the notice and its own conditions.
        assert!(LICENSE_TEXT.contains(&format!("\"{NOTICE}\"")));
        assert!(LICENSE_TEXT.contains("separately downloaded to an end user"));
        assert!(LICENSE_TEXT.contains("enable, disable, or re-enable"));
    }
}
