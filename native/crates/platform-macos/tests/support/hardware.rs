//! The hardware-test convention of platform-macos, shared by the crate's unit
//! tests (`src/lib.rs` includes this file with `#[path]`) and by
//! `tests/vt_encode.rs`.
//!
//! A hardware test probes first and skips when the hardware is absent: the test
//! passes, with a printed line. On GitHub Actions the skip is also a
//! `::warning::` annotation, so a skip is visible in the run rather than
//! looking like a pass. `SCT_MAC_HW_TESTS_REQUIRED=1` turns every skip into a
//! failure; it is opt-in (a bare-metal or self-hosted Mac, or a runner image
//! once a run has shown the hardware is there). Mirrors `SCT_X11_TESTS_REQUIRED`
//! in platform-linux (`tests/support/xserver.rs`, `skip`).

use platform_api::CodecError;

/// Reports missing hardware: a printed skip (plus a workflow warning on
/// GitHub Actions), or a panic when `SCT_MAC_HW_TESTS_REQUIRED=1`.
pub fn skip(why: &str) {
    if std::env::var_os("SCT_MAC_HW_TESTS_REQUIRED").is_some_and(|v| v == "1") {
        panic!("{why}, and SCT_MAC_HW_TESTS_REQUIRED=1");
    }
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        println!("::warning title=VideoToolbox hardware test skipped::{why}");
    }
    eprintln!("skipping: {why}");
}

/// The gate of a test that needs a hardware H.264 encoder, given the result of
/// `platform_macos::h264_hardware_encoder()`: `true` runs the test. A probe that
/// reports no encoder skips; any other error is a failure, not a skip.
pub fn encoder_gate(probe: Result<bool, CodecError>) -> bool {
    match probe {
        Ok(true) => true,
        Ok(false) => {
            skip("no hardware H.264 encoder (VideoToolbox)");
            false
        }
        Err(e) => panic!("the hardware H.264 encoder probe failed: {e}"),
    }
}
