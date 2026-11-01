//! The concrete type behind `platform_api::FrameData::Native` on macOS. Only
//! code in this crate creates it; the VideoToolbox encoder (plan task M2) and
//! the Metal presenter (M4, `import.rs`) downcast it with
//! `frame_data.downcast_ref::<CvPixelBufferFrame>()`. The VideoToolbox decoder
//! (M3) produces it too.
//!
//! The capture side hands out **no** device (`ScreenCapture::device` is
//! `None`): ScreenCaptureKit frames are IOSurface-backed `CVPixelBuffer`s,
//! which any `VTCompressionSession` reads zero-copy without a shared GPU
//! device — unlike D3D11, where capture and encoder must share one.

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use platform_api::PixelFormat;

/// Plane layout and range of a pixel buffer, as far as the presenter cares
/// (the neutral [`PixelFormat`] has no full-range NV12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// `'420v'`: NV12, BT.709 limited range (capture and decoder output).
    Nv12Limited,
    /// `'420f'`: NV12, full range.
    Nv12Full,
    /// `'BGRA'`: one 32-bit plane.
    Bgra,
}

impl PixelLayout {
    fn from_os_type(format: u32) -> Option<Self> {
        [
            (
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Self::Nv12Limited,
            ),
            (
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                Self::Nv12Full,
            ),
            (kCVPixelFormatType_32BGRA, Self::Bgra),
        ]
        .into_iter()
        .find_map(|(os_type, layout)| (os_type == format).then_some(layout))
    }
}

/// `'420v'` → `"420v"`, for error messages a tester can search for.
pub fn fourcc(v: u32) -> String {
    v.to_be_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

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

    /// Plane layout including the range, or `None` for formats the presenter
    /// does not draw.
    pub fn layout(&self) -> Option<PixelLayout> {
        PixelLayout::from_os_type(self.pixel_format_type())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_renders_os_types() {
        assert_eq!(
            fourcc(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            "420v"
        );
        assert_eq!(fourcc(kCVPixelFormatType_32BGRA), "BGRA");
        assert_eq!(fourcc(0x0000_0001), "????");
    }

    #[test]
    fn known_formats_have_a_layout() {
        assert_eq!(
            PixelLayout::from_os_type(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            Some(PixelLayout::Nv12Limited)
        );
        assert_eq!(
            PixelLayout::from_os_type(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange),
            Some(PixelLayout::Nv12Full)
        );
        assert_eq!(
            PixelLayout::from_os_type(kCVPixelFormatType_32BGRA),
            Some(PixelLayout::Bgra)
        );
        assert_eq!(PixelLayout::from_os_type(0x7934_3230), None);
    }
}
