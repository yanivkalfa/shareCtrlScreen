//! One `VTDecompressionSession`, decoding synchronously into IOSurface-backed
//! `'420v'` pixel buffers from the session's own `CVPixelBufferPool`.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::{
    kCFBooleanTrue, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMTimeFlags,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    CVImageBuffer, CVPixelBuffer,
};
use objc2_video_toolbox::{
    kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder, VTDecodeFrameFlags,
    VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTSessionCopyProperty, VTSessionSetProperty,
};

use super::status::Failure;

/// `kCMTimeInvalid`: all fields zero (`CMTime.h`). Spelled out so no extern
/// static has to be read; the decoder needs no duration or decode time.
const TIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: CMTimeFlags(0),
    epoch: 0,
};

/// The contract's timestamps are in 100 ns units.
const TIMESCALE_100NS: i32 = 10_000_000;

/// Status reported when a "Create" call returns success but no object —
/// never documented to happen, so it is reported rather than trusted.
const NULL_RESULT: i32 = objc2_core_media::kCMSampleBufferError_RequiredParameterMissing;

/// What the output callback reports for one decoded frame.
struct Delivery {
    /// The `sourceFrameRefCon` of the frame: which `decode` call it answers.
    tag: usize,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: Option<CFRetained<CVPixelBuffer>>,
}

/// Where the output callback leaves its result. A mutex because VideoToolbox
/// may run the callback on one of its own threads even for a synchronous
/// decode; it has returned by the time `decode_frame` does.
struct OutputSlot(Mutex<Option<Delivery>>);

/// `VTDecompressionOutputCallback`.
///
/// # Safety
/// `refcon` is the `OutputSlot` the owning [`Session`] boxed; the session
/// invalidates the VideoToolbox session (which stops all callbacks) before
/// that box is freed.
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: see the function's safety section.
    let slot = unsafe { &*refcon.cast::<OutputSlot>() };
    // SAFETY: VideoToolbox lends the image for the duration of the callback;
    // retaining it keeps it alive (and out of the pool) until we drop it.
    let image = NonNull::new(image).map(|p| unsafe { CFRetained::retain(p) });
    // Never panic across the FFI boundary: a poisoned lock still holds a
    // usable `Option`.
    let mut guard = slot.0.lock().unwrap_or_else(PoisonError::into_inner);
    *guard = Some(Delivery {
        tag: frame_refcon.addr(),
        status,
        flags,
        image,
    });
}

/// A decompression session and the slot its callback writes to.
pub(crate) struct Session {
    session: CFRetained<VTDecompressionSession>,
    /// Boxed: its address is the callback's refcon, so it must not move.
    slot: Box<OutputSlot>,
    /// Tag of the next frame. Starts at 1 so a stray callback with a null
    /// refcon can never match.
    next_tag: usize,
    hardware: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // WHY first: after `VTDecompressionSessionInvalidate` returns, no
        // callback runs any more, so freeing `slot` (dropped after this body)
        // cannot race with `on_output`.
        // SAFETY: the session is live; invalidating twice is not possible as
        // this runs once.
        unsafe { self.session.invalidate() };
    }
}

fn true_value() -> Result<&'static CFBoolean, Failure> {
    // SAFETY: a CoreFoundation constant, valid for the process lifetime.
    unsafe { kCFBooleanTrue }.ok_or(Failure {
        call: "kCFBooleanTrue",
        status: NULL_RESULT,
    })
}

impl Session {
    /// A session for `format`. Hardware decoding is enabled but not required:
    /// VideoToolbox falls back to its own software decoder (always present
    /// for H.264), whose output is the same IOSurface-backed `'420v'` buffer.
    pub(crate) fn new(format: &CMFormatDescription) -> Result<Self, Failure> {
        let yes = true_value()?;
        // SAFETY: VideoToolbox key constants, valid for the process lifetime.
        let spec_key =
            unsafe { kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder };
        let spec = CFDictionary::<CFString, CFBoolean>::from_slices(&[spec_key], &[yes]);

        // Output buffers (`CVPixelBufferCreate` attributes for the session's
        // pool): `'420v'` = NV12 BT.709 limited range, what the presenter's
        // shader converts; IOSurface-backed and Metal-compatible so
        // `CVMetalTextureCache` can wrap each plane without a copy. No width
        // or height: VideoToolbox then emits the stream's own size instead of
        // scaling.
        let format_type = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: CoreVideo key constants, valid for the process lifetime.
        let (k_format, k_metal, k_iosurface) = unsafe {
            (
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferMetalCompatibilityKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let values: [&CFType; 3] = [&format_type, yes, &iosurface];
        let attributes = CFDictionary::<CFString, CFType>::from_slices(
            &[k_format, k_metal, k_iosurface],
            &values,
        );

        let slot = Box::new(OutputSlot(Mutex::new(None)));
        // VideoToolbox copies the record; the refcon must stay valid for the
        // session's lifetime, which `Drop` guarantees.
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_output),
            decompressionOutputRefCon: std::ptr::from_ref::<OutputSlot>(&slot).cast_mut().cast(),
        };
        let mut raw: *mut VTDecompressionSession = std::ptr::null_mut();
        // SAFETY: valid format description and dictionaries (CFString keys,
        // CF values as documented); `record` and `raw` outlive the call.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                format,
                Some(spec.as_opaque()),
                Some(attributes.as_opaque()),
                &record,
                NonNull::from(&mut raw),
            )
        };
        let call = "VTDecompressionSessionCreate";
        if status != 0 {
            return Err(Failure { call, status });
        }
        let raw = NonNull::new(raw).ok_or(Failure {
            call,
            status: NULL_RESULT,
        })?;
        // SAFETY: "Create" returns a +1 reference.
        let session = unsafe { CFRetained::from_raw(raw) };

        // RealTime is the default ("treated as realtime playback"); set it
        // anyway so a decoder that reads it schedules us at its top
        // priority. Not every decoder supports the property, which is fine.
        let value: &CFType = yes;
        // SAFETY: a live session, a VideoToolbox key and a CFBoolean value.
        let status = unsafe {
            VTSessionSetProperty(&session, kVTDecompressionPropertyKey_RealTime, Some(value))
        };
        if status != 0 {
            tracing::debug!("videotoolbox: RealTime not accepted ({status})");
        }

        Ok(Self {
            hardware: uses_hardware(&session),
            session,
            slot,
            next_tag: 1,
        })
    }

    /// True when the running decoder is hardware accelerated (for the log).
    pub(crate) fn hardware(&self) -> bool {
        self.hardware
    }

    /// Whether this session can decode samples of `format` without being
    /// rebuilt (`VTDecompressionSessionCanAcceptFormatDescription`).
    pub(crate) fn can_accept(&self, format: &CMFormatDescription) -> bool {
        // SAFETY: a live session and format description.
        unsafe { self.session.can_accept_format_description(format) }
    }

    /// Decodes one sample synchronously. `Ok(None)`: the decoder produced no
    /// picture for it (dropped, or nothing to show).
    pub(crate) fn decode(
        &mut self,
        sample: &CMSampleBuffer,
    ) -> Result<Option<CFRetained<CVPixelBuffer>>, Failure> {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        *self.slot.0.lock().unwrap_or_else(PoisonError::into_inner) = None;

        let mut info = VTDecodeInfoFlags(0);
        // Flags 0: neither asynchronous decompression nor temporal processing.
        // VTDecodeFrameFlags: "With the kVTDecodeFrame_EnableAsynchronousDecompression
        // bit clear, the video decoder is compelled to emit every frame before
        // it returns", and without temporal processing no frame is held back
        // — so the slot holds this sample's picture when the call returns.
        // That is what lets `VideoDecoder::decode` return the picture of the
        // access unit it was given (no reorder buffer: the encoder never
        // reorders).
        // SAFETY: a live session and sample buffer; the refcon is an opaque
        // tag, never dereferenced; `info` outlives the call.
        let status = unsafe {
            self.session.decode_frame(
                sample,
                VTDecodeFrameFlags(0),
                std::ptr::without_provenance_mut(tag),
                &mut info,
            )
        };
        let delivery = self
            .slot
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if status != 0 {
            return Err(Failure {
                call: "VTDecompressionSessionDecodeFrame",
                status,
            });
        }
        let Some(delivery) = delivery.filter(|d| d.tag == tag) else {
            return Ok(None);
        };
        if delivery.status != 0 {
            return Err(Failure {
                call: "VTDecompressionSession output",
                status: delivery.status,
            });
        }
        // A dropped or interrupted frame has no (or a black) image: no picture.
        let unusable = delivery
            .flags
            .intersects(VTDecodeInfoFlags::FrameDropped | VTDecodeInfoFlags::FrameInterrupted);
        Ok(delivery.image.filter(|_| !unusable))
    }
}

/// `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`.
fn uses_hardware(session: &VTDecompressionSession) -> bool {
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: a live session, a VideoToolbox key, and an out-pointer for a
    // CF object (returned +1).
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            None,
            std::ptr::from_mut(&mut value).cast(),
        )
    };
    if status != 0 {
        return false;
    }
    let Some(raw) = NonNull::new(value.cast_mut()) else {
        return false;
    };
    // SAFETY: "Copy" returns a +1 reference.
    let value = unsafe { CFRetained::from_raw(raw) };
    value
        .downcast_ref::<CFBoolean>()
        .is_some_and(CFBoolean::as_bool)
}

/// Wraps one AVCC / AV1 sample for `decode`. The bytes are copied into a
/// CoreMedia-owned block, so the sample does not borrow the decoder's
/// scratch buffer.
pub(crate) fn sample_buffer(
    format: &CMFormatDescription,
    data: &[u8],
    pts_100ns: i64,
) -> Result<CFRetained<CMSampleBuffer>, Failure> {
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let call = "CMBlockBufferCreateWithMemoryBlock";
    // SAFETY: a null memory block plus `kCMBlockBufferAssureMemoryNowFlag`
    // makes CoreMedia allocate `data.len()` bytes itself; `block` is a valid
    // out-pointer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            data.len(),
            None,
            std::ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    if status != 0 {
        return Err(Failure { call, status });
    }
    let block = NonNull::new(block).ok_or(Failure {
        call,
        status: NULL_RESULT,
    })?;
    // SAFETY: "Create" returns a +1 reference.
    let block = unsafe { CFRetained::from_raw(block) };
    // SAFETY: `data` is a live slice of exactly the block's length.
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(NonNull::from(data).cast(), &block, 0, data.len())
    };
    if status != 0 {
        return Err(Failure {
            call: "CMBlockBufferReplaceDataBytes",
            status,
        });
    }

    let timing = CMSampleTimingInfo {
        duration: TIME_INVALID,
        presentationTimeStamp: CMTime {
            value: pts_100ns,
            timescale: TIMESCALE_100NS,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        },
        decodeTimeStamp: TIME_INVALID,
    };
    let size = data.len();
    let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
    let call = "CMSampleBufferCreateReady";
    // SAFETY: one sample with one timing and one size entry, all pointing at
    // locals that outlive the call; `sample` is a valid out-pointer.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    if status != 0 {
        return Err(Failure { call, status });
    }
    let sample = NonNull::new(sample).ok_or(Failure {
        call,
        status: NULL_RESULT,
    })?;
    // SAFETY: "Create" returns a +1 reference.
    Ok(unsafe { CFRetained::from_raw(sample) })
}
