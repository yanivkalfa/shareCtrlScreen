//! VideoToolbox encoder acceptance (plan task M2), through the platform's own
//! `CodecFactory` exactly as the host uses it. Needs a Mac with a hardware
//! video encoder (every Apple-silicon Mac); no TCC permission is involved.
//!
//! Every test is `#[ignore]`d so `cargo test --workspace` stays fast, and each
//! probes for a hardware H.264 encoder first and skips with a printed line
//! without one (`tests/support/hardware.rs`). The macOS CI job runs the
//! non-timing tests with `--ignored --nocapture` and
//! `SCT_MAC_HW_TESTS_REQUIRED=1` (`.github/workflows/native-ci.yml`).
//!
//! The two timing tests (`encodes_300_frames_first_idr_mean_under_8ms`,
//! `holds_the_bitrate_within_15_percent_over_5_seconds`) are device-only: a
//! shared CI runner is too loaded for a wall-clock budget, so the CI step skips
//! them by name. Run them on a Mac, `--test-threads=1` so they do not share the
//! media engine (plans/checklists/M2.md §3):
//!
//! ```text
//! cargo test --release -p platform-macos --test vt_encode -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "macos")]

#[path = "support/hardware.rs"]
mod hardware;

use std::time::{Duration, Instant};

use hardware::skip;

use objc2_core_video::{
    kCVReturnSuccess, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use platform_api::{
    Codec, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat, Platform, VideoEncoder,
};
use platform_macos::gpu::CvPixelBufferFrame;
use platform_macos::MacPlatform;

const W: u32 = 1920;
const H: u32 = 1080;

fn logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("platform_macos=debug,codec_openh264=debug")
        .with_test_writer()
        .try_init();
}

fn config(bitrate_bps: u32) -> EncoderConfig {
    EncoderConfig {
        width: W,
        height: H,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps,
        codec: Codec::H264,
    }
}

/// A CPU NV12 picture.
struct Picture {
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl Picture {
    fn frame(&self) -> Frame<'_> {
        Frame::nv12(W, H, &self.y, W as usize, &self.uv, W as usize)
    }
}

/// A diagonal luma ramp in BT.709 limited range, shifted by `phase` pixels so
/// consecutive frames differ like a slow scroll; neutral chroma.
fn gradient(phase: usize) -> Picture {
    let (w, h) = (W as usize, H as usize);
    let y = (0..w * h)
        .map(|i| {
            let (x, row) = (i % w, i / w);
            (16 + ((x + row + phase) % (w + h)) * 219 / (w + h)) as u8
        })
        .collect();
    Picture {
        y,
        uv: vec![128; w * h / 2],
    }
}

/// The scrolling ramp of [`gradient`] plus fresh low-amplitude luma noise
/// (±6 codes) every frame — content for the rate-control test that must
/// sit between two bounds:
/// * left alone, the encoder would spend *more* than the target on it: new
///   noise every frame cannot be predicted, and at low QP every one of its
///   coefficients costs bits;
/// * at the max-QP floor of 32 (quantizer step ≈ 26) noise this small
///   quantizes to almost nothing, so the target is reachable *without*
///   dropping frames — the rate controller settles at a QP below the floor.
///
/// Full-amplitude noise would break the second bound: no QP ≤ 32 fits it
/// in the budget, so the encoder could only meet the target by dropping
/// most frames (WWDC21 10158), which says nothing about its rate control.
fn textured(seed: u32) -> Picture {
    let mut state = seed.wrapping_mul(2_654_435_761).max(1);
    let mut next = || {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut picture = gradient(seed as usize * 4);
    for px in &mut picture.y {
        let offset = (next() % 13) as i32 - 6;
        *px = (*px as i32 + offset).clamp(16, 235) as u8;
    }
    picture
}

fn encoder(bitrate_bps: u32) -> Box<dyn VideoEncoder> {
    let platform = MacPlatform::new();
    // OpenH264 off: a software encoder passing these tests would prove nothing.
    platform.openh264().set_enabled(false);
    platform
        .codecs()
        .new_encoder(None, config(bitrate_bps))
        .unwrap_or_else(|e| panic!("no hardware H.264 encoder: {e}"))
}

fn assert_annexb(u: &EncodedUnit) {
    assert!(
        u.data.starts_with(&[0, 0, 0, 1]),
        "unit does not start with an Annex-B start code"
    );
}

/// 300 synthetic 1080p frames: the first unit is an IDR with SPS/PPS in
/// front, nothing else is a keyframe, and the mean `encode` call is ≤ 8 ms.
#[test]
#[ignore = "timing, device-only: a shared CI runner is too loaded (skipped by name in CI)"]
fn encodes_300_frames_first_idr_mean_under_8ms() {
    if !platform_macos::h264_hardware_encoder_available() {
        skip("no hardware H.264 encoder (VideoToolbox)");
        return;
    }
    logs();
    let mut enc = encoder(8_000_000);
    let pictures: Vec<Picture> = (0..8).map(|i| gradient(i * 4)).collect();
    let mut units = Vec::new();
    let mut spent = Duration::ZERO;
    for i in 0..300 {
        let frame = pictures[i % pictures.len()].frame();
        let start = Instant::now();
        let out = enc.encode(&frame).expect("encode");
        spent += start.elapsed();
        units.extend(out);
    }
    let mean = spent / 300;
    eprintln!("{} units, mean encode {mean:?}", units.len());
    let first = units.first().expect("no output at all");
    assert!(first.keyframe, "first unit is not a keyframe");
    assert_annexb(first);
    assert_eq!(
        first.data[4] & 0x1f,
        7,
        "keyframe does not lead with an SPS"
    );
    assert!(
        units.iter().skip(1).all(|u| !u.keyframe),
        "the encoder inserted a keyframe nobody asked for"
    );
    units.iter().for_each(assert_annexb);
    assert!(
        units.len() >= 290,
        "only {} of 300 frames encoded",
        units.len()
    );
    assert!(
        mean <= Duration::from_millis(8),
        "mean encode {mean:?} > 8 ms"
    );
}

/// Five real-time seconds of 60 fps [`textured`] content at 6 Mbit/s: the
/// bytes produced are within ±15 % of the target, and (because that content
/// fits under the QP floor) at least 90 % of the frames are encoded rather
/// than dropped. A forced keyframe in the middle comes out as one, and a
/// live bitrate change takes without a rebuild.
#[test]
#[ignore = "timing, device-only: a shared CI runner is too loaded (skipped by name in CI)"]
fn holds_the_bitrate_within_15_percent_over_5_seconds() {
    if !platform_macos::h264_hardware_encoder_available() {
        skip("no hardware H.264 encoder (VideoToolbox)");
        return;
    }
    logs();
    const TARGET: u32 = 6_000_000;
    let mut enc = encoder(TARGET / 2);
    // Live change before measuring: the session must follow it.
    enc.set_bitrate(TARGET).expect("live bitrate change");
    let pictures: Vec<Picture> = (0..16).map(textured).collect();
    let interval = Duration::from_micros(16_667);
    let start = Instant::now();
    let mut bytes = 0usize;
    let mut forced_key_seen = false;
    let mut encoded = 0usize;
    let mut i = 0usize;
    while start.elapsed() < Duration::from_secs(5) {
        let due = start + interval * i as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let forced = i == 150;
        if forced {
            enc.force_keyframe();
        }
        for u in enc
            .encode(&pictures[i % pictures.len()].frame())
            .expect("encode")
        {
            if i >= 150 && u.keyframe {
                forced_key_seen = true;
            }
            bytes += u.data.len();
            encoded += 1;
        }
        i += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let bps = bytes as f64 * 8.0 / secs;
    let ratio = bps / TARGET as f64;
    eprintln!(
        "{i} frames submitted, {encoded} encoded in {secs:.2} s: {:.2} Mbit/s \
         ({ratio:.3} × target)",
        bps / 1e6
    );
    assert!(forced_key_seen, "forced keyframe never came out");
    assert!(
        encoded * 10 >= i * 9,
        "only {encoded} of {i} frames encoded: the target was met by dropping frames"
    );
    assert!(
        (0.85..=1.15).contains(&ratio),
        "{:.2} Mbit/s is outside ±15 % of {} Mbit/s",
        bps / 1e6,
        TARGET / 1_000_000
    );
}

/// The luma plane of a decoded picture, tightly packed.
fn decoded_luma(frame: &Frame<'_>) -> Vec<u8> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    match frame.data {
        FrameData::Cpu {
            format: PixelFormat::Nv12,
            plane0,
            ..
        } => (0..h)
            .flat_map(|r| plane0.data[r * plane0.stride..r * plane0.stride + w].to_vec())
            .collect(),
        FrameData::Native(any) => {
            let native = any
                .downcast_ref::<CvPixelBufferFrame>()
                .expect("a decoder output this test does not know");
            let buffer = native.pixel_buffer();
            // SAFETY: read-only lock bracketing reads within the plane's
            // reported stride × rows.
            unsafe {
                assert_eq!(
                    CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
                    kCVReturnSuccess
                );
                let base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
                let plane = std::slice::from_raw_parts(base, stride * h);
                let luma = (0..h)
                    .flat_map(|r| plane[r * stride..r * stride + w].to_vec())
                    .collect();
                CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
                luma
            }
        }
        other => panic!("unexpected decoder output {other:?}"),
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Encode → decode through the platform factory → luma PSNR ≥ 40 dB on a
/// gradient. Both sides are VideoToolbox: OpenH264 stays off throughout, so a
/// failed hardware session fails the test instead of being replaced by the
/// software codec, and the decoder is the M3 VideoToolbox decoder.
#[test]
#[ignore = "needs a Mac with a hardware H.264 encoder (skips without one)"]
fn round_trip_through_the_platform_decoder_is_at_least_40_db() {
    if !platform_macos::h264_hardware_encoder_available() {
        skip("no hardware H.264 encoder (VideoToolbox)");
        return;
    }
    logs();
    let platform = MacPlatform::new();
    platform.openh264().set_enabled(false);
    let mut enc = platform
        .codecs()
        .new_encoder(None, config(8_000_000))
        .expect("hardware H.264 encoder");
    let mut dec = platform
        .codecs()
        .new_decoder(None, Codec::H264, W, H)
        .expect("H.264 decoder");
    let source = gradient(0);
    let mut best = 0.0f64;
    let mut decoded = 0;
    for i in 0..30 {
        for u in enc.encode(&source.frame()).expect("encode") {
            if let Some(frame) = dec
                .decode(&u.data, u.keyframe, i * 166_667)
                .expect("decode")
            {
                assert_eq!((frame.width, frame.height), (W, H));
                best = best.max(psnr(&source.y, &decoded_luma(&frame)));
                decoded += 1;
            }
        }
    }
    eprintln!("{decoded} frames decoded, best luma PSNR {best:.2} dB");
    assert!(decoded > 0, "nothing decoded");
    assert!(best >= 40.0, "luma PSNR {best:.2} dB < 40 dB");
}
