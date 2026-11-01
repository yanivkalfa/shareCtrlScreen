//! Loader for the golden streams in `native/testdata/golden` (format and
//! provenance: its `README.md`). A twin of
//! `platform-linux/src/codec/golden.rs`: the two crates share no dev-only crate,
//! so the ~60 lines are kept in step by hand. Test-only.
//!
//! Each `<name>.<h264|hevc>` is raw Annex-B with `<name>.aus` (one access unit
//! per line: `<offset> <length> <keyframe 0|1>`) and `<name>.sha256` (FFmpeg's
//! per-frame SHA-256 of the decoded NV12 pictures).

use std::path::{Path, PathBuf};

use platform_api::Codec;

pub(super) fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden")
}

/// One golden stream, loaded.
pub(super) struct Golden {
    pub(super) name: String,
    pub(super) codec: Codec,
    pub(super) stream: Vec<u8>,
    /// `(offset, length, keyframe)` of each access unit.
    pub(super) aus: Vec<(usize, usize, bool)>,
    /// Expected `(width, height)` and SHA-256 (hex) of each output picture.
    pub(super) frames: Vec<((u32, u32), String)>,
}

impl Golden {
    pub(super) fn access_units(&self) -> impl Iterator<Item = (&[u8], bool)> + '_ {
        self.aus
            .iter()
            .map(|&(o, l, k)| (&self.stream[o..o + l], k))
    }
}

/// Every golden stream in the directory, sorted by name.
pub(super) fn all() -> Vec<Golden> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir())
        .expect("testdata/golden is readable")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("h264" | "hevc")
            )
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let codec = match path.extension().and_then(|e| e.to_str()) {
                Some("hevc") => Codec::Hevc,
                _ => Codec::H264,
            };
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("UTF-8 file name")
                .to_string();
            let stream = std::fs::read(&path).expect("golden stream");
            let aus_text =
                std::fs::read_to_string(path.with_extension("aus")).expect("golden .aus index");
            let aus = aus_text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    let f: Vec<usize> = l
                        .split_whitespace()
                        .map(|v| v.parse().expect("number in .aus"))
                        .collect();
                    assert_eq!(f.len(), 3, "{name}.aus line `{l}`");
                    assert!(f[0] + f[1] <= stream.len(), "{name}.aus past the stream");
                    (f[0], f[1], f[2] == 1)
                })
                .collect();
            let hash_text =
                std::fs::read_to_string(path.with_extension("sha256")).expect("golden .sha256");
            Golden {
                name,
                codec,
                stream,
                aus,
                frames: parse_framehash(&hash_text),
            }
        })
        .collect()
}

/// FFmpeg `framehash` output: a `#dimensions 0: WxH` header, then one
/// `stream, dts, pts, duration, size, hash` line per frame.
fn parse_framehash(text: &str) -> Vec<((u32, u32), String)> {
    let mut dims = (0, 0);
    let mut frames = Vec::new();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("#dimensions 0:") {
            let (w, h) = d.trim().split_once('x').expect("WxH");
            dims = (w.parse().expect("width"), h.parse().expect("height"));
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            let hash = line.rsplit(',').next().expect("hash field").trim();
            frames.push((dims, hash.to_string()));
        }
    }
    frames
}
