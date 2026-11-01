//! The Cisco OpenH264 rows of the cross-codec matrix (task I5): the decoder
//! against every committed H.264 golden stream (`codec_goldens`), and the
//! encoder's output shape.
//!
//! The binary is downloaded from Cisco once into Cargo's per-target test
//! directory (never the repo). Without network — or with
//! `SCT_OPENH264_OFFLINE=1` — the tests print `SKIP openh264` and pass,
//! unless `SCT_CODEC_TESTS_REQUIRED` names `openh264` (CI does: its runners
//! have the network).

use std::sync::{Arc, OnceLock};

use codec_goldens::golden::{check_decoder, cpu_frame_hash, DecoderUnderTest};
use codec_goldens::{session, shape, skip};
use codec_openh264::{OpenH264, SoftwareDecoder, SoftwareEncoder};
use platform_api::{Codec, CodecError, EncoderConfig, VideoDecoder};

/// The shared binary, or why it is not available.
fn openh264() -> Result<Arc<OpenH264>, String> {
    if std::env::var_os("SCT_OPENH264_OFFLINE").is_some() {
        return Err("SCT_OPENH264_OFFLINE is set".into());
    }
    static SHARED: OnceLock<Arc<OpenH264>> = OnceLock::new();
    let h = SHARED.get_or_init(|| {
        Arc::new(OpenH264::new(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("openh264-codecs"),
        ))
    });
    h.ensure()
        .map_err(|e| format!("Cisco OpenH264 unavailable: {e}"))?;
    Ok(h.clone())
}

/// The OpenH264 decoder row: every H.264 golden, bit-exact with FFmpeg
/// (OpenH264 is a conforming decoder for the profiles it supports and
/// outputs I420, which `SoftwareDecoder` interleaves into NV12 without
/// changing a sample).
#[test]
fn openh264_decodes_the_golden_streams() {
    let h264 = match openh264() {
        Ok(h) => h,
        Err(e) => return skip::skip("openh264", &e),
    };
    check_decoder(DecoderUnderTest {
        name: "openh264",
        decodes: |c| c == Codec::H264,
        make: |c, w, h| -> Result<Box<dyn VideoDecoder>, CodecError> {
            Ok(Box::new(SoftwareDecoder::new(&h264, c, w, h)?))
        },
        hash: cpu_frame_hash,
    });
}

/// Regression (task I5): a Main-profile stream — NVENC's golden, the
/// profile NVENC, Media Foundation and VideoToolbox hosts send — gives one
/// picture per access unit, immediately. OpenH264 holds Main/High pictures
/// in its reorder buffer; before `SoftwareDecoder` flushed it, every access
/// unit of such a stream returned no picture.
#[test]
fn main_profile_streams_give_a_picture_per_access_unit() {
    let h264 = match openh264() {
        Ok(h) => h,
        Err(e) => return skip::skip("openh264", &e),
    };
    let path = codec_goldens::golden::dir().join("nvenc-h264-main-640x360.h264");
    let g = codec_goldens::golden::load(&path).unwrap_or_else(|e| panic!("{e}"));
    let mut dec =
        SoftwareDecoder::new(&h264, Codec::H264, 1920, 1080).unwrap_or_else(|e| panic!("{e}"));
    for (i, (au, key)) in g.access_units().enumerate() {
        match dec.decode(au, key, 0) {
            Ok(Some(f)) => assert_eq!((f.width, f.height), (640, 360), "AU {i}"),
            Ok(None) => panic!("AU {i} of a Main-profile stream gave no picture"),
            Err(e) => panic!("AU {i}: {e}"),
        }
    }
}

/// Encoder-output conformance for OpenH264: the scripted session has the
/// engine's shape (IDR first with SPS/PPS, parameter sets on every IDR, no
/// reordering, the forced keyframe on its frame, one unit per frame).
#[test]
fn openh264_output_has_the_engine_shape() {
    let h264 = match openh264() {
        Ok(h) => h,
        Err(e) => return skip::skip("openh264", &e),
    };
    let cfg = EncoderConfig {
        width: 320,
        height: 240,
        fps_num: 30,
        fps_den: 1,
        bitrate_bps: session::START_BITRATE,
        codec: Codec::H264,
    };
    let mut enc = SoftwareEncoder::new(&h264, cfg, None).unwrap_or_else(|e| panic!("{e}"));
    let s =
        session::record_cpu(&mut enc, 320, 240, session::FRAMES).unwrap_or_else(|e| panic!("{e}"));
    match shape::check_session(Codec::H264, &s) {
        Ok(shape) => eprintln!("OpenH264: {shape:?}"),
        Err(e) => panic!("OpenH264: {e}"),
    }
}
