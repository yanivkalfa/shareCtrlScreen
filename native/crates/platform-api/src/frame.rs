//! OS-neutral geometry and frame types. These replace the `windows` crate's
//! `POINT`/`RECT`/`DXGI_*` types that `capture` exposes today.

use std::any::Any;

/// A point in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// An axis-aligned rectangle in pixels (`x`,`y` = top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// Pixels moved from `src` to `dst` (DXGI move rects; other backends may never
/// produce these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRect {
    pub src: Point,
    pub dst: Rect,
}

/// CPU pixel formats every backend understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// One plane, 4 bytes per pixel, B,G,R,A.
    Bgra8,
    /// Two planes: Y (w×h) and interleaved UV (w×h/2). 8-bit, BT.709 limited range.
    Nv12,
}

/// One CPU plane.
#[derive(Debug, Clone, Copy)]
pub struct CpuPlane<'a> {
    pub data: &'a [u8],
    /// Bytes per row (≥ width × bytes-per-pixel).
    pub stride: usize,
}

/// Where a frame's pixels live.
#[derive(Clone, Copy)]
pub enum FrameData<'a> {
    /// A backend-specific GPU object (D3D11 texture + slice, `CVPixelBuffer`,
    /// DMA-BUF descriptor…). Only the same backend's encoder/presenter can use it;
    /// they downcast with `as_any.downcast_ref::<TheirType>()`.
    Native(&'a dyn Any),
    /// CPU memory. `Bgra8`: `plane0` only. `Nv12`: `plane0` = Y, `plane1` = UV.
    Cpu {
        format: PixelFormat,
        plane0: CpuPlane<'a>,
        plane1: Option<CpuPlane<'a>>,
    },
}

impl std::fmt::Debug for FrameData<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameData::Native(_) => f.write_str("Native(..)"),
            FrameData::Cpu { format, .. } => f.debug_struct("Cpu").field("format", format).finish(),
        }
    }
}

/// A borrowed picture: its size and where its pixels are. Captured frames,
/// encoder inputs, decoder outputs and presenter inputs are all `Frame`s.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: FrameData<'a>,
}

impl<'a> Frame<'a> {
    /// A CPU BGRA frame. Panics in debug builds if `data` is too small.
    pub fn bgra(width: u32, height: u32, data: &'a [u8], stride: usize) -> Self {
        debug_assert!(stride >= width as usize * 4);
        debug_assert!(data.len() >= stride * height as usize);
        Self {
            width,
            height,
            data: FrameData::Cpu {
                format: PixelFormat::Bgra8,
                plane0: CpuPlane { data, stride },
                plane1: None,
            },
        }
    }

    /// A CPU NV12 frame. Panics in debug builds if a plane is too small.
    pub fn nv12(
        width: u32,
        height: u32,
        y: &'a [u8],
        y_stride: usize,
        uv: &'a [u8],
        uv_stride: usize,
    ) -> Self {
        debug_assert!(y.len() >= y_stride * height as usize);
        debug_assert!(uv.len() >= uv_stride * height.div_ceil(2) as usize);
        Self {
            width,
            height,
            data: FrameData::Cpu {
                format: PixelFormat::Nv12,
                plane0: CpuPlane {
                    data: y,
                    stride: y_stride,
                },
                plane1: Some(CpuPlane {
                    data: uv,
                    stride: uv_stride,
                }),
            },
        }
    }
}
