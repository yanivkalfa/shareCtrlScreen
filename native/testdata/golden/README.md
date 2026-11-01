# Golden encoder outputs

Short Annex-B streams as a host sends them, used to check every hardware
decoder bit-exactly: Linux (`crates/platform-linux/src/codec/golden.rs`) and macOS
VideoToolbox (`crates/platform-macos/src/decode/golden.rs`, checked by
`decode::tests::golden_streams_match_ffmpegs_frame_hashes`). The two loaders are twins;
keep them in step. Each
stream comes with:

* `<name>.aus` — one access unit per line: `<byte offset> <length> <keyframe 0|1>`.
* `<name>.sha256` — FFmpeg's per-frame SHA-256 of the decoded NV12 picture,
  the reference every decoder must reproduce (H.264 and HEVC decoding are
  bit-exact by specification):
  `ffmpeg -i <stream> -f framehash -hash sha256 -pix_fmt nv12 <name>.sha256`
  (FFmpeg 6.1.1, Ubuntu 24.04).

| File | Encoder | What it covers |
|---|---|---|
| `nvenc-h264-main-640x360.h264` | L3 NVENC (GTX 1080 Ti, driver 572.83, WSL2) | Main/CABAC, 1 reference, crop 368→360, forced IDR at frame 15, live bitrate change at 22 |
| `nvenc-hevc-main-640x360.hevc` | L3 NVENC, same machine | HEVC Main, same shape (NVDEC only; VA-API decodes H.264 only) |
| `openh264-cb-320x240.h264` | D2 Cisco OpenH264 2.6.0 (screen-content mode) | Constrained Baseline/CAVLC, a different size (resolution change when decoded after the NVENC stream) |
| `x264-high-refs4-cqm-352x288.h264` | FFmpeg 6.1.1 + libx264 (third-party reference) | High profile tools a host may use: 4 references, weighted P, 8x8 transform, JVT scaling matrices, 4 slices per picture, IDR every 10 frames, no B-frames |

The first three are written by
`cargo test -p platform-linux --all-features -- --ignored regenerate_golden_streams`
(NVIDIA driver + network for Cisco's download); the x264 clip by

```bash
ffmpeg -f lavfi -i testsrc2=size=352x288:rate=30 -frames:v 20 -pix_fmt yuv420p \
  -c:v libx264 -profile:v high -preset medium -bf 0 -refs 4 -g 10 -keyint_min 10 -sc_threshold 0 \
  -x264-params "cqm=jvt:slices=4:weightp=2:8x8dct=1" -bsf:v filter_units=remove_types=6 \
  -f h264 x264-high-refs4-cqm-352x288.h264
# ffprobe prints size before pos, whatever order the entries are named in.
ffprobe -v error -select_streams v -show_entries packet=pos,size,flags -of csv=p=0 \
  x264-high-refs4-cqm-352x288.h264 | awk -F, '{print $2, $1, (substr($3,1,1)=="K")}' \
  > x264-high-refs4-cqm-352x288.aus
```

Not here yet: Windows Media Foundation (W1) and VideoToolbox *encoder* (M2)
outputs — they belong to the cross-codec matrix (task I5). Any stream dropped
in with its `.aus` and `.sha256` is picked up by the decoder tests.
