//! CPU bilinear plane scaler for the NVENC path.
//!
//! WHY it exists: the engine encodes at the size the viewer displays, which is
//! smaller than the captured screen whenever the viewer's window is (see
//! `engine::media::host`, "Encode at the size the viewer actually displays").
//! Windows and VA-API scale on the GPU as part of colour conversion; NVENC has no
//! scaler and v1 feeds it from host memory, so the frame is resampled here
//! before the upload. A CUDA kernel (or the DMA-BUF import path) replaces this
//! when NVENC input moves to the GPU.
//!
//! Fixed-point bilinear with pixel-centre alignment. The engine keeps the source
//! aspect and never upscales, so ratios stay in (½, 1] for every realistic
//! window, where bilinear does not alias.

/// One interleaved 8-bit plane: `channels` bytes per pixel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Plane<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub channels: usize,
}

/// True when `len` bytes hold `rows` rows of `row_bytes`, `stride` apart.
fn fits(len: usize, stride: usize, row_bytes: usize, rows: usize) -> bool {
    stride >= row_bytes && (rows == 0 || stride * (rows - 1) + row_bytes <= len)
}

/// Resample `src` into `dst` (`dst_w × dst_h` pixels, `dst_stride` bytes per
/// row, same channel count). Both geometries are checked first — `Frame`
/// only debug-asserts its plane sizes, so a short plane from a capture
/// backend must be refused here, not read out of bounds.
pub(crate) fn bilinear(
    src: Plane<'_>,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    dst_stride: usize,
) -> Result<(), String> {
    let ch = src.channels;
    if !fits(src.data.len(), src.stride, src.width * ch, src.height) {
        return Err(format!(
            "plane of {} bytes is too small for {}x{} × {ch} bytes at stride {}",
            src.data.len(),
            src.width,
            src.height,
            src.stride
        ));
    }
    if !fits(dst.len(), dst_stride, dst_w * ch, dst_h) {
        return Err(format!(
            "scale target of {} bytes is too small for {dst_w}x{dst_h} × {ch}",
            dst.len()
        ));
    }
    if src.width == 0 || src.height == 0 || dst_w == 0 || dst_h == 0 {
        return Ok(());
    }
    // 16.16 source coordinate of each destination pixel centre:
    // x_src = (x_dst + ½)·(src_w / dst_w) − ½, clamped to the edge pixels.
    let axis = |dst_len: usize, src_len: usize| -> Vec<(usize, usize, u32)> {
        let step = ((src_len as u64) << 16) / dst_len as u64;
        (0..dst_len)
            .map(|i| {
                let pos = ((i as u64 * 2 + 1) * step / 2).saturating_sub(1 << 15);
                let i0 = ((pos >> 16) as usize).min(src_len - 1);
                let i1 = (i0 + 1).min(src_len - 1);
                (i0, i1, (pos & 0xffff) as u32)
            })
            .collect()
    };
    let xs = axis(dst_w, src.width);
    let ys = axis(dst_h, src.height);
    for (y, &(y0, y1, fy)) in ys.iter().enumerate() {
        let r0 = &src.data[y0 * src.stride..];
        let r1 = &src.data[y1 * src.stride..];
        let out = &mut dst[y * dst_stride..y * dst_stride + dst_w * ch];
        for (x, &(x0, x1, fx)) in xs.iter().enumerate() {
            for c in 0..ch {
                let a = r0[x0 * ch + c] as u32;
                let b = r0[x1 * ch + c] as u32;
                let d = r1[x0 * ch + c] as u32;
                let e = r1[x1 * ch + c] as u32;
                // Weights are 16-bit fractions; the products fit in u64 and
                // the +2^31 rounds to nearest.
                let top = a * (0x10000 - fx) + b * fx;
                let bottom = d * (0x10000 - fx) + e * fx;
                let v =
                    (top as u64 * (0x10000 - fy) as u64 + bottom as u64 * fy as u64 + (1 << 31))
                        >> 32;
                out[x * ch + c] = v as u8;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_size_copies() {
        let src: Vec<u8> = (0..4 * 3 * 2).map(|v| v as u8 * 7).collect();
        let mut dst = vec![0u8; src.len()];
        bilinear(
            Plane {
                data: &src,
                width: 3,
                height: 2,
                stride: 12,
                channels: 4,
            },
            &mut dst,
            3,
            2,
            12,
        )
        .expect("scale");
        assert_eq!(dst, src);
    }

    #[test]
    fn short_planes_are_refused_not_read_out_of_bounds() {
        fn plane(data: &[u8], stride: usize) -> Plane<'_> {
            Plane {
                data,
                width: 4,
                height: 3,
                stride,
                channels: 4,
            }
        }
        let mut dst = vec![0u8; 4 * 4 * 3];
        // One byte short of 3 rows of 16 at stride 16.
        assert!(bilinear(plane(&[0; 47], 16), &mut dst, 4, 3, 16).is_err());
        // Stride narrower than a row.
        assert!(bilinear(plane(&[0; 64], 12), &mut dst, 4, 3, 16).is_err());
        // Destination too small.
        assert!(bilinear(plane(&[0; 48], 16), &mut dst[..40], 4, 3, 16).is_err());
        // The last row needs only `row_bytes`, not a full stride.
        assert!(bilinear(plane(&[0; 2 * 20 + 16], 20), &mut dst, 4, 3, 16).is_ok());
    }

    #[test]
    fn halving_averages_two_by_two_blocks() {
        // 4x2 single-channel → 2x1: each output is the mean of a 2x2 block.
        let src = [0u8, 100, 200, 40, 100, 200, 0, 60];
        let mut dst = [0u8; 2];
        bilinear(
            Plane {
                data: &src,
                width: 4,
                height: 2,
                stride: 4,
                channels: 1,
            },
            &mut dst,
            2,
            1,
            2,
        )
        .expect("scale");
        assert_eq!(dst, [100, 75]);
    }

    #[test]
    fn flat_colour_stays_flat_and_strides_are_honoured() {
        // 5x3 BGRA with 4 bytes of row padding, scaled to 3x2 into a padded dst.
        let mut src = vec![0xeeu8; 24 * 3];
        for row in src.chunks_mut(24) {
            for px in row[..20].chunks_mut(4) {
                px.copy_from_slice(&[10, 20, 30, 255]);
            }
        }
        let mut dst = vec![0u8; 16 * 2];
        bilinear(
            Plane {
                data: &src,
                width: 5,
                height: 3,
                stride: 24,
                channels: 4,
            },
            &mut dst,
            3,
            2,
            16,
        )
        .expect("scale");
        for row in dst.chunks(16) {
            for px in row[..12].chunks(4) {
                assert_eq!(px, [10, 20, 30, 255]);
            }
            assert_eq!(&row[12..], &[0, 0, 0, 0], "padding untouched");
        }
    }
}
