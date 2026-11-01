//! The concrete type behind `platform_api::FrameData::Native` on macOS. Only
//! code in this crate creates it; the VideoToolbox encoder (plan task M2)
//! downcasts it with `frame_data.downcast_ref::<CvPixelBufferFrame>()`.
//!
//! The capture side hands out **no** device (`ScreenCapture::device` is
//! `None`): ScreenCaptureKit frames are IOSurface-backed `CVPixelBuffer`s,
//! which any `VTCompressionSession` reads zero-copy without a shared GPU
//! device — unlike D3D11, where capture and encoder must share one.

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use platform_api::PixelFormat;

/// One ScreenCaptureKit frame: an IOSurface-backed `CVPixelBuffer` from the
/// stream's surface pool.
///
/// Holding it keeps that surface out of the pool. Every owner must drop it as
/// soon as it is done with it — ScreenCaptureKit stalls the stream once it has
/// no free surface left (WWDC22 "Take ScreenCaptureKit to the next level").
/// VideoToolbox retains the buffer itself for as long as it encodes from it, so
/// the encoder must not keep extra references.
pub struct CvPixelBufferFrame {
    buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: a CVPixelBuffer is a CoreFoundation object whose retain/release are
// atomic, and ScreenCaptureKit never writes to a surface it has delivered
// until the last reference is released. Moving the reference to the media
// thread (or reading it from VideoToolbox's threads) is how Apple's own
// capture → encode pipelines use it; Swift declares CVBuffer `Sendable`.
unsafe impl Send for CvPixelBufferFrame {}

impl CvPixelBufferFrame {
    pub(crate) fn new(buffer: CFRetained<CVPixelBuffer>) -> Self {
        Self { buffer }
    }

    /// The buffer to feed `VTCompressionSessionEncodeFrame`.
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    /// Picture size in pixels.
    pub fn size(&self) -> (u32, u32) {
        let w = CVPixelBufferGetWidth(&self.buffer);
        let h = CVPixelBufferGetHeight(&self.buffer);
        (
            u32::try_from(w).unwrap_or(u32::MAX),
            u32::try_from(h).unwrap_or(u32::MAX),
        )
    }

    /// The CoreVideo pixel format (`'420v'` as configured by the capture, or
    /// `'BGRA'`).
    pub fn pixel_format_type(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.buffer)
    }

    /// The neutral layout of the planes, if it is one `platform-api` names:
    /// `'420v'` = NV12 (BT.709 limited range, as the capture configures the
    /// colour matrix), `'BGRA'` = Bgra8.
    pub fn format(&self) -> Option<PixelFormat> {
        let t = self.pixel_format_type();
        if t == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            Some(PixelFormat::Nv12)
        } else if t == kCVPixelFormatType_32BGRA {
            Some(PixelFormat::Bgra8)
        } else {
            None
        }
    }
}
