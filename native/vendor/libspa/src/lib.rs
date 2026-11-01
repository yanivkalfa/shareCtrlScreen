// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

//! The `libspa` crate provides a high-level API to interact with
//! [libspa].
//!
//! [libspa]: https://docs.pipewire.org/page_spa.html
//!
//! ## ShareCtrlScreen patch
//!
//! This is libspa 0.10.1 from crates.io (MIT), vendored by ShareCtrlScreen and applied
//! through `[patch.crates-io]` in `native/Cargo.toml`, so that it builds against the
//! PipeWire 0.3.48 headers of Ubuntu 22.04, the oldest Linux the app supports. Upstream
//! 0.10.1 (and every release back to 0.8) uses three header symbols unconditionally
//! that 0.3.48 does not have in the form bindgen can bind. Each patched site is marked
//! "ShareCtrlScreen patch"; nothing else differs from the published crate.
//!
//! - `buffer::meta::MetaRegion::is_valid`: the `spa_meta_region_is_valid` expression in
//!   Rust (no gate needed, same result on every version).
//! - `buffer::meta::VideoDamageIter`: `spa_meta_first`/`spa_meta_check` in Rust, with the
//!   bounds check one region wide, as `spa_meta_for_each` uses it.
//! - `param::video::VideoInfoRaw`: built without naming `flags`, `flags`/`set_flags`
//!   behind the existing `v0_3_65` feature, `modifier` cast so both signednesses build.
//!
//! ## References
//!
//! PipeWire repository: <https://gitlab.freedesktop.org/pipewire/pipewire>
//!
//! - `spa_meta_region_is_valid`: macro up to 0.3.58; `static inline` from 0.3.59,
//!   commit 671a7102ff079f75fd7c47b9e9043f625979963b "Fix some badly-behaved macros".
//! - `spa_meta_first`: macro up to 0.3.58; `static inline` from 0.3.59,
//!   commit 90c955c22397b9679da2e11cb33ad6c0b866f9d8 "Replace more macros with accessor
//!   functions". `spa_meta_check` is a macro in every version.
//! - `spa_video_info_raw.flags` and `modifier` as `uint64_t` (was `int64_t`): 0.3.65,
//!   commit c61b9a09b8bd75e41c97d44d5bc8ce20ee5cfa4d "video: Add SPA_VIDEO_FLAG_MODIFIER
//!   flag".

pub mod buffer;
pub mod constants;
pub mod node;
pub mod param;
pub mod pod;
pub mod support;
pub mod utils;

pub use spa_sys as sys;
