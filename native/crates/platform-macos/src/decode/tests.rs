//! The decoder on real VideoToolbox. Everything except the encoder
//! round trip runs on any Mac, including GitHub's macOS runners (H.264 has a
//! software decoder where hardware is missing; no window, no permissions):
//! `cargo test -p platform-macos decode::`.
//!
//! The streams come from [`H264Synth`]: lossless I_PCM keyframes and
//! skip-only deltas, so every decoded sample is checked exactly.

use std::ptr::NonNull;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess, CVPixelBuffer,
    CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use platform_api::{Codec, CodecFactory, EncoderConfig, Frame, FrameData, VideoDecoder};
use sha2::{Digest, Sha256};

use super::golden;
use super::synth::{pattern, H264Synth};
use super::*;
use crate::hardware::skip;

/// A decoded picture copied out of its pixel buffer (tight rows).
struct Picture {
    width: u32,
    height: u32,
    y: Vec<u8>,
    /// Interleaved Cb, Cr; `width` bytes per row, `height / 2` rows.
    uv: Vec<u8>,
}

/// Copies both planes of a `'420v'` buffer.
fn read_planes(frame: &CvPixelBufferFrame) -> Picture {
    let (width, height) = frame.size();
    let buffer = frame.pixel_buffer();
    let (w, h) = (width as usize, height as usize);
    let mut y = Vec::with_capacity(w * h);
    let mut uv = Vec::with_capacity(w * h / 2);
    // SAFETY: lock/unlock bracket the CPU reads; each row read lies within
    // the plane's reported bytes-per-row × rows.
    unsafe {
        assert_eq!(
            CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
            kCVReturnSuccess
        );
        for (plane, rows, out) in [(0usize, h, &mut y), (1, h / 2, &mut uv)] {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast::<u8>();
            assert!(!base.is_null(), "plane {plane} has no CPU address");
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
            for row in 0..rows {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), w));
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
    }
    Picture {
        width,
        height,
        y,
        uv,
    }
}

/// The `CvPixelBufferFrame` behind a decoded frame — the presenter's type.
fn native<'a>(frame: &'a Frame<'a>) -> &'a CvPixelBufferFrame {
    let FrameData::Native(any) = frame.data else {
        panic!("decoder output must be a GPU frame, got {:?}", frame.data);
    };
    any.downcast_ref::<CvPixelBufferFrame>()
        .unwrap_or_else(|| panic!("decoder output must be a CvPixelBufferFrame"))
}

/// Decodes `au`; the picture if one came out (checked against
/// `dimensions()` and the frame's own size).
fn decode(dec: &mut VtDecoder, au: &[u8], keyframe: bool, n: i64) -> Option<Picture> {
    let picture = match dec.decode(au, keyframe, n * 166_667) {
        Ok(Some(frame)) => {
            let picture = read_planes(native(&frame));
            assert_eq!((frame.width, frame.height), (picture.width, picture.height));
            Some(picture)
        }
        Ok(None) => None,
        Err(e) => panic!("decode #{n}: {e}"),
    };
    if let Some(p) = &picture {
        assert_eq!(dec.dimensions(), (p.width, p.height));
    }
    picture
}

/// Every visible sample equals the synthesized source.
fn assert_exact(synth: &H264Synth, p: &Picture) {
    let w = p.width as usize;
    for y in 0..p.height {
        for x in 0..p.width {
            let got = p.y[y as usize * w + x as usize];
            assert_eq!(got, synth.luma(x, y), "luma at ({x},{y})");
        }
    }
    for cy in 0..p.height / 2 {
        for cx in 0..p.width / 2 {
            let i = cy as usize * w + 2 * cx as usize;
            assert_eq!(
                (p.uv[i], p.uv[i + 1]),
                synth.chroma(cx, cy),
                "chroma at ({cx},{cy})"
            );
        }
    }
}

#[test]
fn pcm_keyframe_and_skipped_deltas_decode_bit_exact() {
    let mut synth = H264Synth::new(320, 240, 3);
    let mut dec = VtDecoder::new(Codec::H264, 0, 0);
    let key = synth.keyframe();
    let first = decode(&mut dec, &key, true, 0)
        .unwrap_or_else(|| panic!("a keyframe must give a picture (no reorder delay)"));
    assert_eq!((first.width, first.height), (320, 240));
    assert_exact(&synth, &first);
    for n in 1..=5 {
        let p = decode(&mut dec, &synth.repeat(), false, n)
            .unwrap_or_else(|| panic!("delta #{n} must give a picture"));
        assert_exact(&synth, &p);
    }
}

#[test]
fn h264_probe_describes_1080p_and_opens_a_session() {
    let format = format::from_parameter_sets(Codec::H264, &[&PROBE_SPS, &PROBE_PPS])
        .unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(format::dimensions(&format), (1920, 1080));
    probe_h264().unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn deltas_before_the_first_keyframe_give_no_picture() {
    let mut synth = H264Synth::new(320, 240, 5);
    let mut dec = VtDecoder::new(Codec::H264, 640, 480);
    let _ = synth.keyframe();
    assert!(decode(&mut dec, &synth.repeat(), false, 0).is_none());
    // The hint is reported until a picture exists.
    assert_eq!(dec.dimensions(), (640, 480));
    let p = decode(&mut dec, &synth.keyframe(), true, 1)
        .unwrap_or_else(|| panic!("keyframe after deltas"));
    assert_exact(&synth, &p);
}

#[test]
fn resolution_switch_1080p_to_720p_reports_new_dimensions() {
    let mut dec = VtDecoder::new(Codec::H264, 1920, 1080);
    let mut n = 0;
    for (width, height, seed) in [(1920, 1080, 1), (1280, 720, 2), (1920, 1080, 4)] {
        // A new encoder at the new size, as the host does after a resize.
        let mut synth = H264Synth::new(width, height, seed);
        let p = decode(&mut dec, &synth.keyframe(), true, n)
            .unwrap_or_else(|| panic!("{width}x{height} keyframe"));
        // 1080p is coded as 1088 rows and cropped: the cropped size counts.
        assert_eq!((p.width, p.height), (width, height));
        assert_exact(&synth, &p);
        for _ in 0..2 {
            n += 1;
            let p = decode(&mut dec, &synth.repeat(), false, n)
                .unwrap_or_else(|| panic!("{width}x{height} delta"));
            assert_eq!(dec.dimensions(), (width, height));
            assert_exact(&synth, &p);
        }
        n += 1;
    }
}

#[test]
fn keyframe_without_parameter_sets_is_an_actionable_error() {
    let mut synth = H264Synth::new(320, 240, 7);
    let key = synth.keyframe();
    let idr_only: Vec<u8> = annexb::nal_units(&key)
        .filter(|n| annexb::role(Codec::H264, n) == Some(annexb::NalRole::RandomAccess))
        .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
        .collect();
    let mut dec = VtDecoder::new(Codec::H264, 0, 0);
    let err = dec
        .decode(&idr_only, true, 0)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("parameter sets"), "{err}");
    let p = decode(&mut dec, &key, true, 1).unwrap_or_else(|| panic!("full keyframe"));
    assert_exact(&synth, &p);
}

#[test]
fn damaged_data_never_wedges_the_stream() {
    let mut synth = H264Synth::new(320, 240, 9);
    let mut dec = VtDecoder::new(Codec::H264, 0, 0);
    assert!(decode(&mut dec, &synth.keyframe(), true, 0).is_some());
    // A P slice header followed by noise.
    let mut garbage = vec![0, 0, 0, 1, 0x41, 0x9a];
    garbage.extend((0..2000u32).map(|i| pattern(i, i / 7, 11)));
    let failed = dec.decode(&garbage, false, 1).is_err();
    if failed {
        // After an error the references are suspect: deltas wait for a
        // keyframe (the viewer requests one after a few of these).
        assert!(matches!(dec.decode(&synth.repeat(), false, 2), Ok(None)));
    }
    let p = decode(&mut dec, &synth.keyframe(), true, 3)
        .unwrap_or_else(|| panic!("a keyframe recovers"));
    assert_exact(&synth, &p);
}

#[test]
fn decoded_frames_import_into_metal_without_a_copy() {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
        .unwrap_or_else(|e| panic!("no Metal adapter: {e}"));
    let (device, _queue) = pollster::block_on(adapter.request_device(&Default::default()))
        .unwrap_or_else(|e| panic!("no Metal device: {e}"));
    let importer =
        crate::import::Importer::new(&device).unwrap_or_else(|e| panic!("importer: {e}"));

    let mut synth = H264Synth::new(320, 240, 13);
    let mut dec = VtDecoder::new(Codec::H264, 0, 0);
    let frame = dec
        .decode(&synth.keyframe(), true, 0)
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("no picture"));
    let buffer = native(&frame);
    assert_eq!(buffer.layout(), Some(crate::gpu::PixelLayout::Nv12Limited));
    // CVMetalTextureCache only wraps IOSurface-backed, Metal-compatible
    // buffers: this is the zero-copy path the presenter takes.
    let imported = importer
        .import(buffer)
        .unwrap_or_else(|e| panic!("import: {e}"));
    assert_eq!(imported.size(), (320, 240));
}

/// A 420v IOSurface-backed buffer holding smooth gradients.
fn gradient(width: usize, height: usize) -> CvPixelBufferFrame {
    let empty = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: CoreFoundation / CoreVideo constants.
    let (yes, io_key, metal_key) = unsafe {
        (
            objc2_core_foundation::kCFBooleanTrue
                .unwrap_or_else(|| panic!("kCFBooleanTrue missing")),
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        )
    };
    let values: [&CFType; 2] = [&empty, yes];
    let attrs = CFDictionary::<CFString, CFType>::from_slices(&[io_key, metal_key], &values);
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid attributes and out-pointer.
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    assert_eq!(rc, kCVReturnSuccess, "CVPixelBufferCreate");
    let raw = NonNull::new(out).unwrap_or_else(|| panic!("CVPixelBufferCreate gave null"));
    // SAFETY: "Create" returns a +1 reference.
    let buffer = unsafe { CFRetained::from_raw(raw) };
    // SAFETY: lock/unlock bracket the CPU writes, each within the plane's
    // bytes-per-row × rows.
    unsafe {
        assert_eq!(
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)),
            kCVReturnSuccess
        );
        let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
        for y in 0..height {
            let row = std::slice::from_raw_parts_mut(base.add(y * stride), width);
            for (x, px) in row.iter_mut().enumerate() {
                *px = (16 + (x * 150 / width) + (y * 69 / height)) as u8;
            }
        }
        let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, 1).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
        for y in 0..height / 2 {
            let row = std::slice::from_raw_parts_mut(base.add(y * stride), width);
            for (i, px) in row.iter_mut().enumerate() {
                let x = i / 2;
                *px = if i % 2 == 0 {
                    (64 + x * 128 / width) as u8
                } else {
                    (192 - y * 256 / height) as u8
                };
            }
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
    }
    CvPixelBufferFrame::new(buffer)
}

/// PSNR in dB over both planes.
fn psnr(a: &Picture, b: &Picture) -> f64 {
    let pairs = a.y.iter().zip(&b.y).chain(a.uv.iter().zip(&b.uv));
    let (sum, count) = pairs.fold((0f64, 0usize), |(s, c), (&p, &q)| {
        let d = f64::from(p) - f64::from(q);
        (s + d * d, c + 1)
    });
    let mse = sum / count.max(1) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Encode → decode through the platform's own hardware codec factory
/// (task M2's VideoToolbox encoder, never the OpenH264 fallback): a
/// 1280×720 gradient must come back at ≥ 40 dB PSNR, for H.264 and — where
/// this Mac encodes and decodes it in hardware — HEVC.
///
/// Needs a Mac with a hardware video encoder (any Apple silicon Mac, most
/// Intel Macs). It probes first and skips without one (see
/// `tests/support/hardware.rs`); run with
/// `cargo test -p platform-macos --lib decode:: -- --ignored --nocapture`.
#[test]
#[ignore = "needs a Mac with a hardware H.264 encoder (skips without one)"]
fn round_trip_gradient_psnr_at_least_40_db() {
    if !crate::h264_hardware_encoder_available() {
        skip("no hardware H.264 encoder (VideoToolbox)");
        return;
    }
    let platform = crate::MacPlatform::new();
    let factory = platform.codecs.hardware();
    let (width, height) = (1280, 720);
    let source = gradient(width, height);
    let expected = read_planes(&source);
    let mut codecs = vec![Codec::H264];
    if factory.host_encodable().contains(&Codec::Hevc) && hardware_decodes(Codec::Hevc) {
        codecs.push(Codec::Hevc);
    }
    for codec in codecs {
        let cfg = EncoderConfig {
            width: width as u32,
            height: height as u32,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 10_000_000,
            codec,
        };
        let mut encoder = factory
            .new_encoder(None, cfg)
            .unwrap_or_else(|e| panic!("{codec:?} encoder: {e}"));
        let mut decoder = factory
            .new_decoder(None, codec, cfg.width, cfg.height)
            .unwrap_or_else(|e| panic!("{codec:?} decoder: {e}"));
        let input = Frame {
            width: cfg.width,
            height: cfg.height,
            data: FrameData::Native(&source),
        };
        let mut last = None;
        for n in 0..30i64 {
            let units = encoder
                .encode(&input)
                .unwrap_or_else(|e| panic!("{codec:?} encode #{n}: {e}"));
            for unit in units {
                match decoder.decode(&unit.data, unit.keyframe, n * 333_333) {
                    Ok(Some(frame)) => last = Some(read_planes(native(&frame))),
                    Ok(None) => {}
                    Err(e) => panic!("{codec:?} decode #{n}: {e}"),
                }
            }
        }
        let decoded = last.unwrap_or_else(|| panic!("{codec:?}: no picture decoded"));
        assert_eq!((decoded.width, decoded.height), (cfg.width, cfg.height));
        let db = psnr(&expected, &decoded);
        println!("{codec:?}: {db:.1} dB");
        assert!(db >= 40.0, "{codec:?}: PSNR {db:.1} dB < 40 dB");
    }
}

/// SHA-256 (hex) of a decoded picture laid out like FFmpeg's `nv12` rawvideo:
/// the Y rows, then the interleaved UV rows, tightly packed (even sizes).
fn nv12_hash(p: &Picture) -> String {
    let mut hasher = Sha256::new();
    hasher.update(&p.y);
    hasher.update(&p.uv);
    hex::encode(hasher.finalize())
}

/// Decodes every access unit of `g` on `dec`: one picture each, its size and
/// hash, with `dimensions()` checked against every picture.
fn decode_golden(dec: &mut VtDecoder, g: &golden::Golden) -> Vec<((u32, u32), String)> {
    g.access_units()
        .enumerate()
        .map(|(i, (au, key))| {
            let picture = decode(dec, au, key, i as i64)
                .unwrap_or_else(|| panic!("{} AU {i}: no picture", g.name));
            ((picture.width, picture.height), nv12_hash(&picture))
        })
        .collect()
}

/// `got` against FFmpeg's reference for `g`: same count, sizes and hashes.
fn assert_matches_ffmpeg(g: &golden::Golden, got: &[((u32, u32), String)]) {
    assert_eq!(got.len(), g.frames.len(), "{}: picture count", g.name);
    for (i, (got, want)) in got.iter().zip(&g.frames).enumerate() {
        assert_eq!(got.0, want.0, "{} frame {i}: size", g.name);
        assert_eq!(
            got.1, want.1,
            "{} frame {i}: pixels differ from FFmpeg's",
            g.name
        );
    }
}

/// The VideoToolbox decoder reproduces FFmpeg's per-frame NV12 hashes on the
/// golden streams (`native/testdata/golden`). H.264 and HEVC decoding are
/// bit-exact by specification, and VideoToolbox hands out the decoded samples
/// unconverted in `420v` (video range), so the planes must hash identically;
/// a crop, reference or parameter-set mistake changes them. Each H.264 stream
/// is decoded on a fresh decoder, then all of them on one decoder, which
/// crosses the resolutions (640×360, 320×240, 352×288) like a host that
/// changes size mid-session. HEVC is checked only where VideoToolbox decodes
/// it in hardware (`VTIsHardwareDecodeSupported`).
///
/// Run by the macOS CI step (`--ignored`); skips where hardware decode is
/// missing, and fails there under `SCT_MAC_HW_TESTS_REQUIRED=1`.
#[test]
#[ignore = "needs a Mac with hardware decode (VideoToolbox); the macOS CI step runs it"]
fn golden_streams_match_ffmpegs_frame_hashes() {
    if !hardware_decodes(Codec::H264) {
        skip("no hardware H.264 decode (VTIsHardwareDecodeSupported is false)");
        return;
    }
    let goldens = golden::all();
    let h264: Vec<&golden::Golden> = goldens.iter().filter(|g| g.codec == Codec::H264).collect();
    assert!(!h264.is_empty(), "no H.264 golden streams");
    for g in &h264 {
        let mut dec = VtDecoder::new(Codec::H264, 1920, 1080);
        assert_matches_ffmpeg(g, &decode_golden(&mut dec, g));
        println!("{}: {} frames match FFmpeg", g.name, g.frames.len());
    }
    let mut shared = VtDecoder::new(Codec::H264, 1920, 1080);
    for g in &h264 {
        assert_matches_ffmpeg(g, &decode_golden(&mut shared, g));
    }
    println!(
        "{} H.264 streams on one decoder (resolution changes): all match FFmpeg",
        h264.len()
    );
    if hardware_decodes(Codec::Hevc) {
        for g in goldens.iter().filter(|g| g.codec == Codec::Hevc) {
            let mut dec = VtDecoder::new(Codec::Hevc, 1920, 1080);
            assert_matches_ffmpeg(g, &decode_golden(&mut dec, g));
            println!("{}: {} frames match FFmpeg", g.name, g.frames.len());
        }
    } else {
        skip("no hardware HEVC decode (VTIsHardwareDecodeSupported is false)");
    }
}
