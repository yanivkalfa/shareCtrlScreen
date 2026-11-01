//! The scripted host session every encoder records for its golden stream and
//! for the shape check: the same moving picture, a forced keyframe half-way
//! (a viewer joining, or a decoder asking for recovery) and a live bitrate
//! drop at three quarters (the transport's bandwidth estimate falling) —
//! what a host really sends in its first second.
//!
//! The script is the one task L3 used for the NVENC and OpenH264 goldens, so
//! all streams in `testdata/golden` are comparable.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use platform_api::{Codec, CodecError, EncodedUnit, Frame, VideoEncoder};

/// Frames per scripted session: one second at 30 fps, few enough to keep
/// each committed stream small.
pub const FRAMES: usize = 30;

/// The bitrate the session starts at and the one it drops to.
pub const START_BITRATE: u32 = 3_000_000;
pub const DROPPED_BITRATE: u32 = 1_000_000;

/// A smooth moving BGRA picture: horizontal and vertical ramps plus a block
/// that steps along a row, so every frame differs from the last.
pub fn pattern(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let block = (x / 32 + t).is_multiple_of(8) && y / 32 == 2;
            px[i] = (x * 255 / w) as u8;
            px[i + 1] = (y * 255 / h) as u8;
            px[i + 2] = if block {
                240
            } else {
                ((x + y) * 255 / (w + h)) as u8
            };
            px[i + 3] = 255;
        }
    }
    px
}

/// What one scripted session produced.
#[derive(Debug, Clone)]
pub struct Session {
    pub units: Vec<EncodedUnit>,
    /// Frames fed in.
    pub frames: usize,
    /// The frame `force_keyframe` was called before.
    pub forced_keyframe: usize,
}

/// Run the script on `enc`. `encode` hands each frame — CPU BGRA
/// `width`×`height`, [`pattern`] — to the encoder: `|e, f| e.encode(f)` for
/// CPU encoders; GPU encoders upload it first.
pub fn record<E: VideoEncoder + ?Sized>(
    enc: &mut E,
    width: u32,
    height: u32,
    frames: usize,
    mut encode: impl FnMut(&mut E, &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError>,
) -> Result<Session, CodecError> {
    let (w, h) = (width as usize, height as usize);
    let forced_keyframe = frames / 2;
    let mut units = Vec::new();
    for t in 0..frames {
        if t == forced_keyframe {
            enc.force_keyframe();
        }
        if t == frames * 3 / 4 {
            enc.set_bitrate(DROPPED_BITRATE)?;
        }
        let px = pattern(w, h, t);
        units.extend(encode(enc, &Frame::bgra(width, height, &px, w * 4))?);
    }
    Ok(Session {
        units,
        frames,
        forced_keyframe,
    })
}

/// [`record`] for encoders that take CPU frames.
pub fn record_cpu(
    enc: &mut dyn VideoEncoder,
    width: u32,
    height: u32,
    frames: usize,
) -> Result<Session, CodecError> {
    record(enc, width, height, frames, |e, f| e.encode(f))
}

/// The file extension of a golden stream.
pub fn extension(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "obu",
    }
}

/// Write `units` as golden stream `<name>.<ext>` plus its `.aus` index into
/// `dir`. The `.sha256` reference is FFmpeg's to write (README).
///
/// Refuses to replace an existing file: committed goldens are never
/// rewritten (binary history stays small, and a decoder test that passed
/// against a stream keeps meaning the same thing). A deliberate replacement
/// deletes the old files first and gets a new name.
pub fn write(dir: &Path, name: &str, codec: Codec, units: &[EncodedUnit]) -> std::io::Result<()> {
    let mut stream = Vec::new();
    let mut index = String::new();
    for u in units {
        // Infallible: writing to a String.
        let _ = writeln!(
            index,
            "{} {} {}",
            stream.len(),
            u.data.len(),
            u8::from(u.keyframe)
        );
        stream.extend_from_slice(&u.data);
    }
    for (ext, bytes) in [(extension(codec), &stream[..]), ("aus", index.as_bytes())] {
        let path = dir.join(format!("{name}.{ext}"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        file.write_all(bytes)?;
    }
    eprintln!(
        "wrote {name}.{}: {} access units, {} bytes",
        extension(codec),
        units.len(),
        stream.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockCodecFactory;
    use platform_api::{CodecFactory, EncoderConfig};

    #[test]
    fn the_script_forces_a_keyframe_half_way() {
        let codecs = MockCodecFactory;
        let mut enc = codecs
            .new_encoder(
                None,
                EncoderConfig {
                    width: 64,
                    height: 32,
                    ..EncoderConfig::default()
                },
            )
            .unwrap();
        let s = record_cpu(enc.as_mut(), 64, 32, 8).unwrap();
        assert_eq!((s.frames, s.forced_keyframe), (8, 4));
        let keys: Vec<usize> = (0..s.units.len())
            .filter(|&i| s.units[i].keyframe)
            .collect();
        assert_eq!(keys, vec![0, 4]);
    }

    #[test]
    fn write_refuses_to_replace_a_golden() {
        let dir = std::env::temp_dir().join(format!("sct-goldens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let units = [
            EncodedUnit {
                data: vec![0, 0, 0, 1, 0x65],
                keyframe: true,
            },
            EncodedUnit {
                data: vec![0, 0, 1, 0x41],
                keyframe: false,
            },
        ];
        write(&dir, "t", Codec::H264, &units).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("t.aus")).unwrap(),
            "0 5 1\n5 4 0\n"
        );
        assert_eq!(std::fs::read(dir.join("t.h264")).unwrap().len(), 9);
        let again = write(&dir, "t", Codec::H264, &units).unwrap_err();
        assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
