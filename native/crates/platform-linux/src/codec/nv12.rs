//! The CPU NV12 picture a hardware decoder lends out until its next `decode`
//! (`platform_api::VideoDecoder`).
//!
//! Decoded pictures live in GPU surfaces that are padded to whole macroblocks
//! (or the driver's pitch/height alignment) and are reused for the next
//! picture, so every decoder copies the visible rectangle out into this
//! tightly packed buffer: Y rows of `width` bytes, then interleaved UV rows of
//! `width` rounded up to even. That gives the presenter one layout whatever
//! the driver's pitch, and the copy is also what makes the "valid until the
//! next `decode`" promise hold while the driver reuses its surface.

use platform_api::Frame;

/// Where one plane of a mapped source image starts and how far apart its rows
/// are, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourcePlane {
    pub offset: usize,
    pub pitch: usize,
}

/// The visible rectangle of a decoded picture inside its (larger) surface.
/// `x` and `y` are even: 4:2:0 cropping is in units of two luma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visible {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A tightly packed NV12 picture.
#[derive(Debug, Default)]
pub(crate) struct Nv12Picture {
    y: Vec<u8>,
    uv: Vec<u8>,
    width: u32,
    height: u32,
}

impl Nv12Picture {
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per Y row.
    pub(crate) fn y_stride(&self) -> usize {
        self.width as usize
    }

    /// Bytes per UV row: one Cb,Cr pair per two luma columns, rounded up.
    pub(crate) fn uv_stride(&self) -> usize {
        self.width.div_ceil(2) as usize * 2
    }

    pub(crate) fn uv_rows(&self) -> usize {
        self.height.div_ceil(2) as usize
    }

    /// Size the planes for a `width`×`height` picture. Contents are
    /// unspecified until written; the allocation is kept when the size is
    /// unchanged (every frame of a session).
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let y_len = self.y_stride() * height as usize;
        let uv_len = self.uv_stride() * self.uv_rows();
        self.y.resize(y_len, 0);
        self.uv.resize(uv_len, 0);
    }

    /// Mutable planes for a decoder that writes them itself (a device copy).
    pub(crate) fn planes_mut(&mut self) -> (&mut [u8], &mut [u8]) {
        (&mut self.y, &mut self.uv)
    }

    /// Copy the `visible` rectangle out of a mapped NV12 image `src` (the whole
    /// mapping) whose planes start at `y` and `uv`. Every row is bounds-checked
    /// against the mapping first, so a driver that reports a layout larger
    /// than what it mapped yields an error instead of an out-of-bounds read.
    pub(crate) fn copy_from(
        &mut self,
        src: &[u8],
        y: SourcePlane,
        uv: SourcePlane,
        visible: Visible,
    ) -> Result<(), String> {
        if !visible.x.is_multiple_of(2) || !visible.y.is_multiple_of(2) {
            return Err(format!(
                "visible area starts at odd ({}, {}) — not on the 4:2:0 chroma grid",
                visible.x, visible.y
            ));
        }
        self.resize(visible.width, visible.height);
        let (x, top) = (visible.x as usize, visible.y as usize);
        let y_row = self.y_stride();
        let uv_row = self.uv_stride();
        let uv_rows = self.uv_rows();
        check_plane("Y", src.len(), y, x, top, y_row, visible.height as usize)?;
        check_plane("UV", src.len(), uv, x, top / 2, uv_row, uv_rows)?;
        for (r, dst) in self.y.chunks_exact_mut(y_row.max(1)).enumerate() {
            let start = y.offset + (top + r) * y.pitch + x;
            dst.copy_from_slice(&src[start..start + y_row]);
        }
        for (r, dst) in self.uv.chunks_exact_mut(uv_row.max(1)).enumerate() {
            let start = uv.offset + (top / 2 + r) * uv.pitch + x;
            dst.copy_from_slice(&src[start..start + uv_row]);
        }
        Ok(())
    }

    /// The picture as the contract's CPU NV12 frame.
    pub(crate) fn frame(&self) -> Frame<'_> {
        Frame::nv12(
            self.width,
            self.height,
            &self.y,
            self.y_stride(),
            &self.uv,
            self.uv_stride(),
        )
    }
}

/// `rows` rows of `row_bytes` starting at column `x`, row `top` of `plane` fit
/// inside a mapping of `len` bytes, and inside the plane's pitch.
fn check_plane(
    name: &str,
    len: usize,
    plane: SourcePlane,
    x: usize,
    top: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), String> {
    if rows == 0 || row_bytes == 0 {
        return Ok(());
    }
    if x + row_bytes > plane.pitch {
        return Err(format!(
            "{name} plane: columns {x}..{} do not fit its {}-byte pitch",
            x + row_bytes,
            plane.pitch
        ));
    }
    let end = plane.offset + (top + rows - 1) * plane.pitch + x + row_bytes;
    if end > len {
        return Err(format!(
            "{name} plane: rows {top}..{} end at byte {end}, past the {len}-byte mapping",
            top + rows
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::{FrameData, PixelFormat};

    /// A padded 8x6-in-16x8 NV12 surface: Y = row*16+col, UV = 200+row*16+col.
    fn padded_surface() -> (Vec<u8>, SourcePlane, SourcePlane) {
        let pitch = 16;
        let mut buf = vec![0u8; pitch * 8 + pitch * 4];
        for r in 0..8 {
            for c in 0..pitch {
                buf[r * pitch + c] = (r * 16 + c) as u8;
            }
        }
        for r in 0..4 {
            for c in 0..pitch {
                buf[pitch * 8 + r * pitch + c] = (200 + r * 16 + c) as u8;
            }
        }
        (
            buf,
            SourcePlane { offset: 0, pitch },
            SourcePlane {
                offset: pitch * 8,
                pitch,
            },
        )
    }

    #[test]
    fn copies_the_visible_rectangle_and_drops_the_pitch() {
        let (src, y, uv) = padded_surface();
        let mut pic = Nv12Picture::default();
        let visible = Visible {
            x: 2,
            y: 2,
            width: 6,
            height: 4,
        };
        pic.copy_from(&src, y, uv, visible).expect("fits");
        assert_eq!((pic.width(), pic.height()), (6, 4));
        assert_eq!((pic.y_stride(), pic.uv_stride()), (6, 6));
        let f = pic.frame();
        let FrameData::Cpu {
            format,
            plane0,
            plane1: Some(plane1),
        } = f.data
        else {
            panic!("CPU NV12 expected");
        };
        assert_eq!(format, PixelFormat::Nv12);
        // Y row 0 = source row 2, columns 2..8.
        assert_eq!(&plane0.data[..6], &[34, 35, 36, 37, 38, 39]);
        assert_eq!(&plane0.data[18..24], &[82, 83, 84, 85, 86, 87]);
        // UV row 0 = source chroma row 1, columns 2..8.
        assert_eq!(&plane1.data[..6], &[218, 219, 220, 221, 222, 223]);
        assert_eq!(plane1.data.len(), 6 * 2);
    }

    #[test]
    fn odd_width_rounds_the_chroma_row_up() {
        let (src, y, uv) = padded_surface();
        let mut pic = Nv12Picture::default();
        pic.copy_from(
            &src,
            y,
            uv,
            Visible {
                x: 0,
                y: 0,
                width: 7,
                height: 5,
            },
        )
        .expect("fits");
        assert_eq!((pic.y_stride(), pic.uv_stride(), pic.uv_rows()), (7, 8, 3));
        let f = pic.frame();
        assert_eq!((f.width, f.height), (7, 5));
    }

    #[test]
    fn refuses_layouts_past_the_mapping_or_pitch() {
        let (src, y, uv) = padded_surface();
        let mut pic = Nv12Picture::default();
        let too_wide = Visible {
            x: 12,
            y: 0,
            width: 6,
            height: 2,
        };
        assert!(pic.copy_from(&src, y, uv, too_wide).is_err());
        let too_tall = Visible {
            x: 0,
            y: 4,
            width: 8,
            height: 6,
        };
        assert!(pic.copy_from(&src, y, uv, too_tall).is_err());
        let short = &src[..src.len() - 1];
        let all = Visible {
            x: 0,
            y: 0,
            width: 16,
            height: 8,
        };
        assert!(pic.copy_from(short, y, uv, all).is_err());
        assert!(pic.copy_from(&src, y, uv, all).is_ok());
        let odd = Visible {
            x: 1,
            y: 0,
            width: 4,
            height: 2,
        };
        assert!(pic.copy_from(&src, y, uv, odd).is_err());
    }

    #[test]
    fn resize_keeps_the_layout_consistent() {
        let mut pic = Nv12Picture::default();
        pic.resize(1920, 1080);
        let (y, uv) = pic.planes_mut();
        assert_eq!((y.len(), uv.len()), (1920 * 1080, 1920 * 540));
        pic.resize(1366, 767);
        let (y, uv) = pic.planes_mut();
        assert_eq!((y.len(), uv.len()), (1366 * 767, 1366 * 384));
    }
}
