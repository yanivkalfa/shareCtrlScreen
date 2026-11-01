//! Encode → decode through the real Cisco binary.
//!
//! The binary is downloaded from Cisco once into Cargo's per-target test
//! directory (never the repo) and reused by later runs. These tests need the
//! network on the first run; set `SCT_OPENH264_OFFLINE=1` to skip them on a
//! machine that has none (they then print why and pass vacuously — CI does not
//! set it, so the round trip is enforced there).

use std::any::Any;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use codec_openh264::{OpenH264, SoftwareDecoder, SoftwareEncoder, WithSoftwareH264};
use platform_api::mock::split_annex_b;
use platform_api::unavailable::NoCodecs;
use platform_api::{
    Codec, CodecFactory, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat, VideoDecoder,
    VideoEncoder,
};

fn openh264() -> Option<Arc<OpenH264>> {
    if std::env::var_os("SCT_OPENH264_OFFLINE").is_some() {
        eprintln!("SCT_OPENH264_OFFLINE set: skipping the OpenH264 round trip");
        return None;
    }
    static SHARED: OnceLock<Arc<OpenH264>> = OnceLock::new();
    let h = SHARED.get_or_init(|| {
        Arc::new(OpenH264::new(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("openh264-codecs"),
        ))
    });
    if let Err(e) = h.ensure() {
        panic!("Cisco OpenH264 unavailable (set SCT_OPENH264_OFFLINE=1 to skip): {e}");
    }
    Some(h.clone())
}

fn cfg(width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps_num: 30,
        fps_den: 1,
        bitrate_bps: 4_000_000,
        codec: Codec::H264,
    }
}

/// A screen-like picture: flat background, a block of dense "text", and a
/// window that moves with `t`.
fn screen_bgra(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            let (mut b, mut g, mut r) = (235u8, 228u8, 220u8);
            // "Text": a deterministic high-frequency pattern in the top-left.
            if x < w / 2 && y < h / 2 && ((x / 2) ^ (y / 3)).wrapping_mul(2654435761) % 7 == 0 {
                (b, g, r) = (20, 20, 20);
            }
            // Moving window.
            let wx = (t * 8) % (w / 2);
            if x >= wx + w / 4 && x < wx + w / 2 && y >= h / 2 && y < h * 3 / 4 {
                (b, g, r) = (200, 120, (x % 256) as u8);
            }
            px[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    px
}

fn nal_types(unit: &EncodedUnit) -> Vec<u8> {
    split_annex_b(&unit.data)
        .iter()
        .map(|n| n[0] & 0x1f)
        .collect()
}

/// Mean absolute difference between two luma planes of the same size.
fn luma_mad(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize, w: usize, h: usize) -> f64 {
    let mut sum = 0u64;
    for y in 0..h {
        for x in 0..w {
            sum += (a[y * a_stride + x] as i32 - b[y * b_stride + x] as i32).unsigned_abs() as u64;
        }
    }
    sum as f64 / (w * h) as f64
}

/// The luma OpenH264 was given, recomputed independently (BT.709 limited).
fn reference_luma(bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let (b, g, r) = (
                bgra[i * 4] as f64,
                bgra[i * 4 + 1] as f64,
                bgra[i * 4 + 2] as f64,
            );
            (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0).round() as u8
        })
        .collect()
}

fn decoded_luma(frame: &Frame<'_>) -> (Vec<u8>, usize) {
    match frame.data {
        FrameData::Cpu {
            format: PixelFormat::Nv12,
            plane0,
            plane1: Some(_),
        } => (plane0.data.to_vec(), plane0.stride),
        other => panic!("decoder must produce CPU NV12, got {other:?}"),
    }
}

#[test]
fn bgra_round_trip_is_faithful_and_keyframes_are_self_contained() {
    let Some(h264) = openh264() else { return };
    let (w, h) = (320usize, 240usize);
    let mut enc = SoftwareEncoder::new(&h264, cfg(w as u32, h as u32), None).unwrap();
    let mut dec = SoftwareDecoder::new(&h264, Codec::H264, 1920, 1080).unwrap();
    enc.force_keyframe(); // as the host does; must be harmless before frame 1

    let mut decoded = 0;
    for t in 0..12 {
        if t == 6 {
            enc.force_keyframe();
            enc.set_bitrate(1_500_000).unwrap();
        }
        let src = screen_bgra(w, h, t);
        let units = enc
            .encode(&Frame::bgra(w as u32, h as u32, &src, w * 4))
            .unwrap();
        for u in &units {
            let types = nal_types(u);
            if u.keyframe {
                assert!(
                    types.contains(&7) && types.contains(&8) && types.contains(&5),
                    "{types:?}"
                );
            } else {
                assert!(!types.contains(&5), "delta carries an IDR slice: {types:?}");
            }
            if t == 0 {
                assert!(u.keyframe, "the first access unit must be an IDR");
            }
            let pic = dec
                .decode(&u.data, u.keyframe, t as i64 * 333_333)
                .unwrap()
                .expect("no reordering: every AU yields its picture");
            assert_eq!((pic.width, pic.height), (w as u32, h as u32));
            let (y, stride) = decoded_luma(&pic);
            let mad = luma_mad(&y, stride, &reference_luma(&src, w, h), w, w, h);
            assert!(mad < 4.0, "frame {t}: luma error {mad:.2}");
            decoded += 1;
        }
    }
    assert_eq!(dec.dimensions(), (w as u32, h as u32));
    assert!(decoded >= 11, "only {decoded} of 12 frames came out");
}

#[test]
fn forced_keyframe_arrives_on_the_next_frame() {
    let Some(h264) = openh264() else { return };
    let (w, h) = (160usize, 96usize);
    let mut enc = SoftwareEncoder::new(&h264, cfg(w as u32, h as u32), None).unwrap();
    let mut keys = Vec::new();
    for t in 0..8 {
        if t == 5 {
            enc.force_keyframe();
        }
        let src = screen_bgra(w, h, t);
        for u in enc
            .encode(&Frame::bgra(w as u32, h as u32, &src, w * 4))
            .unwrap()
        {
            keys.push(u.keyframe);
        }
    }
    assert_eq!(
        keys,
        [true, false, false, false, false, true, false, false],
        "IDR only on frame 0 and when forced (intra period 0; a moving window is \
         not a scene change)"
    );
}

#[test]
fn nv12_input_cropping_and_scaling() {
    let Some(h264) = openh264() else { return };
    // NV12 input of an odd-sized screen (cropped by one pixel).
    let (fw, fh) = (161usize, 97usize);
    let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
    let y: Vec<u8> = (0..fw * fh).map(|i| (16 + (i % 200)) as u8).collect();
    let uv = vec![128u8; cw * 2 * ch];
    let mut enc = SoftwareEncoder::new(&h264, cfg(160, 96), None).unwrap();
    let mut dec = SoftwareDecoder::new(&h264, Codec::H264, 160, 96).unwrap();
    let units = enc
        .encode(&Frame::nv12(fw as u32, fh as u32, &y, fw, &uv, cw * 2))
        .unwrap();
    let pic = dec.decode(&units[0].data, true, 0).unwrap().unwrap();
    assert_eq!((pic.width, pic.height), (160, 96));

    // BGRA at twice the configured size (the host encoding at the viewer's size).
    let src = screen_bgra(640, 480, 0);
    let mut enc = SoftwareEncoder::new(&h264, cfg(320, 240), None).unwrap();
    let units = enc.encode(&Frame::bgra(640, 480, &src, 640 * 4)).unwrap();
    let mut dec = SoftwareDecoder::new(&h264, Codec::H264, 0, 0).unwrap();
    let pic = dec.decode(&units[0].data, true, 0).unwrap().unwrap();
    assert_eq!((pic.width, pic.height), (320, 240));
}

#[test]
fn deltas_before_the_first_keyframe_are_dropped_not_errors() {
    let Some(h264) = openh264() else { return };
    let (w, h) = (160usize, 96usize);
    let mut enc = SoftwareEncoder::new(&h264, cfg(w as u32, h as u32), None).unwrap();
    let mut units = Vec::new();
    for t in 0..3 {
        let src = screen_bgra(w, h, t);
        units.extend(
            enc.encode(&Frame::bgra(w as u32, h as u32, &src, w * 4))
                .unwrap(),
        );
    }
    let mut dec = SoftwareDecoder::new(&h264, Codec::H264, 1920, 1080).unwrap();
    // A viewer joining late: deltas first.
    assert!(dec.decode(&units[1].data, false, 0).unwrap().is_none());
    assert!(dec.decode(&units[0].data, true, 0).unwrap().is_some());
    assert!(dec.decode(&units[1].data, false, 0).unwrap().is_some());
}

#[test]
fn adapter_falls_back_to_openh264_when_hardware_has_no_h264() {
    let Some(h264) = openh264() else { return };
    let codecs = WithSoftwareH264::new(
        NoCodecs {
            reason: "no hardware codec in this test",
        },
        h264,
    );
    assert_eq!(codecs.host_encodable(), vec![Codec::H264]);
    let mut enc = codecs.new_encoder(None, cfg(160, 96)).unwrap();
    let mut dec = codecs
        .new_decoder(None::<&dyn Any>, Codec::H264, 160, 96)
        .unwrap();
    let src = screen_bgra(160, 96, 0);
    let units = enc.encode(&Frame::bgra(160, 96, &src, 160 * 4)).unwrap();
    assert!(dec.decode(&units[0].data, true, 0).unwrap().is_some());
}

/// Acceptance: 1080p30 encode ≤ 12 ms/frame on a 4-core runner. Timing is
/// only meaningful with optimisations (the conversion is our code):
/// `cargo test -p codec-openh264 --release --test roundtrip -- --ignored`.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "timing needs --release (BGRA→I420 is compiled Rust)"
)]
fn encode_1080p30_within_budget() {
    let Some(h264) = openh264() else { return };
    let (w, h) = (1920usize, 1080usize);
    let mut enc = SoftwareEncoder::new(&h264, cfg(w as u32, h as u32), None).unwrap();
    enc.set_bitrate(8_000_000).unwrap();
    let frames: Vec<Vec<u8>> = (0..8).map(|t| screen_bgra(w, h, t)).collect();
    // Warm-up: the IDR and the first deltas are not the steady state.
    for f in frames.iter().take(3) {
        enc.encode(&Frame::bgra(w as u32, h as u32, f, w * 4))
            .unwrap();
    }
    let mut times: Vec<Duration> = (0..60)
        .map(|i| {
            let f = &frames[i % frames.len()];
            let start = Instant::now();
            enc.encode(&Frame::bgra(w as u32, h as u32, f, w * 4))
                .unwrap();
            start.elapsed()
        })
        .collect();
    times.sort();
    // The median, not the mean: one descheduled frame on a shared CI runner
    // says nothing about the encoder.
    let median = times[times.len() / 2];
    eprintln!(
        "openh264 1080p encode (incl. BGRA→I420): median {median:?}, min {:?}, max {:?}",
        times[0],
        times[times.len() - 1]
    );
    assert!(
        median <= Duration::from_millis(12),
        "1080p encode median {median:?}/frame exceeds the 12 ms budget"
    );
}
