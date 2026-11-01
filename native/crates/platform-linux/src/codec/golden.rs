//! Golden encoder outputs (`native/testdata/golden`) and the check every
//! hardware decoder runs against them.
//!
//! Each stream `<name>.<h264|hevc>` (raw Annex-B, as a host sends it) comes
//! with `<name>.aus` — one access unit per line, `<offset> <length>
//! <keyframe 0|1>`, written by the encoder test that produced it — and
//! `<name>.sha256`, FFmpeg's per-frame SHA-256 of the decoded NV12 pictures
//! (`ffmpeg -i <stream> -f framehash -hash sha256 -pix_fmt nv12 <name>.sha256`).
//! H.264 and HEVC decoding are bit-exact by specification, so a hardware
//! decoder must reproduce FFmpeg's hashes exactly: any parameter, reference
//! or crop mistake changes them. `testdata/golden/README.md` lists where each
//! file came from.

use std::path::{Path, PathBuf};

use platform_api::{Codec, CodecError, FrameData, PixelFormat, VideoDecoder};
use sha2::{Digest, Sha256};

pub(crate) fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden")
}

/// One golden stream, loaded.
pub(crate) struct Golden {
    pub name: String,
    pub codec: Codec,
    pub stream: Vec<u8>,
    /// `(offset, length, keyframe)` of each access unit.
    pub aus: Vec<(usize, usize, bool)>,
    /// Expected `(width, height)` and SHA-256 (hex) of each output picture.
    pub frames: Vec<((u32, u32), String)>,
}

impl Golden {
    pub(crate) fn access_units(&self) -> impl Iterator<Item = (&[u8], bool)> + '_ {
        self.aus
            .iter()
            .map(|&(o, l, k)| (&self.stream[o..o + l], k))
    }
}

/// Every golden stream in the directory, sorted by name.
pub(crate) fn all() -> Vec<Golden> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir()).expect("testdata/golden is readable");
    let mut paths: Vec<PathBuf> = entries
        .map(|e| e.expect("directory entry").path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("h264" | "hevc")
            )
        })
        .collect();
    paths.sort();
    for path in paths {
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
        let frames = parse_framehash(&hash_text);
        out.push(Golden {
            name,
            codec,
            stream,
            aus,
            frames,
        });
    }
    out
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

/// SHA-256 of a decoded frame laid out like FFmpeg's `nv12` rawvideo: Y rows
/// of `width` bytes, then UV rows of `width` (even sizes) bytes.
pub(crate) fn frame_hash(frame: &platform_api::Frame<'_>) -> String {
    let FrameData::Cpu {
        format: PixelFormat::Nv12,
        plane0,
        plane1: Some(plane1),
    } = frame.data
    else {
        panic!("decoders hand out CPU NV12 frames");
    };
    let (w, h) = (frame.width as usize, frame.height as usize);
    let mut hasher = Sha256::new();
    for r in 0..h {
        hasher.update(&plane0.data[r * plane0.stride..r * plane0.stride + w]);
    }
    let uv_row = w.div_ceil(2) * 2;
    for r in 0..h.div_ceil(2) {
        hasher.update(&plane1.data[r * plane1.stride..r * plane1.stride + uv_row]);
    }
    hex::encode(hasher.finalize())
}

/// Decode every golden stream `decodes` accepts with decoders from `make`,
/// asserting FFmpeg's pictures come out, one per access unit; then decode two
/// H.264 streams of different sizes back to back on one decoder (a
/// mid-stream resolution change).
pub(crate) fn check_decoder(
    decodes: impl Fn(Codec) -> bool,
    make: impl Fn(Codec, u32, u32) -> Result<Box<dyn VideoDecoder>, CodecError>,
) {
    let goldens = all();
    assert!(
        !goldens.is_empty(),
        "no golden streams in {}",
        dir().display()
    );
    let mut checked = 0;
    for g in goldens.iter().filter(|g| decodes(g.codec)) {
        let mut dec = make(g.codec, 1920, 1080).expect("decoder");
        let got = decode_all(dec.as_mut(), g);
        compare(g, &got);
        eprintln!("{}: {} frames match FFmpeg", g.name, got.len());
        checked += 1;
    }
    assert!(checked > 0, "no golden stream this decoder takes");

    let h264: Vec<&Golden> = goldens.iter().filter(|g| g.codec == Codec::H264).collect();
    let pair = h264.iter().find_map(|a| {
        h264.iter()
            .find(|b| b.frames.first().map(|f| f.0) != a.frames.first().map(|f| f.0))
            .map(|b| (*a, *b))
    });
    if let (true, Some((a, b))) = (decodes(Codec::H264), pair) {
        let mut dec = make(Codec::H264, 1920, 1080).expect("decoder");
        let first = decode_all(dec.as_mut(), a);
        compare(a, &first);
        let second = decode_all(dec.as_mut(), b);
        compare(b, &second);
        eprintln!(
            "resolution change {:?} → {:?} on one decoder: ok",
            a.frames[0].0, b.frames[0].0
        );
    }
}

/// Decode `g`; each output's dimensions (as reported by both the frame and
/// `dimensions()`) and hash.
fn decode_all(dec: &mut dyn VideoDecoder, g: &Golden) -> Vec<((u32, u32), String)> {
    let mut out = Vec::new();
    for (i, (au, key)) in g.access_units().enumerate() {
        let ts = i as i64 * 166_667;
        let frame = dec
            .decode(au, key, ts)
            .unwrap_or_else(|e| panic!("{} AU {i}: {e}", g.name));
        let Some(frame) = frame else {
            panic!(
                "{} AU {i}: no picture (every AU of a golden stream has one)",
                g.name
            );
        };
        let dims = (frame.width, frame.height);
        let hash = frame_hash(&frame);
        assert_eq!(dec.dimensions(), dims, "{} AU {i}: dimensions()", g.name);
        out.push((dims, hash));
    }
    out
}

fn compare(g: &Golden, got: &[((u32, u32), String)]) {
    assert_eq!(got.len(), g.frames.len(), "{}: frame count", g.name);
    for (i, (got, want)) in got.iter().zip(&g.frames).enumerate() {
        assert_eq!(got.0, want.0, "{} frame {i}: size", g.name);
        assert_eq!(
            got.1, want.1,
            "{} frame {i}: pixels differ from FFmpeg's",
            g.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framehash_parsing() {
        let text = "#format: frame checksums\n#version: 2\n#hash: SHA256\n#tb 0: 1/25\n\
                    #media_type 0: video\n#codec_id 0: rawvideo\n#dimensions 0: 640x360\n\
                    #sar 0: 1/1\n#stream#, dts,        pts, duration,     size, hash\n\
                    0,          0,          0,        1,   345600, abcd\n\
                    0,          1,          1,        1,   345600, ef01\n";
        assert_eq!(
            parse_framehash(text),
            vec![((640, 360), "abcd".into()), ((640, 360), "ef01".into())]
        );
    }

    #[test]
    fn goldens_are_consistent() {
        for g in all() {
            assert_eq!(
                g.aus.len(),
                g.frames.len(),
                "{}: one picture per AU",
                g.name
            );
            assert!(
                g.aus.first().is_some_and(|a| a.2),
                "{}: starts on a keyframe",
                g.name
            );
            let mut end = 0;
            for &(o, l, _) in &g.aus {
                assert_eq!(o, end, "{}: AUs are contiguous", g.name);
                end = o + l;
            }
            assert_eq!(end, g.stream.len(), "{}: index covers the stream", g.name);
        }
    }
}
