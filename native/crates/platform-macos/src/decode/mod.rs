//! `VideoDecoder` on VideoToolbox (plan task M3).
//!
//! Annex-B access units (H.264, HEVC) or AV1 temporal units come in; their
//! parameter sets / sequence header become a `CMVideoFormatDescription`, the
//! rest one length-prefixed `CMSampleBuffer`; a synchronous
//! `VTDecompressionSession` turns that into an IOSurface-backed `'420v'`
//! `CVPixelBuffer`, handed out as [`CvPixelBufferFrame`] — the type the Metal
//! presenter imports without a copy.
//!
//! Stream changes: a keyframe with different parameter sets (the host
//! follows the viewer's window size, e.g. 1080p → 720p) gets a new format
//! description; the session keeps going when it can accept it, and is
//! rebuilt otherwise. The picture size is always the decoded buffer's.
//!
//! Failures never wedge the stream: after any decode error the decoder waits
//! for the next keyframe (deltas give `Ok(None)`, which makes the viewer ask
//! for one), and errors that mean the session itself is gone (sleep/wake, GPU
//! change, decoder crash) also drop the session so the keyframe rebuilds it.
//!
//! ## References
//! * `VTDecompressionSessionCreate`, `VTDecompressionSessionDecodeFrame`,
//!   `VTDecompressionSessionCanAcceptFormatDescription`,
//!   `VTDecompressionSessionInvalidate`:
//!   <https://developer.apple.com/documentation/videotoolbox/vtdecompressionsession>
//! * `VTDecompressionSessionDecodeFrame` ("If both flags are clear, the
//!   decompression completes and your output callback function will be called
//!   before VTDecompressionSessionDecodeFrame returns"):
//!   <https://developer.apple.com/documentation/videotoolbox/vtdecompressionsessiondecodeframe(_:samplebuffer:flags:framerefcon:infoflagsout:)>
//! * `VTDecodeFrameFlags`, `VTDecodeInfoFlags`:
//!   <https://developer.apple.com/documentation/videotoolbox/vtdecodeframeflags>,
//!   <https://developer.apple.com/documentation/videotoolbox/vtdecodeinfoflags>
//! * `VTRegisterSupplementalVideoDecoderIfAvailable` ("Call this function to
//!   find and register video decoders that aren't registered by default."):
//!   <https://developer.apple.com/documentation/videotoolbox/vtregistersupplementalvideodecoderifavailable(_:)>
//! * `kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder`,
//!   `kVTDecompressionPropertyKey_RealTime`,
//!   `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`:
//!   <https://developer.apple.com/documentation/videotoolbox/decompression-properties>
//! * `VTIsHardwareDecodeSupported` (the test gate for hardware decode; the
//!   macOS CI step runs the golden-stream test only where it is true):
//!   <https://developer.apple.com/documentation/videotoolbox/vtishardwaredecodesupported(_:)>
//! * VideoToolbox result codes (`VTErrors.h`):
//!   <https://developer.apple.com/documentation/videotoolbox/videotoolbox-constants>
//! * `CMVideoFormatDescriptionCreateFromH264ParameterSets` /
//!   `…FromHEVCParameterSets` ("parameter sets' data can come from raw NAL
//!   units and must have any emulation prevention bytes needed"),
//!   `CMVideoFormatDescriptionCreate`,
//!   `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`:
//!   <https://developer.apple.com/documentation/coremedia/cmvideoformatdescription>
//! * `CMBlockBufferCreateWithMemoryBlock`, `CMBlockBufferReplaceDataBytes`,
//!   `CMSampleBufferCreateReady`:
//!   <https://developer.apple.com/documentation/coremedia/cmblockbuffer-api>,
//!   <https://developer.apple.com/documentation/coremedia/cmsamplebuffer-api>
//! * `kCVPixelBufferPixelFormatTypeKey`, `kCVPixelBufferMetalCompatibilityKey`,
//!   `kCVPixelBufferIOSurfacePropertiesKey`:
//!   <https://developer.apple.com/documentation/corevideo/pixel-buffer-attribute-keys>
//! * Technical Q&A / WWDC14 513 "Direct Access to Video Encoding and
//!   Decoding" (Annex-B → AVCC, parameter sets out of band, rebuilding on a
//!   format change): <https://developer.apple.com/videos/play/wwdc2014/513/>
//! * ISO/IEC 14496-15 (AVCC/HVCC sample format), ITU-T H.264 / H.265 Table
//!   7-1 (NAL unit types), AV1 bitstream spec §5.3, §5.5, §5.9.2 (OBUs,
//!   sequence header, frame header), AV1 Codec ISO Media File Format
//!   Binding §2.3–§2.4 (`av1C`, samples):
//!   <https://aomediacodec.github.io/av1-spec/>, <https://aomediacodec.github.io/av1-isobmff/>
//! * AV1 needs `av1C` in the format description:
//!   <https://developer.apple.com/forums/thread/722933>
//! * plans/01-MACOS-FINDINGS.md §4.

mod annexb;
mod av1;
mod format;
mod session;
mod status;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod synth;
#[cfg(test)]
mod tests;

use std::sync::Once;

use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription,
    CMVideoCodecType,
};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_video_toolbox::{
    VTIsHardwareDecodeSupported, VTRegisterSupplementalVideoDecoderIfAvailable,
};
use platform_api::{Codec, CodecError, Frame, FrameData, VideoDecoder};

use crate::gpu::{fourcc, CvPixelBufferFrame};
use annexb::ParameterSets;
use av1::SequenceHeader;
use session::Session;
use status::{Failure, Recovery};

fn codec_type(codec: Codec) -> CMVideoCodecType {
    match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Av1 => kCMVideoCodecType_AV1,
    }
}

/// True when this Mac decodes `codec` in hardware. AV1: M3-generation Apple
/// silicon and later; HEVC: every supported Mac with a T2 or Apple silicon,
/// most Intel ones.
pub(crate) fn hardware_decodes(codec: Codec) -> bool {
    register_supplemental(codec);
    // SAFETY: a pure capability query with a valid codec type.
    unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) }
}

/// Asks VideoToolbox to register the system's AV1 decoder, once per process,
/// before anything queries or opens one. WHY: Apple ships some decoders as
/// supplemental ones that are not registered by default; an unregistered
/// decoder makes the capability query say "no" and session creation fail
/// with `kVTCouldNotFindVideoDecoderErr`. Where AV1 is already registered,
/// or absent, the call does nothing. H.264 and HEVC are always registered.
fn register_supplemental(codec: Codec) {
    static AV1: Once = Once::new();
    if codec == Codec::Av1 {
        // SAFETY: takes a codec type by value and touches no caller memory.
        AV1.call_once(|| unsafe {
            VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_AV1);
        });
    }
}

/// The probe's H.264 sequence parameter set: Constrained Baseline
/// (profile_idc 66, constraint_set0/1), level 4.0, 120 × 68 macroblocks
/// cropped by 8 rows to 1920 × 1080, POC type 2, one reference frame, no VUI
/// (ITU-T H.264 §7.3.2.1.1). NAL header included; it contains no
/// `00 00 0x` run, so no emulation-prevention byte is needed.
const PROBE_SPS: [u8; 10] = [0x67, 0x42, 0xc0, 0x28, 0xda, 0x01, 0xe0, 0x08, 0x9f, 0x95];

/// The probe's picture parameter set: ids 0, CAVLC, one slice group, QP 26,
/// deblocking control present (ITU-T H.264 §7.3.2.2).
const PROBE_PPS: [u8; 4] = [0x68, 0xce, 0x3c, 0x80];

/// Proves that VideoToolbox opens an H.264 decoder session on this Mac by
/// creating one for a 1080p Baseline stream and dropping it.
///
/// WHY: [`VtDecoder`] builds its session only at the first keyframe, so
/// without this a Mac whose VideoToolbox refuses H.264 would accept the
/// decoder and then fail every keyframe. Refusing here instead is what lets
/// `WithSoftwareH264` fall back to Cisco OpenH264 for decoding.
pub(crate) fn probe_h264() -> Result<(), CodecError> {
    let fail = |failure: Failure| failure.into_error(Codec::H264);
    let format =
        format::from_parameter_sets(Codec::H264, &[&PROBE_SPS, &PROBE_PPS]).map_err(fail)?;
    let session = Session::new(&format).map_err(fail)?;
    tracing::debug!(
        "videotoolbox: H264 decoder probe ok ({})",
        if session.hardware() {
            "hardware"
        } else {
            "software"
        }
    );
    // Dropping the session invalidates it; the real one is built at the
    // stream's first keyframe from the host's parameter sets.
    Ok(())
}

/// What the viewer advertises: AV1 and HEVC only where the hardware decodes
/// them (software HEVC/AV1 would cost more than the bandwidth saves);
/// H.264 always — VideoToolbox decodes it in software where hardware lacks.
pub(crate) fn decodable() -> Vec<Codec> {
    let mut codecs: Vec<Codec> = [Codec::Av1, Codec::Hevc]
        .into_iter()
        .filter(|&c| hardware_decodes(c))
        .collect();
    codecs.push(Codec::H264);
    codecs
}

/// Format state of the stream: what the current format description was
/// built from.
enum StreamFormat {
    Nal(ParameterSets),
    /// The sequence header in force and its OBU (size field set).
    Av1(Option<(SequenceHeader, Vec<u8>)>),
}

pub(crate) struct VtDecoder {
    codec: Codec,
    stream: StreamFormat,
    format: Option<CFRetained<CMFormatDescription>>,
    session: Option<Session>,
    /// Deltas are dropped until a keyframe decodes: at the start, and after
    /// any failure (its references may be gone).
    awaiting_keyframe: bool,
    dims: (u32, u32),
    /// Scratch for the sample being built (reused, no per-frame allocation).
    sample: Vec<u8>,
    /// The picture lent out by the last `decode`.
    last: Option<CvPixelBufferFrame>,
}

impl VtDecoder {
    /// `width`/`height` are only what `dimensions` reports before the first
    /// picture. No VideoToolbox object exists until the first keyframe brings
    /// the parameter sets.
    pub(crate) fn new(codec: Codec, width: u32, height: u32) -> Self {
        let stream = match codec {
            Codec::H264 | Codec::Hevc => StreamFormat::Nal(ParameterSets::new(codec)),
            Codec::Av1 => StreamFormat::Av1(None),
        };
        Self {
            codec,
            stream,
            format: None,
            session: None,
            awaiting_keyframe: true,
            dims: (width, height),
            sample: Vec::new(),
            last: None,
        }
    }

    /// Splits `au` into `self.sample`, takes its parameter sets / sequence
    /// header into the format, and says whether it is a random-access point.
    fn ingest(&mut self, au: &[u8]) -> Result<bool, CodecError> {
        let codec = self.codec;
        let (random_access, new_format) = match &mut self.stream {
            StreamFormat::Nal(sets) => {
                let split = annexb::split_into_avcc(codec, au, &mut self.sample)?;
                let changed = sets.absorb(&split.parameter_sets);
                let format = if changed && sets.complete() {
                    match format::from_parameter_sets(codec, &sets.all()) {
                        Ok(f) => Some(f),
                        Err(failure) => {
                            // Forget them, so the next keyframe's identical
                            // copy counts as new and is tried again.
                            *sets = ParameterSets::new(codec);
                            return Err(self.fail(failure));
                        }
                    }
                } else {
                    None
                };
                (split.random_access, format)
            }
            StreamFormat::Av1(current) => {
                let in_force = current.as_ref().map(|(h, _)| h);
                let split = av1::split_into_sample(au, in_force, &mut self.sample)?;
                let mut format = None;
                if let Some((header, obu)) = split.sequence_header {
                    if current.as_ref().map(|(_, o)| o) != Some(&obu) {
                        match format::av1(&header, &obu) {
                            Ok(f) => {
                                format = Some(f);
                                *current = Some((header, obu));
                            }
                            Err(failure) => {
                                *current = None;
                                return Err(self.fail(failure));
                            }
                        }
                    }
                }
                (split.random_access, format)
            }
        };
        if let Some(format) = new_format {
            self.change_format(format);
        }
        Ok(random_access)
    }

    /// Switches to a new format description. The session survives when it
    /// can decode the new format in place; otherwise it is dropped and the
    /// keyframe that carried the change builds a new one — nothing is lost,
    /// because a format change always arrives with a keyframe.
    fn change_format(&mut self, format: CFRetained<CMFormatDescription>) {
        let (w, h) = format::dimensions(&format);
        let kept = self.session.as_ref().is_some_and(|s| s.can_accept(&format));
        if !kept {
            self.session = None;
        }
        tracing::info!(
            "videotoolbox: {:?} stream format {w}x{h} ({})",
            self.codec,
            if kept { "session kept" } else { "new session" }
        );
        self.format = Some(format);
    }

    /// Applies a failure's recovery and turns it into the contract's error.
    fn fail(&mut self, failure: Failure) -> CodecError {
        self.awaiting_keyframe = true;
        if failure.recovery() == Recovery::RebuildSession {
            self.session = None;
        }
        failure.into_error(self.codec)
    }

    /// The session for `format`, built on first use.
    fn session(&mut self, format: &CMFormatDescription) -> Result<&mut Session, Failure> {
        let session = match self.session.take() {
            Some(session) => session,
            None => {
                register_supplemental(self.codec);
                let session = Session::new(format)?;
                tracing::info!(
                    "videotoolbox: {:?} decoder session ready ({})",
                    self.codec,
                    if session.hardware() {
                        "hardware"
                    } else {
                        "software"
                    }
                );
                session
            }
        };
        Ok(self.session.insert(session))
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        // The previous picture goes back to the session's pool now; the
        // contract only lends it until this call.
        self.last = None;
        let random_access = match self.ingest(au) {
            Ok(r) => r,
            Err(e) => {
                self.awaiting_keyframe = true;
                return Err(e);
            }
        };
        // The transport's flag, or what the bitstream says: either is a
        // valid entry point (the encoder never reorders).
        let keyframe = keyframe || random_access;
        if self.awaiting_keyframe && !keyframe {
            return Ok(None);
        }
        let Some(format) = self.format.clone() else {
            if keyframe {
                self.awaiting_keyframe = true;
                return Err(CodecError::Backend(format!(
                    "VideoToolbox {:?} decoder: a keyframe arrived without its parameter sets \
                     (SPS/PPS/VPS or AV1 sequence header), so it cannot be decoded; the host \
                     encoder must repeat them on every keyframe",
                    self.codec
                )));
            }
            return Ok(None);
        };
        if self.sample.is_empty() {
            // Parameter sets only: no picture in this unit.
            return Ok(None);
        }
        if self.session.is_none() && !keyframe {
            // A rebuilt session can only start at a keyframe.
            self.awaiting_keyframe = true;
            return Ok(None);
        }

        let decoded = session::sample_buffer(&format, &self.sample, pts_100ns)
            .and_then(|sample| self.session(&format)?.decode(&sample));
        let buffer = match decoded {
            Ok(Some(buffer)) => buffer,
            Ok(None) => {
                // Accepted without a picture: references are intact.
                if keyframe {
                    self.awaiting_keyframe = false;
                }
                return Ok(None);
            }
            Err(failure) => return Err(self.fail(failure)),
        };
        if keyframe {
            self.awaiting_keyframe = false;
        }

        let frame = CvPixelBufferFrame::new(buffer);
        if frame.pixel_format_type() != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            // The session was asked for '420v'; anything else would be drawn
            // wrongly by the presenter, so it is refused here, loudly. Deltas
            // then wait for the next keyframe (giving `Ok(None)`) instead of
            // each repeating this error.
            self.awaiting_keyframe = true;
            return Err(CodecError::Backend(format!(
                "VideoToolbox {:?} decoder returned '{}' pixels instead of '420v'; \
                 please report this with the log",
                self.codec,
                fourcc(frame.pixel_format_type())
            )));
        }
        // The decoded buffer's size is the coded-and-cropped picture size;
        // it follows format changes by construction.
        self.dims = frame.size();
        let (width, height) = self.dims;
        let frame: &CvPixelBufferFrame = self.last.insert(frame);
        Ok(Some(Frame {
            width,
            height,
            data: FrameData::Native(frame),
        }))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.dims
    }
}
