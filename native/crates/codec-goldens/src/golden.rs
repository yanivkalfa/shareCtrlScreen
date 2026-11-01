//! The golden streams in `native/testdata/golden` and the check every
//! decoder runs against them.
//!
//! Each stream `<name>.<h264|hevc>` (raw Annex-B, as a host sends it) comes
//! with `<name>.aus` — one access unit per line, `<offset> <length>
//! <keyframe 0|1>`, written by the encoder test that produced it — and
//! `<name>.sha256`, FFmpeg's per-frame SHA-256 of the decoded NV12 pictures
//! (`ffmpeg -i <stream> -f framehash -hash sha256 -pix_fmt nv12 <name>.sha256`).
//! H.264 and HEVC decoding are bit-exact by specification (H.264 §8, HEVC
//! §8: the decoding process is normative, output pictures included), so a
//! decoder must reproduce FFmpeg's hashes exactly: any parameter, reference
//! or crop mistake changes them. `testdata/golden/README.md` lists where each
//! file came from.

use std::path::{Path, PathBuf};

use platform_api::{Codec, CodecError, Frame, FrameData, PixelFormat, VideoDecoder};
use sha2::{Digest, Sha256};

/// `native/testdata/golden`.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden")
}

/// One output picture: its `(width, height)` and SHA-256 (hex) of its NV12
/// planes.
pub type Picture = ((u32, u32), String);

/// One golden stream, loaded.
#[derive(Debug, Clone)]
pub struct Golden {
    pub name: String,
    pub codec: Codec,
    pub stream: Vec<u8>,
    /// `(offset, length, keyframe)` of each access unit.
    pub aus: Vec<(usize, usize, bool)>,
    /// Expected `(width, height)` and SHA-256 (hex) of each output picture.
    pub frames: Vec<Picture>,
}

impl Golden {
    pub fn access_units(&self) -> impl Iterator<Item = (&[u8], bool)> + '_ {
        self.aus
            .iter()
            .map(|&(o, l, k)| (&self.stream[o..o + l], k))
    }
}

/// The codec of a golden stream file, by extension.
pub fn codec_of(path: &Path) -> Option<Codec> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("h264") => Some(Codec::H264),
        Some("hevc") => Some(Codec::Hevc),
        _ => None,
    }
}

/// Every stream file in the golden directory, sorted by name.
pub fn stream_paths() -> Result<Vec<PathBuf>, String> {
    let dir = dir();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if codec_of(&path).is_some() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Load one stream with its index and reference hashes.
pub fn load(path: &Path) -> Result<Golden, String> {
    let codec = codec_of(path).ok_or_else(|| format!("{}: not a stream", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{}: not a UTF-8 file name", path.display()))?
        .to_string();
    let stream = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut aus = Vec::new();
    for line in read_text(&path.with_extension("aus"))?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<usize> = line
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{name}.aus line `{line}`: {e}"))?;
        let [offset, length, key] = fields[..] else {
            return Err(format!("{name}.aus line `{line}`: three numbers expected"));
        };
        if offset + length > stream.len() {
            return Err(format!("{name}.aus line `{line}`: past the stream end"));
        }
        aus.push((offset, length, key == 1));
    }
    let frames = parse_framehash(&read_text(&path.with_extension("sha256"))?)
        .map_err(|e| format!("{name}.sha256: {e}"))?;
    Ok(Golden {
        name,
        codec,
        stream,
        aus,
        frames,
    })
}

/// Every golden stream, sorted by name. Panics with the reason when one
/// cannot be loaded: a broken fixture is a test failure, never a skip.
pub fn all() -> Vec<Golden> {
    let loaded = stream_paths().and_then(|paths| paths.iter().map(|p| load(p)).collect());
    match loaded {
        Ok(goldens) => goldens,
        Err(e) => panic!("golden streams: {e}"),
    }
}

/// FFmpeg `framehash` output: a `#dimensions 0: WxH` header, then one
/// `stream, dts, pts, duration, size, hash` line per frame.
pub fn parse_framehash(text: &str) -> Result<Vec<Picture>, String> {
    let mut dims = None;
    let mut frames = Vec::new();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("#dimensions 0:") {
            let parsed = d
                .trim()
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
            dims = Some(parsed.ok_or_else(|| format!("bad dimensions `{line}`"))?);
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            let size = dims.ok_or("a frame line before `#dimensions`")?;
            let hash = line.rsplit(',').next().unwrap_or_default().trim();
            if hash.len() != 64 {
                return Err(format!("bad hash in `{line}`"));
            }
            frames.push((size, hash.to_string()));
        }
    }
    Ok(frames)
}

/// SHA-256 of an NV12 picture laid out like FFmpeg's `nv12` rawvideo: `h`
/// Y rows of `w` bytes, then `ceil(h/2)` UV rows of `2·ceil(w/2)` bytes.
pub fn nv12_hash(
    w: usize,
    h: usize,
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
) -> Result<String, String> {
    let uv_row = w.div_ceil(2) * 2;
    let ch = h.div_ceil(2);
    if w == 0
        || h == 0
        || y_stride < w
        || uv_stride < uv_row
        || y.len() < y_stride * (h - 1) + w
        || uv.len() < uv_stride * (ch - 1) + uv_row
    {
        return Err(format!("NV12 planes too small for {w}x{h}"));
    }
    let mut hasher = Sha256::new();
    for r in 0..h {
        hasher.update(&y[r * y_stride..r * y_stride + w]);
    }
    for r in 0..ch {
        hasher.update(&uv[r * uv_stride..r * uv_stride + uv_row]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// [`nv12_hash`] of a CPU NV12 frame — what the OpenH264, NVDEC and VA-API
/// decoders hand out. GPU decoders read their surface back and call
/// [`nv12_hash`] themselves.
pub fn cpu_frame_hash(frame: &Frame<'_>) -> Result<String, String> {
    let FrameData::Cpu {
        format: PixelFormat::Nv12,
        plane0,
        plane1: Some(plane1),
    } = frame.data
    else {
        return Err(format!("not a CPU NV12 frame: {:?}", frame.data));
    };
    nv12_hash(
        frame.width as usize,
        frame.height as usize,
        plane0.data,
        plane0.stride,
        plane1.data,
        plane1.stride,
    )
}

/// One decoder in the matrix.
pub struct DecoderUnderTest<D, M, H>
where
    D: Fn(Codec) -> bool,
    M: Fn(Codec, u32, u32) -> Result<Box<dyn VideoDecoder>, CodecError>,
    H: FnMut(&Frame<'_>) -> Result<String, String>,
{
    /// Backend name, as `SCT_CODEC_TESTS_REQUIRED` lists it
    /// ([`crate::skip`]).
    pub name: &'static str,
    /// Which codecs this decoder builds on this machine.
    pub decodes: D,
    /// A fresh decoder (the `width`/`height` hint is 1920x1080, as a
    /// viewer that does not know the host's size yet passes).
    pub make: M,
    /// The SHA-256 of a decoded picture's NV12 planes.
    pub hash: H,
}

/// The matrix row of one decoder: decode every golden stream of a codec it
/// builds, assert one picture per access unit with FFmpeg's size and hash,
/// then decode two H.264 streams of different sizes back to back on one
/// decoder (a mid-session resolution change). Prints one `matrix:` line per
/// stream; returns the number of streams checked. Fails when no committed
/// stream is of a codec it decodes.
pub fn check_decoder<D, M, H>(mut dut: DecoderUnderTest<D, M, H>) -> usize
where
    D: Fn(Codec) -> bool,
    M: Fn(Codec, u32, u32) -> Result<Box<dyn VideoDecoder>, CodecError>,
    H: FnMut(&Frame<'_>) -> Result<String, String>,
{
    let goldens = all();
    assert!(
        !goldens.is_empty(),
        "no golden streams in {}",
        dir().display()
    );
    let mut checked = 0;
    for g in &goldens {
        if !(dut.decodes)(g.codec) {
            eprintln!(
                "matrix: {} × {}: SKIP (no {:?} decoder here)",
                dut.name, g.name, g.codec
            );
            continue;
        }
        let mut dec = make(&dut, g);
        let got = decode_all(dec.as_mut(), g, &mut dut.hash);
        compare(g, &got);
        eprintln!(
            "matrix: {} × {}: PASS ({} frames bit-exact with FFmpeg)",
            dut.name,
            g.name,
            got.len()
        );
        checked += 1;
    }
    assert!(checked > 0, "{}: no golden stream it decodes", dut.name);

    let h264: Vec<&Golden> = goldens.iter().filter(|g| g.codec == Codec::H264).collect();
    let pair = h264.iter().find_map(|a| {
        h264.iter()
            .find(|b| b.frames.first().map(|f| f.0) != a.frames.first().map(|f| f.0))
            .map(|b| (*a, *b))
    });
    if let (true, Some((a, b))) = ((dut.decodes)(Codec::H264), pair) {
        let mut dec = make(&dut, a);
        let first = decode_all(dec.as_mut(), a, &mut dut.hash);
        compare(a, &first);
        let second = decode_all(dec.as_mut(), b, &mut dut.hash);
        compare(b, &second);
        eprintln!(
            "matrix: {} × {} → {} on one decoder ({:?} → {:?}): PASS",
            dut.name, a.name, b.name, a.frames[0].0, b.frames[0].0
        );
    }
    checked
}

fn make<D, M, H>(dut: &DecoderUnderTest<D, M, H>, g: &Golden) -> Box<dyn VideoDecoder>
where
    D: Fn(Codec) -> bool,
    M: Fn(Codec, u32, u32) -> Result<Box<dyn VideoDecoder>, CodecError>,
    H: FnMut(&Frame<'_>) -> Result<String, String>,
{
    match (dut.make)(g.codec, 1920, 1080) {
        Ok(d) => d,
        Err(e) => panic!("{}: no {:?} decoder for {}: {e}", dut.name, g.codec, g.name),
    }
}

/// Decode `g`; each output's dimensions (as reported by both the frame and
/// `dimensions()`) and hash.
fn decode_all(
    dec: &mut dyn VideoDecoder,
    g: &Golden,
    hash: &mut impl FnMut(&Frame<'_>) -> Result<String, String>,
) -> Vec<Picture> {
    let mut out = Vec::new();
    for (i, (au, key)) in g.access_units().enumerate() {
        let ts = i as i64 * 333_333;
        let frame = match dec.decode(au, key, ts) {
            Ok(Some(f)) => f,
            Ok(None) => panic!(
                "{} AU {i}: no picture (every AU of a golden stream has one, \
                 and the decoders have no reorder delay)",
                g.name
            ),
            Err(e) => panic!("{} AU {i}: {e}", g.name),
        };
        let dims = (frame.width, frame.height);
        let digest = hash(&frame).unwrap_or_else(|e| panic!("{} AU {i}: hashing: {e}", g.name));
        assert_eq!(dec.dimensions(), dims, "{} AU {i}: dimensions()", g.name);
        out.push((dims, digest));
    }
    out
}

fn compare(g: &Golden, got: &[Picture]) {
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
    use crate::shape;

    #[test]
    fn framehash_parsing() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let text = format!(
            "#format: frame checksums\n#version: 2\n#hash: SHA256\n#tb 0: 1/25\n\
             #media_type 0: video\n#codec_id 0: rawvideo\n#dimensions 0: 640x360\n\
             #sar 0: 1/1\n#stream#, dts,        pts, duration,     size, hash\n\
             0,          0,          0,        1,   345600, {a}\n\
             0,          1,          1,        1,   345600, {b}\n"
        );
        assert_eq!(
            parse_framehash(&text),
            Ok(vec![((640, 360), a), ((640, 360), b)])
        );
        assert!(parse_framehash("0, 0, 0, 1, 1, abc\n").is_err());
    }

    #[test]
    fn nv12_hash_ignores_row_padding() {
        // 3x3: Y rows of 3 bytes, UV rows of 4 bytes (2 chroma pairs).
        let tight_y = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        let tight_uv = [10u8, 11, 12, 13, 14, 15, 16, 17];
        let padded_y = [1u8, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9];
        let padded_uv = [10u8, 11, 12, 13, 0, 0, 14, 15, 16, 17];
        let tight = nv12_hash(3, 3, &tight_y, 3, &tight_uv, 4).unwrap();
        assert_eq!(nv12_hash(3, 3, &padded_y, 4, &padded_uv, 6).unwrap(), tight);
        assert!(nv12_hash(3, 3, &tight_y[..8], 3, &tight_uv, 4).is_err());
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
            let mut end = 0;
            for &(o, l, _) in &g.aus {
                assert_eq!(o, end, "{}: AUs are contiguous", g.name);
                end = o + l;
            }
            assert_eq!(end, g.stream.len(), "{}: index covers the stream", g.name);
        }
    }

    /// Every committed stream has the shape the engine relies on — including
    /// the `.aus` keyframe flags, which the matrix feeds to each decoder.
    #[test]
    fn every_golden_has_the_engine_shape() {
        for g in all() {
            match shape::check(g.codec, g.access_units()) {
                Ok(s) => eprintln!("{}: {s:?}", g.name),
                Err(e) => panic!("{}: {e}", g.name),
            }
        }
    }

    /// Step two of making a golden (README): FFmpeg's per-frame hashes for
    /// every stream that has none yet. Never touches an existing `.sha256`.
    /// Run where FFmpeg is installed (the WSL2 Ubuntu here):
    /// `cargo test -p codec-goldens -- --ignored write_missing_ffmpeg_hashes --nocapture`
    #[test]
    #[ignore = "needs ffmpeg"]
    fn write_missing_ffmpeg_hashes() {
        let version = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .unwrap_or_else(|e| panic!("ffmpeg not runnable: {e}"));
        let version = String::from_utf8_lossy(&version.stdout);
        eprintln!(
            "{}",
            version.lines().next().unwrap_or("ffmpeg (no version line)")
        );
        let paths = stream_paths().unwrap_or_else(|e| panic!("{e}"));
        for path in paths {
            let out = path.with_extension("sha256");
            if out.exists() {
                continue;
            }
            let status = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-f", "framehash", "-hash", "sha256", "-pix_fmt", "nv12"])
                .arg(&out)
                .status()
                .unwrap_or_else(|e| panic!("ffmpeg: {e}"));
            assert!(status.success(), "ffmpeg failed on {}", path.display());
            let g = load(&path).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                g.frames.len(),
                g.aus.len(),
                "{}: FFmpeg frames vs AUs",
                g.name
            );
            eprintln!("wrote {} ({} frames)", out.display(), g.frames.len());
        }
    }
}
