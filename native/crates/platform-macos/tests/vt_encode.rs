//! VideoToolbox encoder acceptance (plan task M2), through the platform's own
//! `CodecFactory` exactly as the host uses it. Needs a Mac with a hardware
//! video encoder (every Apple-silicon Mac); no TCC permission is involved, so
//! it also runs on a GitHub macOS runner that exposes VideoToolbox. Every test
//! is `#[ignore]`d because a machine without the hardware cannot pass them —
//! see plans/checklists/M2.md:
//!
//! ```text
//! cargo test -p platform-macos --test vt_encode -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` keeps the timing test from sharing the media engine.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use objc2_core_video::{
    kCVReturnSuccess, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use platform_api::{
    Codec, EncodedUnit, EncoderConfig, Frame, FrameData, Platform, PixelFormat, VideoEncoder,
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

/// Full-frame luma noise: far more detail than any test bitrate can carry,
/// so the rate controller — not the content — sets the output size.
fn noise(seed: u32) -> Picture {
    let mut state = seed.wrapping_mul(2_654_435_761).max(1);
    let mut next = || {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let (w, h) = (W as usize, H as usize);
    Picture {
        y: (0..w * h).map(|_| 16 + (next() % 220) as u8).collect(),
        uv: vec![128; w * h / 2],
    }
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
#[ignore = "needs macOS hardware"]
fn encodes_300_frames_first_idr_mean_under_8ms() {
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
    assert_eq!(first.data[4] & 0x1f, 7, "keyframe does not lead with an SPS");
    assert!(
        units.iter().skip(1).all(|u| !u.keyframe),
        "the encoder inserted a keyframe nobody asked for"
    );
    units.iter().for_each(assert_annexb);
    assert!(units.len() >= 290, "only {} of 300 frames encoded", units.len());
    assert!(mean <= Duration::from_millis(8), "mean encode {mean:?} > 8 ms");
}

/// Five real-time seconds of 60 fps noise at 6 Mbit/s: the bytes produced
/// are within ±15 % of the target. A forced keyframe in the middle comes out
/// as one, and a live bitrate change takes without a rebuild.
#[test]
#[ignore = "needs macOS hardware"]
fn holds_the_bitrate_within_15_percent_over_5_seconds() {
    logs();
    const TARGET: u32 = 6_000_000;
    let mut enc = encoder(TARGET / 2);
    // Live change before measuring: the session must follow it.
    enc.set_bitrate(TARGET).expect("live bitrate change");
    let pictures: Vec<Picture> = (0..16).map(noise).collect();
    let interval = Duration::from_micros(16_667);
    let start = Instant::now();
    let mut bytes = 0usize;
    let mut forced_key_seen = false;
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
        for u in enc.encode(&pictures[i % pictures.len()].frame()).expect("encode") {
            if i >= 150 && u.keyframe {
                forced_key_seen = true;
            }
            bytes += u.data.len();
        }
        i += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let bps = bytes as f64 * 8.0 / secs;
    let ratio = bps / TARGET as f64;
    eprintln!("{i} frames in {secs:.2} s: {:.2} Mbit/s ({ratio:.3} × target)", bps / 1e6);
    assert!(forced_key_seen, "forced keyframe never came out");
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

/// Encode → decode through the platform factory on both sides → luma PSNR
/// ≥ 40 dB on a gradient. The decoder is whatever the platform provides for
/// H.264: VideoToolbox once task M3 is merged; Cisco OpenH264 until then
/// (this test then also needs the network for the one-time download).
#[test]
#[ignore = "needs macOS hardware"]
fn round_trip_through_the_platform_decoder_is_at_least_40_db() {
    logs();
    let platform = MacPlatform::new();
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
            if let Some(frame) = dec.decode(&u.data, u.keyframe, i * 166_667).expect("decode") {
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
