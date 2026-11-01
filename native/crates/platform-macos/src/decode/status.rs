//! `OSStatus` → [`CodecError`] with a message a tester can act on, and the
//! decoder's reaction to each failure.
//!
//! Codes and their meaning: `VTErrors.h` (VideoToolbox), `CMFormatDescription.h`,
//! `CMBlockBuffer.h` and `CMSampleBuffer.h` (CoreMedia).

use objc2_core_media::{
    kCMBlockBufferBadCustomBlockSourceErr, kCMBlockBufferBlockAllocationFailedErr,
    kCMBlockBufferInsufficientSpaceErr, kCMBlockBufferStructureAllocationFailedErr,
    kCMFormatDescriptionError_AllocationFailed, kCMFormatDescriptionError_InvalidParameter,
    kCMSampleBufferError_AllocationFailed, kCMSampleBufferError_Invalidated,
    kCMSampleBufferError_RequiredParameterMissing,
};
use objc2_video_toolbox::{
    kVTAllocationFailedErr, kVTCouldNotCreateInstanceErr, kVTCouldNotFindVideoDecoderErr,
    kVTFormatDescriptionChangeNotSupportedErr, kVTInvalidSessionErr, kVTParameterErr,
    kVTSessionMalfunctionErr, kVTVideoDecoderAuthorizationErr, kVTVideoDecoderBadDataErr,
    kVTVideoDecoderMalfunctionErr, kVTVideoDecoderNeedsRosettaErr,
    kVTVideoDecoderNotAvailableNowErr, kVTVideoDecoderReferenceMissingErr,
    kVTVideoDecoderRemovedErr, kVTVideoDecoderUnknownErr, kVTVideoDecoderUnsupportedDataFormatErr,
};
use platform_api::{Codec, CodecError};

/// What the decoder does after a failed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// The stream is broken until the next keyframe; the session is fine.
    AwaitKeyframe,
    /// The session itself is gone or wedged: drop it and build a new one at
    /// the next keyframe.
    RebuildSession,
}

/// Name, cause/remedy and recovery for a VideoToolbox / CoreMedia status.
// Apple's constant names are matched as they are: renaming them would hide
// which header code each arm handles.
#[allow(non_upper_case_globals)]
pub(crate) fn describe(status: i32) -> (&'static str, &'static str, Recovery) {
    use Recovery::{AwaitKeyframe as Key, RebuildSession as Rebuild};
    match status {
        kVTParameterErr => (
            "kVTParameterErr",
            "VideoToolbox rejected a decoder parameter; please report this with the log",
            Rebuild,
        ),
        kVTInvalidSessionErr => (
            "kVTInvalidSessionErr",
            "the decoder session was invalidated (sleep/wake or a GPU switch); \
             it is rebuilt at the next keyframe",
            Rebuild,
        ),
        kVTAllocationFailedErr => (
            "kVTAllocationFailedErr",
            "out of memory for video frames; close other apps and reconnect",
            Rebuild,
        ),
        kVTCouldNotFindVideoDecoderErr => (
            "kVTCouldNotFindVideoDecoderErr",
            "this Mac has no VideoToolbox decoder for this codec; reconnect so the session \
             negotiates H.264",
            Rebuild,
        ),
        kVTCouldNotCreateInstanceErr => (
            "kVTCouldNotCreateInstanceErr",
            "the decoder could not be started; if this repeats, restart the Mac",
            Rebuild,
        ),
        kVTVideoDecoderBadDataErr => (
            "kVTVideoDecoderBadDataErr",
            "the decoder rejected the host's data (packet loss or a host encoder fault); \
             a new keyframe is requested",
            Key,
        ),
        kVTVideoDecoderUnsupportedDataFormatErr => (
            "kVTVideoDecoderUnsupportedDataFormatErr",
            "this Mac cannot decode the stream's profile or format (for example 4:4:4 or \
             high bit depth); ask the host to use H.264",
            Rebuild,
        ),
        kVTVideoDecoderMalfunctionErr => (
            "kVTVideoDecoderMalfunctionErr",
            "the hardware decoder failed; it is restarted at the next keyframe — if this \
             repeats, restart the app",
            Rebuild,
        ),
        kVTVideoDecoderNotAvailableNowErr => (
            "kVTVideoDecoderNotAvailableNowErr",
            "the decoder is busy or suspended (another app holds every hardware decoder, or \
             the Mac is going to sleep); retried at the next keyframe",
            Rebuild,
        ),
        kVTFormatDescriptionChangeNotSupportedErr => (
            "kVTFormatDescriptionChangeNotSupportedErr",
            "the decoder cannot switch to the stream's new format in place; it is rebuilt",
            Rebuild,
        ),
        kVTVideoDecoderAuthorizationErr => (
            "kVTVideoDecoderAuthorizationErr",
            "macOS refused this app the decoder; reinstall the app from the DMG",
            Rebuild,
        ),
        kVTVideoDecoderRemovedErr => (
            "kVTVideoDecoderRemovedErr",
            "the decoder disappeared (an external GPU was unplugged?); it is rebuilt at the \
             next keyframe",
            Rebuild,
        ),
        kVTSessionMalfunctionErr => (
            "kVTSessionMalfunctionErr",
            "the decoder session failed; it is rebuilt at the next keyframe — if this \
             repeats, restart the app",
            Rebuild,
        ),
        kVTVideoDecoderNeedsRosettaErr => (
            "kVTVideoDecoderNeedsRosettaErr",
            "this decoder needs Rosetta; install it (softwareupdate --install-rosetta) or ask \
             the host to use H.264",
            Rebuild,
        ),
        kVTVideoDecoderReferenceMissingErr => (
            "kVTVideoDecoderReferenceMissingErr",
            "a reference frame was lost (packet loss); a new keyframe is requested",
            Key,
        ),
        kVTVideoDecoderUnknownErr => (
            "kVTVideoDecoderUnknownErr",
            "the decoder failed for an unspecified reason; it is rebuilt at the next keyframe",
            Rebuild,
        ),
        kCMFormatDescriptionError_InvalidParameter => (
            "kCMFormatDescriptionError_InvalidParameter",
            "the host's parameter sets (SPS/PPS/VPS or AV1 sequence header) are invalid; \
             a new keyframe is requested",
            Key,
        ),
        kCMFormatDescriptionError_AllocationFailed
        | kCMBlockBufferStructureAllocationFailedErr
        | kCMBlockBufferBlockAllocationFailedErr
        | kCMSampleBufferError_AllocationFailed => (
            "CoreMedia allocation failure",
            "out of memory for compressed video; close other apps and reconnect",
            Key,
        ),
        kCMBlockBufferInsufficientSpaceErr..=kCMBlockBufferBadCustomBlockSourceErr => (
            "kCMBlockBufferError",
            "a compressed frame could not be wrapped for the decoder; please report this \
             with the log",
            Key,
        ),
        kCMSampleBufferError_Invalidated..=kCMSampleBufferError_RequiredParameterMissing => (
            "kCMSampleBufferError",
            "a compressed frame could not be wrapped for the decoder; please report this \
             with the log",
            Key,
        ),
        _ => (
            "unknown status",
            "unexpected VideoToolbox/CoreMedia failure; please report this with the log",
            Rebuild,
        ),
    }
}

/// A VideoToolbox / CoreMedia call that returned a non-zero status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Failure {
    pub call: &'static str,
    pub status: i32,
}

impl Failure {
    pub(crate) fn recovery(&self) -> Recovery {
        describe(self.status).2
    }

    /// The error the decoder returns, naming the call, the code and what to do.
    pub(crate) fn into_error(self, codec: Codec) -> CodecError {
        let (name, advice, _) = describe(self.status);
        CodecError::Backend(format!(
            "VideoToolbox {codec:?} decoder: {} failed with {} ({name}): {advice}",
            self.call, self.status
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_damage_waits_for_a_keyframe_and_session_loss_rebuilds() {
        assert_eq!(describe(-12909).2, Recovery::AwaitKeyframe);
        assert_eq!(describe(-17694).2, Recovery::AwaitKeyframe);
        assert_eq!(describe(-12903).2, Recovery::RebuildSession);
        assert_eq!(describe(-12911).2, Recovery::RebuildSession);
        assert_eq!(describe(-17691).2, Recovery::RebuildSession);
        // Unknown codes rebuild: a fresh session is the safe default.
        assert_eq!(describe(1234).2, Recovery::RebuildSession);
    }

    #[test]
    fn coremedia_ranges_cover_their_codes() {
        assert_eq!(describe(-12705).0, "kCMBlockBufferError");
        assert_eq!(describe(-12700).0, "CoreMedia allocation failure");
        assert_eq!(describe(-12735).0, "kCMSampleBufferError");
        assert_eq!(describe(-12710).2, Recovery::AwaitKeyframe);
    }

    #[test]
    fn messages_name_the_code_and_what_to_do() {
        let failure = Failure {
            call: "VTDecompressionSessionCreate",
            status: -12906,
        };
        assert_eq!(failure.recovery(), Recovery::RebuildSession);
        let msg = failure.into_error(Codec::Hevc).to_string();
        assert!(msg.contains("-12906"), "{msg}");
        assert!(msg.contains("kVTCouldNotFindVideoDecoderErr"), "{msg}");
        assert!(msg.contains("H.264"), "{msg}");
        assert!(msg.contains("Hevc"), "{msg}");
    }
}
