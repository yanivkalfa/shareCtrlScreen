# Golden encoder outputs

Short Annex-B streams as a host sends them, used for the cross-codec matrix
(task I5): **every encoder's output decodes bit-exactly on every decoder**.
One loader and one checker serve every decoder crate:
`crates/codec-goldens` (dev-only). Each stream comes with:

* `<name>.aus` — one access unit per line: `<byte offset> <length> <keyframe 0|1>`,
  written by the encoder test that produced the stream.
* `<name>.sha256` — FFmpeg's per-frame SHA-256 of the decoded NV12 picture,
  the reference every decoder must reproduce (H.264 and HEVC decoding are
  bit-exact by specification):
  `ffmpeg -i <stream> -f framehash -hash sha256 -pix_fmt nv12 <name>.sha256`
  (FFmpeg 6.1.1, Ubuntu 24.04).

Every committed stream also passes the encoder-shape check
(`codec_goldens::shape`, test `every_golden_has_the_engine_shape`): Annex-B,
IDR first, the keyframe flag on exactly the IDRs, SPS/PPS (and VPS) in front
of every IDR, no reordering.

## Streams

| File | Encoder | What it covers |
|---|---|---|
| `nvenc-h264-main-640x360.h264` | L3 NVENC (GTX 1080 Ti, driver 572.83, WSL2) | Main/CABAC, 1 reference, crop 368→360, forced IDR at frame 15, live bitrate change at 22 |
| `nvenc-hevc-main-640x360.hevc` | L3 NVENC, same machine | HEVC Main, same shape |
| `openh264-cb-320x240.h264` | D2 Cisco OpenH264 2.6.0 (screen-content mode) | Constrained Baseline/CAVLC, a different size (resolution change when decoded after the NVENC stream) |
| `x264-high-refs4-cqm-352x288.h264` | FFmpeg 6.1.1 + libx264 (third-party reference) | High profile tools a host may use: 4 references, weighted P, 8x8 transform, JVT scaling matrices, 4 slices per picture, IDR every 10 frames, no B-frames |

Not here yet (device-pending; the decoder tests pick each stream up as
soon as its three files are dropped in):

* **Media Foundation hardware encoder output (W1)**: `mf-h264-640x360`,
  `mf-h264-480x270`, `mf-hevc-640x360`. They must come from a hardware
  MFT. On machine A both NVIDIA MFTs fail to open (`E_UNEXPECTED`,
  0x8000FFFF) and Media Foundation falls back to Microsoft's software
  encoder, so the regenerate test refuses to write them there. Record them
  on another Windows machine: plans/checklists/I5.md §1.
* **VideoToolbox encoder output (M2)**: `vt-h264-640x360`,
  `vt-h264-480x270`, `vt-hevc-640x360`. These need a Mac:
  plans/checklists/I5.md §2.

## Making a golden: two steps

The `.sha256` must come from FFmpeg, not from any decoder under test.

1. On the machine with the encoder, record the scripted session
   (`codec_goldens::session`: 30 frames of the same moving picture, a forced
   IDR at frame 15, a live bitrate drop from 3 to 1 Mbit/s at frame 22). This
   writes `<name>.<h264|hevc>` and `<name>.aus`:
   * NVENC + OpenH264 (WSL2 with the NVIDIA driver, network for Cisco's
     download):
     `cargo test -p platform-linux --all-features -- --ignored regenerate_golden_streams`
   * Media Foundation (Windows, hardware MFT):
     `cargo test -p platform-windows -- --ignored regenerate_media_foundation_goldens --nocapture`
   * VideoToolbox (a Mac):
     `cargo test -p platform-macos --test vt_goldens -- --ignored regenerate_videotoolbox_goldens --nocapture`
2. Where FFmpeg is installed (the WSL2 Ubuntu here), write the hashes of
   every stream that has none yet:
   `cargo test -p codec-goldens -- --ignored write_missing_ffmpeg_hashes --nocapture`

The writer refuses to replace an existing file, and the hash step never
touches an existing `.sha256`. Committed goldens are never rewritten: a new
recording gets a new name.

The x264 clip was made with FFmpeg directly:

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

## Decoders and how each is hashed

Every decoder below gives the decoded samples unconverted, so all of them
are compared bit-exactly. None needs the PSNR fallback (≥ 45 dB against
FFmpeg's frames), which is reserved for a decoder whose pipeline converts
colour.

| Decoder | Test | Codecs checked | Hashed from |
|---|---|---|---|
| Media Foundation (hardware MFT, else Microsoft's software decoder) | `platform-windows` `golden_tests::media_foundation_decodes_the_golden_streams` | what the viewer advertises: H.264 always, HEVC only with a hardware HEVC MFT | the D3D11 NV12 texture, copied to a staging texture |
| Cisco OpenH264 | `codec-openh264` `tests/goldens.rs` | H.264 | CPU NV12 (OpenH264's I420 interleaved, samples unchanged) |
| NVDEC | `platform-linux` `codec::nvdec::tests::nvdec_decodes_the_golden_streams` | H.264, HEVC | CPU NV12 |
| VA-API | `platform-linux` `codec::vaapi::decode::tests::vaapi_decodes_the_golden_streams` (device-only); the driver-free planner runs every H.264 golden on CI | H.264 | CPU NV12 |
| VideoToolbox | `platform-macos` `decode::tests::golden_streams_match_ffmpegs_frame_hashes` (task T2's macOS CI step) | H.264, and HEVC with a hardware decoder | the IOSurface `420v` planes |

The software HEVC MFT (HEVC Video Extensions) is deliberately left out:
the viewer never advertises it (not real-time), and it holds the first
picture back, which a low-latency viewer cannot use.
