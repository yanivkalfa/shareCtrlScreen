//! CPU pixel conversions around OpenH264, which only takes and returns planar
//! I420. Colour is **BT.709 limited range** both ways — what `PixelFormat::Nv12`
//! promises and what the SPS's VUI declares, so any decoder (hardware or this
//! one) reproduces the host's colours.
//!
//! Integer arithmetic only, row-pair loops over fixed-size `as_chunks` arrays,
//! no per-pixel branches: the compiler vectorises these, which keeps 1080p
//! conversion at a few milliseconds.

use openh264::formats::YUVSource;

/// BT.709 limited-range RGB→CbCr in Q14 fixed point: (Kb/Kr colour
/// differences) × 224/255. Each row is rounded so it sums to exactly zero —
/// otherwise neutral greys pick up a colour cast.
const U_R: i32 = -1649;
const U_G: i32 = -5547;
const U_B: i32 = 7196;
const V_R: i32 = 7196;
const V_G: i32 = -6537;
const V_B: i32 = -659;
const Q: u32 = 14;
/// Luma Y = 16 + (219/255)·(Kr·R + Kg·G + Kb·B) in Q8, so the whole sum fits
/// 16-bit lanes: 0.2126, 0.7152, 0.0722 × 219/255 × 256 = 46.7, 157.3, 15.9.
/// They sum to 220, so white maps to 16 + 219 = 235 exactly.
const Y8_R: u16 = 47;
const Y8_G: u16 = 157;
const Y8_B: u16 = 16;

/// An I420 picture with tightly packed planes (strides = width, width / 2).
/// Width and height are even: H.264 4:2:0 needs whole chroma samples, and the
/// host only ever configures even sizes.
#[derive(Debug, Default)]
pub struct I420 {
    width: usize,
    height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        debug_assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
        let (cw, ch) = (width / 2, height / 2);
        Self {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// BGRA → I420 for the top-left `dst.width × dst.height` pixels of `src`
/// (`src` may be up to one pixel larger each way: odd screen sizes are cropped
/// to the even size the encoder runs at). Chroma is the 2×2 box average.
pub fn bgra_to_i420(src: &[u8], stride: usize, dst: &mut I420) {
    let (w, h) = (dst.width, dst.height);
    let cw = w / 2;
    // Horizontal bands of whole row pairs, converted in parallel: at 1080p the
    // conversion is memory-bound and a single core cannot saturate the bus.
    // Small pictures are not worth a thread.
    let bands = if w * h >= MIN_PARALLEL_PIXELS {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(MAX_BANDS)
    } else {
        1
    };
    let pair_rows = (h / 2).div_ceil(bands);
    let band = |i: usize| (i * pair_rows * 2, ((i + 1) * pair_rows * 2).min(h));
    let mut failed = Vec::new();
    {
        let mut ys = dst.y[..w * h].chunks_mut(pair_rows * 2 * w);
        let mut us = dst.u.chunks_mut(pair_rows * cw);
        let mut vs = dst.v.chunks_mut(pair_rows * cw);
        std::thread::scope(|s| {
            let mut inline = None;
            for i in 0..bands {
                let (Some(y), Some(u), Some(v)) = (ys.next(), us.next(), vs.next()) else {
                    break;
                };
                let src = &src[band(i).0 * stride..];
                if i == 0 {
                    // The calling thread does a share of the work too.
                    inline = Some((src, y, u, v));
                    continue;
                }
                let spawned = std::thread::Builder::new()
                    .name("sct-bgra-i420".into())
                    .spawn_scoped(s, move || bgra_band(src, stride, w, y, u, v));
                if spawned.is_err() {
                    // Out of threads: finish this band below, on this thread.
                    failed.push(i);
                }
            }
            if let Some((src, y, u, v)) = inline {
                bgra_band(src, stride, w, y, u, v);
            }
        });
    }
    for i in failed {
        let (r0, r1) = band(i);
        bgra_band(
            &src[r0 * stride..],
            stride,
            w,
            &mut dst.y[r0 * w..r1 * w],
            &mut dst.u[r0 / 2 * cw..r1 / 2 * cw],
            &mut dst.v[r0 / 2 * cw..r1 / 2 * cw],
        );
    }
}

/// Below this many pixels (~0.5 MP) conversion takes well under a millisecond.
const MIN_PARALLEL_PIXELS: usize = 512 * 1024;
/// The acceptance target is a 4-core machine; more bands only add overhead.
const MAX_BANDS: usize = 4;

/// Convert `y.len() / w` rows (an even number) starting at `src`.
fn bgra_band(src: &[u8], stride: usize, w: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let cw = w / 2;
    let round = 1 << (Q + 1);
    for (pair, (y2, (u, v))) in y
        .chunks_exact_mut(2 * w)
        .zip(u.chunks_exact_mut(cw).zip(v.chunks_exact_mut(cw)))
        .enumerate()
    {
        let r0 = &src[2 * pair * stride..][..w * 4];
        let r1 = &src[(2 * pair + 1) * stride..][..w * 4];
        let (y0, y1) = y2.split_at_mut(w);
        // Luma in 16-bit lanes (the Q8 sum is at most 255 · 220 + 128 < 2^16),
        // which baseline SSE2/NEON multiply natively, so LLVM vectorises it.
        for (row, out) in [(r0, y0), (r1, y1)] {
            for (px, y) in row.as_chunks::<4>().0.iter().zip(out.iter_mut()) {
                let (b, g, r) = (px[0] as u16, px[1] as u16, px[2] as u16);
                *y = (16 + ((Y8_R * r + Y8_G * g + Y8_B * b + 128) >> 8)) as u8;
            }
        }
        // Chroma: the 2×2 box average, from the two rows just read (in cache).
        for (((a, b), u), v) in r0
            .as_chunks::<8>()
            .0
            .iter()
            .zip(r1.as_chunks::<8>().0)
            .zip(u.iter_mut())
            .zip(v.iter_mut())
        {
            // Sum of four = 4× the average: fold the /4 into the shift.
            let bs = a[0] as i32 + a[4] as i32 + b[0] as i32 + b[4] as i32;
            let gs = a[1] as i32 + a[5] as i32 + b[1] as i32 + b[5] as i32;
            let rs = a[2] as i32 + a[6] as i32 + b[2] as i32 + b[6] as i32;
            *u = (128 + ((U_R * rs + U_G * gs + U_B * bs + round) >> (Q + 2))) as u8;
            *v = (128 + ((V_R * rs + V_G * gs + V_B * bs + round) >> (Q + 2))) as u8;
        }
    }
}

/// NV12 → I420 (deinterleave chroma), top-left `dst` size like [`bgra_to_i420`].
pub fn nv12_to_i420(y: &[u8], y_stride: usize, uv: &[u8], uv_stride: usize, dst: &mut I420) {
    let (w, h) = (dst.width, dst.height);
    let cw = w / 2;
    for row in 0..h {
        dst.y[row * w..][..w].copy_from_slice(&y[row * y_stride..][..w]);
    }
    split_uv(uv, uv_stride, cw, h / 2, &mut dst.u, &mut dst.v);
}

/// Interleaved UV (`cw × ch` samples) → packed U and V planes.
pub fn split_uv(uv: &[u8], uv_stride: usize, cw: usize, ch: usize, u: &mut [u8], v: &mut [u8]) {
    for row in 0..ch {
        let src = &uv[row * uv_stride..][..cw * 2];
        let u = &mut u[row * cw..][..cw];
        let v = &mut v[row * cw..][..cw];
        for ((p, u), v) in src
            .as_chunks::<2>()
            .0
            .iter()
            .zip(u.iter_mut())
            .zip(v.iter_mut())
        {
            *u = p[0];
            *v = p[1];
        }
    }
}

/// Copy a `w × h` 8-bit plane into a packed buffer (stride `w`).
pub fn copy_plane(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut Vec<u8>) {
    dst.resize(w * h, 0);
    for (row, out) in dst.chunks_exact_mut(w).enumerate() {
        out.copy_from_slice(&src[row * stride..][..w]);
    }
}

/// I420 chroma → NV12 interleaved chroma (`cw × ch` samples, packed rows).
pub fn interleave_uv(
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    cw: usize,
    ch: usize,
    dst: &mut Vec<u8>,
) {
    dst.resize(cw * 2 * ch, 128);
    for row in 0..ch {
        let out = &mut dst[row * cw * 2..][..cw * 2];
        let (u, v) = (&u[row * u_stride..][..cw], &v[row * v_stride..][..cw]);
        for ((o, &u), &v) in out.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
            o[0] = u;
            o[1] = v;
        }
    }
}

/// Bilinear resample of a `C`-channel interleaved 8-bit plane into a packed
/// `dw × dh` plane (stride `dw · C`). Pixel-centre aligned, Q8 weights.
///
/// Used only when the host encodes at the viewer's size rather than the screen's
/// (the GPU converter does this on the hardware path). Bilinear is sharp enough
/// for the ≤ 2× reductions that happen in practice; it is not a general
/// high-ratio downscaler.
pub fn scale_bilinear<const C: usize>(
    src: &[u8],
    sw: usize,
    sh: usize,
    s_stride: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
) {
    // Source coordinate of each destination column/row in Q8, clamped so both
    // taps are inside the picture.
    let taps = |d: usize, s: usize| -> Vec<(usize, usize, u32)> {
        (0..d)
            .map(|i| {
                let pos = ((2 * i + 1) * s * 256 / (2 * d)).saturating_sub(128);
                let i0 = (pos >> 8).min(s - 1);
                let i1 = (i0 + 1).min(s - 1);
                (i0, i1, (pos & 0xff) as u32)
            })
            .collect()
    };
    let xs = taps(dw, sw);
    let ys = taps(dh, sh);
    for (row, &(y0, y1, fy)) in ys.iter().enumerate() {
        let a = &src[y0 * s_stride..];
        let b = &src[y1 * s_stride..];
        let out = &mut dst[row * dw * C..][..dw * C];
        for (o, &(x0, x1, fx)) in out.as_chunks_mut::<C>().0.iter_mut().zip(&xs) {
            for c in 0..C {
                let p = |r: &[u8], x: usize| r[x * C + c] as u32;
                let top = p(a, x0) * (256 - fx) + p(a, x1) * fx;
                let bot = p(b, x0) * (256 - fx) + p(b, x1) * fx;
                o[c] = ((top * (256 - fy) + bot * fy + (1 << 15)) >> 16) as u8;
            }
        }
    }
}

/// BT.709 limited-range YCbCr → RGB for one sample (the reference the tests
/// check the fixed-point forward path against).
#[cfg(test)]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let (y, u, v) = (
        (y as f32 - 16.0) * 255.0 / 219.0,
        (u as f32 - 128.0) * 255.0 / 224.0,
        (v as f32 - 128.0) * 255.0 / 224.0,
    );
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    let c = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    (c(r), c(g), c(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.iter().copied().cycle().take(w * h * 4).collect()
    }

    #[test]
    fn primaries_and_greys_round_trip_within_one_step() {
        for (b, g, r) in [
            (0, 0, 0),
            (255, 255, 255),
            (128, 128, 128),
            (0, 0, 255),
            (0, 255, 0),
            (255, 0, 0),
            (30, 160, 220),
        ] {
            let src = solid(4, 2, [b, g, r, 255]);
            let mut dst = I420::new(4, 2);
            bgra_to_i420(&src, 16, &mut dst);
            let (r2, g2, b2) = yuv_to_rgb(dst.y[0], dst.u[0], dst.v[0]);
            for (want, got) in [(r, r2), (g, g2), (b, b2)] {
                assert!(
                    (want as i32 - got as i32).abs() <= 2,
                    "{b},{g},{r} → {b2},{g2},{r2}"
                );
            }
        }
    }

    #[test]
    fn greys_are_exactly_neutral_and_limited_range() {
        let mut dst = I420::new(2, 2);
        bgra_to_i420(&solid(2, 2, [255, 255, 255, 255]), 8, &mut dst);
        assert_eq!((dst.y[0], dst.u[0], dst.v[0]), (235, 128, 128));
        bgra_to_i420(&solid(2, 2, [0, 0, 0, 255]), 8, &mut dst);
        assert_eq!((dst.y[0], dst.u[0], dst.v[0]), (16, 128, 128));
    }

    #[test]
    fn parallel_bands_match_one_band_exactly() {
        // Large enough to be split, with a height that does not divide evenly
        // into four bands and a padded stride.
        let (w, h, stride) = (1280usize, 722usize, 1280 * 4 + 64);
        let src: Vec<u8> = (0..stride * h).map(|i| (i * 7 % 251) as u8).collect();
        let mut banded = I420::new(w, h);
        bgra_to_i420(&src, stride, &mut banded);
        let mut single = I420::new(w, h);
        bgra_band(&src, stride, w, &mut single.y, &mut single.u, &mut single.v);
        assert!(banded.y == single.y && banded.u == single.u && banded.v == single.v);
    }

    #[test]
    fn odd_source_is_cropped_not_overrun() {
        // 3×3 source with padding, encoded at 2×2.
        let src = solid(3, 3, [10, 20, 30, 255]);
        let mut dst = I420::new(2, 2);
        bgra_to_i420(&src, 12, &mut dst);
        assert!(dst.y.iter().all(|&y| y == dst.y[0]));
    }

    #[test]
    fn nv12_and_back_is_lossless() {
        let (w, h) = (4, 2);
        let y: Vec<u8> = (0..8).collect();
        let uv = vec![100, 200, 101, 201];
        let mut i420 = I420::new(w, h);
        nv12_to_i420(&y, w, &uv, w, &mut i420);
        assert_eq!(i420.u, [100, 101]);
        assert_eq!(i420.v, [200, 201]);
        let mut back = Vec::new();
        interleave_uv(&i420.u, 2, &i420.v, 2, 2, 1, &mut back);
        assert_eq!(back, uv);
        assert_eq!(i420.y, y);
    }

    #[test]
    fn bilinear_identity_and_flat_fields() {
        let src: Vec<u8> = (0..16).collect();
        let mut same = vec![0; 16];
        scale_bilinear::<1>(&src, 4, 4, 4, &mut same, 4, 4);
        assert_eq!(same, src);
        let flat = vec![77u8; 8 * 6 * 4];
        let mut half = vec![0; 4 * 3 * 4];
        scale_bilinear::<4>(&flat, 8, 6, 32, &mut half, 4, 3);
        assert!(half.iter().all(|&p| p == 77));
    }
}
