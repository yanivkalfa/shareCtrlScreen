//! `CMVideoFormatDescription`s: from H.264/HEVC parameter sets, or from an
//! AV1 sequence header wrapped in an `av1C` atom.

use std::ffi::c_int;
use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMVideoCodecType_AV1,
    CMFormatDescription, CMVideoFormatDescriptionCreate,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, CMVideoFormatDescriptionGetDimensions,
};
use platform_api::Codec;

use super::annexb::LENGTH_SIZE;
use super::av1::SequenceHeader;
use super::status::Failure;

/// Takes ownership of the +1 reference a CoreMedia "Create" call returned.
fn adopt(
    call: &'static str,
    status: i32,
    out: *const CMFormatDescription,
) -> Result<CFRetained<CMFormatDescription>, Failure> {
    if status != 0 {
        return Err(Failure { call, status });
    }
    let raw = NonNull::new(out.cast_mut()).ok_or(Failure {
        call,
        status: objc2_core_media::kCMFormatDescriptionError_InvalidParameter,
    })?;
    // SAFETY: a successful "Create" call returns a +1 reference (CoreFoundation
    // ownership rule), which `CFRetained` releases on drop.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

/// A description for an H.264 or HEVC stream whose samples carry NAL units
/// with [`LENGTH_SIZE`]-byte length prefixes. `sets` is non-empty, in
/// CoreMedia's order (VPS, SPS, PPS), each a NAL unit with its header and
/// emulation-prevention bytes — as CoreMedia requires.
pub(crate) fn from_parameter_sets(
    codec: Codec,
    sets: &[&[u8]],
) -> Result<CFRetained<CMFormatDescription>, Failure> {
    let pointers: Vec<NonNull<u8>> = sets.iter().map(|s| NonNull::from(*s).cast()).collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    let pointers_ptr = NonNull::from(pointers.as_slice()).cast::<NonNull<u8>>();
    let sizes_ptr = NonNull::from(sizes.as_slice()).cast::<usize>();
    let mut out: *const CMFormatDescription = std::ptr::null();
    // WHY 4 fits a c_int: it is a constant of the AVCC format.
    let length_size = LENGTH_SIZE as c_int;
    match codec {
        Codec::H264 => {
            // SAFETY: `pointers`/`sizes` hold `sets.len()` entries describing
            // live slices that outlive the call; CoreMedia only reads them and
            // copies what it keeps. `out` is a valid out-pointer.
            let status = unsafe {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    sets.len(),
                    pointers_ptr,
                    sizes_ptr,
                    length_size,
                    NonNull::from(&mut out),
                )
            };
            adopt(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets",
                status,
                out,
            )
        }
        Codec::Hevc => {
            // SAFETY: as above.
            let status = unsafe {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    sets.len(),
                    pointers_ptr,
                    sizes_ptr,
                    length_size,
                    None,
                    NonNull::from(&mut out),
                )
            };
            adopt(
                "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
                status,
                out,
            )
        }
        Codec::Av1 => Err(Failure {
            call: "CMVideoFormatDescriptionCreateFromParameterSets (AV1 has none)",
            status: objc2_core_media::kCMFormatDescriptionError_InvalidParameter,
        }),
    }
}

/// A description for an AV1 stream: the sequence header's maximum frame size
/// and the `av1C` record as a sample description extension atom — the form
/// an `.mp4` `av01` sample entry has, which is what VideoToolbox's AV1
/// decoder is configured from (Apple Developer Forums thread 722933).
pub(crate) fn av1(
    header: &SequenceHeader,
    config_obu: &[u8],
) -> Result<CFRetained<CMFormatDescription>, Failure> {
    let call = "CMVideoFormatDescriptionCreate (AV1)";
    let too_big = Failure {
        call,
        status: objc2_core_media::kCMFormatDescriptionError_InvalidParameter,
    };
    let width = i32::try_from(header.max_width).map_err(|_| too_big)?;
    let height = i32::try_from(header.max_height).map_err(|_| too_big)?;
    let record = CFData::from_bytes(&header.av1c(config_obu));
    let atom = CFString::from_static_str("av1C");
    let atoms = CFDictionary::<CFString, CFData>::from_slices(&[&*atom], &[&*record]);
    // SAFETY: a CoreMedia constant, valid for the process lifetime.
    let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    let atoms_value: &CFType = &atoms;
    let extensions = CFDictionary::<CFString, CFType>::from_slices(&[key], &[atoms_value]);
    let mut out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: the extensions dictionary has CFString keys and property-list
    // values, which CoreMedia deep-copies; `out` is a valid out-pointer.
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            kCMVideoCodecType_AV1,
            width,
            height,
            Some(extensions.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    adopt(call, status, out)
}

/// The encoded size a description declares, for the log.
pub(crate) fn dimensions(format: &CMFormatDescription) -> (i32, i32) {
    // SAFETY: `format` is a live video format description.
    let d = unsafe { CMVideoFormatDescriptionGetDimensions(format) };
    (d.width, d.height)
}
