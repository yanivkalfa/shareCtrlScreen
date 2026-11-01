//! The VideoToolbox encoder's rows of the cross-codec matrix (task I5): its
//! output shape, and the regeneration of the `vt-*` golden streams every
//! other decoder (Media Foundation, OpenH264, NVDEC, VA-API) is then checked
//! against. The VideoToolbox *decoder* row is
//! `decode::tests::golden_streams_match_ffmpegs_frame_hashes` (task T2).
//!
//! Both tests are `#[ignore]`d — they need a Mac with a hardware encoder —
//! and probe first, skipping with a printed line without one
//! (`tests/support/hardware.rs`). The procedure is in plans/checklists/I5.md:
//!
//! ```text
//! cargo test -p platform-macos --test vt_goldens -- --ignored --nocapture
//! ```

#![cfg(target_os = "macos")]

#[path = "support/hardware.rs"]
mod hardware;

use codec_goldens::session::{self, Session, FRAMES, START_BITRATE};
use codec_goldens::shape;
use hardware::encoder_gate;
use platform_api::{Codec, EncoderConfig, Platform};
use platform_macos::MacPlatform;

/// The scripted session on the VideoToolbox encoder, through the platform's
/// factory with OpenH264 off (a software encoder must not stand in).
fn vt_session(codec: Codec, width: u32, height: u32) -> Session {
    let platform = MacPlatform::new();
    platform.openh264().set_enabled(false);
    let cfg = EncoderConfig {
        width,
        height,
        fps_num: 30,
        fps_den: 1,
        bitrate_bps: START_BITRATE,
        codec,
    };
    let mut enc = platform
        .codecs()
        .new_encoder(None, cfg)
        .unwrap_or_else(|e| panic!("VideoToolbox {codec:?} encoder: {e}"));
    session::record_cpu(enc.as_mut(), width, height, FRAMES)
        .unwrap_or_else(|e| panic!("VideoToolbox {codec:?} session: {e}"))
}

fn codecs() -> Vec<Codec> {
    MacPlatform::new().codecs().host_encodable()
}

/// Encoder-output conformance: the scripted session (forced keyframe, live
/// bitrate drop) has the engine's shape — H.264, and HEVC where the Mac
/// encodes it.
#[test]
#[ignore = "needs a Mac with a hardware encoder"]
fn videotoolbox_output_has_the_engine_shape() {
    if !encoder_gate(platform_macos::h264_hardware_encoder()) {
        return;
    }
    for codec in codecs() {
        let s = vt_session(codec, 640, 360);
        match shape::check_session(codec, &s) {
            Ok(shape) => eprintln!("VideoToolbox {codec:?}: {shape:?}"),
            Err(e) => panic!("VideoToolbox {codec:?}: {e}"),
        }
    }
}

/// Step one of the VideoToolbox goldens (README): writes
/// `vt-h264-640x360.h264`, `vt-h264-480x270.h264` (a second size for the
/// matrix's resolution change) and, where the Mac encodes HEVC,
/// `vt-hevc-640x360.hevc`, each with its `.aus`, into `testdata/golden`.
/// Existing files are never replaced. Step two (FFmpeg's `.sha256`) runs
/// wherever FFmpeg is installed:
/// `cargo test -p codec-goldens -- --ignored write_missing_ffmpeg_hashes`.
#[test]
#[ignore = "needs a Mac with a hardware encoder"]
fn regenerate_videotoolbox_goldens() {
    if !encoder_gate(platform_macos::h264_hardware_encoder()) {
        return;
    }
    let dir = codec_goldens::golden::dir();
    let mut streams = vec![
        ("vt-h264-640x360", Codec::H264, 640, 360),
        ("vt-h264-480x270", Codec::H264, 480, 270),
    ];
    if codecs().contains(&Codec::Hevc) {
        streams.push(("vt-hevc-640x360", Codec::Hevc, 640, 360));
    }
    for (name, codec, w, h) in streams {
        let s = vt_session(codec, w, h);
        if let Err(e) = shape::check_session(codec, &s) {
            panic!("{name}: {e}");
        }
        session::write(&dir, name, codec, &s.units).unwrap_or_else(|e| panic!("{e}"));
    }
}
