//! Skipping a codec test whose hardware or binary is absent, and turning the
//! skip into a failure where CI knows the backend exists.
//!
//! Backend names (what `SCT_CODEC_TESTS_REQUIRED` lists):
//! `media-foundation` (MF decoder: hardware, else Microsoft's software
//! H.264/HEVC MFTs), `media-foundation-encoder`, `openh264`, `nvenc`,
//! `nvdec`, `vaapi`, `vaapi-encoder`, `videotoolbox`,
//! `videotoolbox-encoder`.

/// The environment variable that turns skips into failures.
pub const REQUIRED_ENV: &str = "SCT_CODEC_TESTS_REQUIRED";

/// Whether `list` (the variable's value) names `backend`.
fn names(list: &str, backend: &str) -> bool {
    let list = list.trim();
    list == "1" || list.eq_ignore_ascii_case("all") || list.split(',').any(|b| b.trim() == backend)
}

/// Whether this run must have `backend`.
pub fn required(backend: &str) -> bool {
    std::env::var(REQUIRED_ENV).is_ok_and(|v| names(&v, backend))
}

/// Report that `backend` is absent: prints `SKIP <backend>: <reason>` and
/// returns, so the test passes — or panics when this run requires it.
pub fn skip(backend: &str, reason: &str) {
    if required(backend) {
        panic!("{backend} is required here ({REQUIRED_ENV}) but absent: {reason}");
    }
    eprintln!("SKIP {backend}: {reason}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_backends_exactly() {
        assert!(names("1", "nvdec"));
        assert!(names(" all ", "nvdec"));
        assert!(names("openh264, media-foundation", "media-foundation"));
        assert!(!names(
            "openh264,media-foundation",
            "media-foundation-encoder"
        ));
        assert!(!names("", "openh264"));
        assert!(!names("0", "openh264"));
    }
}
