//! The hardware-test convention of platform-macos, shared by the crate's unit
//! tests (`src/lib.rs` includes this file with `#[path]`) and by
//! `tests/vt_encode.rs`.
//!
//! A hardware test probes first and calls [`skip`] when the hardware is
//! absent: one line is printed and the test passes, so `cargo test` stays green
//! on a Mac without it (a VM, an Intel Mac with no hardware encoder). The macOS
//! CI job sets `SCT_MAC_HW_TESTS_REQUIRED=1` on its hardware step, which turns a
//! skip into a failure, so a runner that is supposed to have the hardware cannot
//! go quiet. Mirrors `SCT_X11_TESTS_REQUIRED` in platform-linux
//! (`tests/support/xserver.rs`, `skip`).

/// Reports missing hardware: a printed skip, or a panic when
/// `SCT_MAC_HW_TESTS_REQUIRED=1` says this run must exercise everything.
pub fn skip(why: &str) {
    if std::env::var_os("SCT_MAC_HW_TESTS_REQUIRED").is_some_and(|v| v == "1") {
        panic!("{why}, and SCT_MAC_HW_TESTS_REQUIRED=1");
    }
    eprintln!("skipping: {why}");
}
