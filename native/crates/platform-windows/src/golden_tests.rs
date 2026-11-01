//! The Media Foundation rows of the cross-codec matrix (task I5): the MF
//! decoder against every committed golden stream, the MF encoder's output
//! shape, and the regeneration of the MF goldens.
//!
//! The MF decoder hands out D3D11 NV12 textures (a slice of the decoder's
//! texture array on the hardware path, an uploaded texture on the software
//! path). Both hold the decoded samples untouched — NV12 is the decoder's
//! output format, no colour conversion runs — so a staging copy of the
//! texture is hashed exactly like FFmpeg's `nv12` rawvideo.
//!
//! Run with the rest of the crate's tests (`cargo test -p platform-windows`);
//! the regeneration is `#[ignore]`d, see [`regenerate_media_foundation_goldens`].
//!
//! ## References
//!
//! * `CODECAPI_AVEncVideoForceKeyFrame` (dynamic, next frame an IDR),
//!   `CODECAPI_AVEncCommonMeanBitRate` (dynamic in CBR):
//!   <https://learn.microsoft.com/windows/win32/directshow/avencvideoforcekeyframe-property>,
//!   <https://learn.microsoft.com/windows/win32/directshow/avenccommonmeanbitrate-property>
//! * H.264 encoder MFT output (Annex-B, SPS/PPS in the stream, also exposed
//!   as `MF_MT_MPEG_SEQUENCE_HEADER`):
//!   <https://learn.microsoft.com/windows/win32/medfound/h-264-video-encoder>
//! * `ID3D11DeviceContext::CopySubresourceRegion` and NV12 subresource
//!   layout (UV plane after `RowPitch × Height` luma bytes when mapped):
//!   <https://learn.microsoft.com/windows/win32/api/d3d11/nf-d3d11-id3d11devicecontext-copysubresourceregion>,
//!   <https://learn.microsoft.com/windows/win32/medfound/recommended-8-bit-yuv-formats-for-video-rendering#nv12>
//! * `D3D_DRIVER_TYPE_WARP` (the software rasterizer a GPU-less CI runner
//!   has): <https://learn.microsoft.com/windows/win32/direct3darticles/directx-warp>

use codec_goldens::golden::{check_decoder, nv12_hash, DecoderUnderTest};
use codec_goldens::session::{self, Session, FRAMES, START_BITRATE};
use codec_goldens::{shape, skip};
use platform_api::{
    Codec, CodecError, CodecFactory, EncoderConfig, Frame, FrameData, VideoDecoder, VideoEncoder,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

use crate::adapters::{mf_encoder, MfCodecFactory, MfEncoder};
use crate::gpu::{D3d11Device, D3d11Texture};

/// The codec crate's log in the test output: it says why a hardware MFT was
/// passed over (`Encoder::new` falls back to software with a warning).
fn logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("codec=info")
        .with_test_writer()
        .try_init();
}

fn to_mf(c: Codec) -> codec::Codec {
    match c {
        Codec::H264 => codec::Codec::H264,
        Codec::Hevc => codec::Codec::Hevc,
        Codec::Av1 => codec::Codec::Av1,
    }
}

/// A D3D11 device for the codecs: the GPU when there is one, else WARP (a
/// CI runner has no GPU; Microsoft's software decoders only need a device
/// for their upload textures). The video-support flag first, because the
/// encoder's BGRA→NV12 video processor and hardware decoders need it.
fn device() -> Result<(D3d11Device, &'static str), String> {
    let tries: [(D3D_DRIVER_TYPE, D3D11_CREATE_DEVICE_FLAG, &str); 3] = [
        (
            D3D_DRIVER_TYPE_HARDWARE,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            "hardware",
        ),
        (
            D3D_DRIVER_TYPE_WARP,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            "WARP with video support",
        ),
        (
            D3D_DRIVER_TYPE_WARP,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            "WARP",
        ),
    ];
    let mut errors = Vec::new();
    for (driver, flags, what) in tries {
        let mut device = None;
        // SAFETY: standard device creation; `device` is the out-parameter.
        let made = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        match (made, device) {
            (Ok(()), Some(d)) => return Ok((D3d11Device(d), what)),
            (Err(e), _) => errors.push(format!("{what}: {e}")),
            (Ok(()), None) => errors.push(format!("{what}: no device returned")),
        }
    }
    Err(format!("no D3D11 device ({})", errors.join("; ")))
}

/// A BGRA texture shaped like a Desktop Duplication frame (render target +
/// shader resource), holding `bgra` (tight rows).
fn bgra_texture(
    dev: &ID3D11Device,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<D3d11Texture, CodecError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: bgra.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `init` points at `bgra`, which outlives the call and holds
    // `height` rows of `width * 4` bytes (checked by the caller's frame).
    unsafe { dev.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
        .map_err(|e| CodecError::Backend(format!("input texture: {e}")))?;
    let texture =
        texture.ok_or_else(|| CodecError::Backend("input texture: none returned".into()))?;
    Ok(D3d11Texture {
        texture,
        array_index: 0,
    })
}

/// Hand a session's CPU BGRA frame to the MF encoder the way capture does:
/// as a D3D11 texture.
fn encode_on_gpu(
    dev: &ID3D11Device,
    enc: &mut MfEncoder,
    frame: &Frame<'_>,
) -> Result<Vec<platform_api::EncodedUnit>, CodecError> {
    let FrameData::Cpu { plane0, .. } = frame.data else {
        return Err(CodecError::Unsupported("the session feeds CPU BGRA".into()));
    };
    let tex = bgra_texture(dev, frame.width, frame.height, plane0.data)?;
    enc.encode(&Frame {
        width: frame.width,
        height: frame.height,
        data: FrameData::Native(&tex),
    })
}

/// SHA-256 of a decoded D3D11 NV12 frame's visible `width`×`height`.
fn texture_hash(dev: &ID3D11Device, frame: &Frame<'_>) -> Result<String, String> {
    let FrameData::Native(any) = frame.data else {
        return Err(format!("the MF decoder returned {:?}", frame.data));
    };
    let src = any
        .downcast_ref::<D3d11Texture>()
        .ok_or("the MF decoder returned a non-D3D11 frame")?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; `desc` is a plain out-struct.
    unsafe { src.texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_NV12 {
        return Err(format!("decoded texture is DXGI format {}", desc.Format.0));
    }
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    // SAFETY: valid device and descriptor.
    unsafe { dev.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|e| format!("staging texture: {e}"))?;
    let staging = staging.ok_or("staging texture: none returned")?;
    // SAFETY: a live device always has an immediate context.
    let ctx = unsafe { dev.GetImmediateContext() }.map_err(|e| format!("context: {e}"))?;
    // Subresource of slice `array_index`, mip 0 (D3D11CalcSubresource).
    let subresource = src.array_index * desc.MipLevels.max(1);
    // SAFETY: GPU copy into our staging texture, then a CPU map released
    // before returning. A mapped NV12 surface is `RowPitch × Height` luma
    // bytes followed by `RowPitch × Height/2` interleaved chroma bytes, and
    // the slices below stay inside that.
    unsafe {
        ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, &src.texture, subresource, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("map: {e}"))?;
        let pitch = mapped.RowPitch as usize;
        let rows = desc.Height as usize;
        let base = mapped.pData as *const u8;
        let y = std::slice::from_raw_parts(base, pitch * rows);
        let uv = std::slice::from_raw_parts(base.add(pitch * rows), pitch * rows.div_ceil(2));
        let hash = nv12_hash(
            frame.width as usize,
            frame.height as usize,
            y,
            pitch,
            uv,
            pitch,
        );
        ctx.Unmap(&staging, 0);
        hash
    }
}

/// The Media Foundation decoder row of the matrix: every golden stream of a
/// codec Media Foundation has a decoder for here (hardware first, else the
/// Microsoft software H.264 decoder and, where the HEVC Video Extensions are
/// installed, the HEVC one), every frame bit-exact with FFmpeg. Prints
/// `SKIP media-foundation` and passes when there is no D3D11 device or no
/// MF decoder at all, unless `SCT_CODEC_TESTS_REQUIRED` names
/// `media-foundation`.
#[test]
fn media_foundation_decodes_the_golden_streams() {
    let (dev, kind) = match device() {
        Ok(d) => d,
        Err(e) => return skip::skip("media-foundation", &e),
    };
    logs();
    if !codec::decode::can_decode(codec::Codec::H264) {
        return skip::skip("media-foundation", "no H.264 decoder MFT");
    }
    // What the viewer advertises (`viewer_decodable`): H.264 always, HEVC
    // only with a hardware decoder MFT. The software HEVC MFT (HEVC Video
    // Extensions) is deliberately not advertised — it is not real-time —
    // and is not tested here: it also holds the first picture back, which
    // the low-latency viewer cannot use.
    let advertised = MfCodecFactory.viewer_decodable();
    for c in [Codec::H264, Codec::Hevc] {
        eprintln!(
            "Media Foundation {c:?} decoder: {} (D3D11 device: {kind})",
            if codec::decode::can_decode_hw(to_mf(c)) {
                "hardware MFT"
            } else if advertised.contains(&c) {
                "software MFT"
            } else {
                "not advertised"
            }
        );
    }
    let decodes = |c: Codec| advertised.contains(&c);
    check_decoder(DecoderUnderTest {
        name: "media-foundation",
        decodes,
        make: |c, w, h| -> Result<Box<dyn VideoDecoder>, CodecError> {
            MfCodecFactory.new_decoder(Some(&dev as &dyn std::any::Any), c, w, h)
        },
        hash: |f: &Frame<'_>| texture_hash(&dev.0, f),
    });
}

fn session_config(codec: Codec, width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps_num: 30,
        fps_den: 1,
        bitrate_bps: START_BITRATE,
        codec,
    }
}

/// Record the scripted session on a built Media Foundation encoder. Any
/// failure here is the encoder breaking mid-session, never an absent one.
fn record_mf_session(
    dev: &D3d11Device,
    enc: &mut MfEncoder,
    width: u32,
    height: u32,
) -> Result<Session, CodecError> {
    session::record(enc, width, height, FRAMES, |e, f| {
        encode_on_gpu(&dev.0, e, f)
    })
}

/// Encoder-output conformance (task I5) for Media Foundation: the scripted
/// session (forced keyframe, live bitrate drop) has the engine's shape —
/// H.264 on whatever encoder MF builds (hardware MFT, else Microsoft's
/// software one), HEVC where a hardware HEVC MFT exists. Prints `SKIP
/// media-foundation-encoder` only when there is no device or the encoder
/// cannot be built (no MFT, no video support on the device); once an
/// encoder exists, any failure during the session fails the test.
#[test]
fn media_foundation_output_has_the_engine_shape() {
    logs();
    let (dev, kind) = match device() {
        Ok(d) => d,
        Err(e) => return skip::skip("media-foundation-encoder", &e),
    };
    let mut codecs = vec![Codec::H264];
    if codec::encode::can_encode(codec::Codec::Hevc) {
        codecs.push(Codec::Hevc);
    }
    for c in codecs {
        let mut enc = match mf_encoder(&dev, session_config(c, 640, 360)) {
            Ok(e) => e,
            Err(e) => {
                skip::skip(
                    "media-foundation-encoder",
                    &format!("{c:?} on the {kind} device: {e}"),
                );
                continue;
            }
        };
        let which = if enc.is_hardware() {
            "hardware"
        } else {
            "software"
        };
        let s = record_mf_session(&dev, &mut enc, 640, 360)
            .unwrap_or_else(|e| panic!("Media Foundation {c:?} ({which}) session: {e}"));
        match shape::check_session(c, &s) {
            Ok(shape) => eprintln!("Media Foundation {c:?} ({which}): {shape:?}"),
            Err(e) => panic!("Media Foundation {c:?} ({which}): {e}"),
        }
    }
}

/// Step one of the Media Foundation goldens (README): records the scripted
/// session on this machine's **hardware** MF encoder and writes
/// `mf-h264-640x360.h264`, `mf-h264-480x270.h264` (a second size: the MF
/// encoder cannot change size within a session, so the matrix gets its
/// resolution change from this pair) and, where a hardware HEVC MFT exists,
/// `mf-hevc-640x360.hevc`, each with its `.aus`, into `testdata/golden`.
/// Fails rather than write a golden from Microsoft's software encoder under
/// an `mf-` hardware name. Existing files are never replaced.
///
/// ```text
/// cargo test -p platform-windows -- --ignored regenerate_media_foundation_goldens --nocapture
/// ```
///
/// Step two writes the `.sha256` with FFmpeg (WSL2 here):
/// `cargo test -p codec-goldens -- --ignored write_missing_ffmpeg_hashes`.
#[test]
#[ignore = "needs a Media Foundation hardware encoder"]
fn regenerate_media_foundation_goldens() {
    logs();
    let (dev, kind) = device().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(kind, "hardware", "the goldens come from a GPU encoder");
    let dir = codec_goldens::golden::dir();
    let mut streams = vec![
        ("mf-h264-640x360", Codec::H264, 640, 360),
        ("mf-h264-480x270", Codec::H264, 480, 270),
    ];
    if codec::encode::can_encode(codec::Codec::Hevc) {
        streams.push(("mf-hevc-640x360", Codec::Hevc, 640, 360));
    } else {
        eprintln!("no hardware HEVC MFT here: no mf-hevc golden");
    }
    for (name, c, w, h) in streams {
        let mut enc =
            mf_encoder(&dev, session_config(c, w, h)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            enc.is_hardware(),
            "{name}: Media Foundation fell back to the software encoder; \
             a golden named mf-* must come from the hardware MFT"
        );
        let s = record_mf_session(&dev, &mut enc, w, h).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Err(e) = shape::check_session(c, &s) {
            panic!("{name}: {e}");
        }
        session::write(&dir, name, c, &s.units).unwrap_or_else(|e| panic!("{e}"));
    }
}
