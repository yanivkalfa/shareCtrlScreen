//! The cross-codec matrix (task I5): every encoder's output decodes on every
//! decoder, bit-exactly. Dev-only — decoder and encoder crates take it as a
//! `[dev-dependencies]` entry; nothing shipped links it.
//!
//! * [`golden`] — the committed streams in `native/testdata/golden` (one per
//!   encoder in the project, plus a third-party x264 reference) and
//!   [`golden::check_decoder`], the table-driven row each decoder runs: every
//!   stream of a codec it builds, every frame's NV12 SHA-256 against
//!   FFmpeg's.
//! * [`shape`] — the encoder-output conformance check (Annex-B, in-band
//!   parameter sets on every IDR, IDR first, no B slices, no reordering),
//!   run on every committed golden and on each encoder that can run.
//! * [`session`] — the scripted host session (forced keyframe, live bitrate
//!   drop) every encoder records, and the writer for new goldens.
//! * [`nal`] — the minimal H.264 / HEVC header parsing behind [`shape`].
//!
//! ## Skips and `SCT_CODEC_TESTS_REQUIRED`
//!
//! A decoder or encoder that is absent on the machine (no GPU, no network
//! for Cisco's OpenH264 download) makes its test print `SKIP <backend>:
//! <reason>` and pass — unless `SCT_CODEC_TESTS_REQUIRED` names that backend
//! (`1` or `all` names every backend; otherwise a comma-separated list of
//! [`skip`] backend names), in which case the skip is a failure. CI sets it
//! for the backends each runner is known to have (`native-ci.yml`).
//!
//! ## References
//!
//! * ITU-T H.264 (08/2021): Annex B (byte stream), §7.3.2.1.1 / §7.3.2.2 /
//!   §7.3.3 (SPS, PPS, slice header syntax), Table 7-1 (NAL unit types),
//!   Table 7-6 (slice_type), §8.2.1 (picture order count).
//! * ITU-T H.265 (09/2023): Annex B, §7.3.2.2 / §7.3.2.3 / §7.3.6.1 (SPS,
//!   PPS, slice segment header), §7.3.3 (profile_tier_level), §7.4.2.2
//!   (NAL unit types: IRAP, RADL/RASL, sub-layer non-reference), Table 7-7
//!   (slice_type), §8.3.1 (picture order count).
//! * FFmpeg `framehash` muxer (`libavformat/hashenc.c`; format version 2) —
//!   the reference hashes: <https://ffmpeg.org/ffmpeg-formats.html#framehash>.

pub mod golden;
pub mod nal;
pub mod session;
pub mod shape;
pub mod skip;
