//! Hardware video decode (Plan 04 §5c). The tightest option is raw D3D11VA
//! (`ID3D11VideoDevice` → `CreateVideoDecoder` → per-frame
//! `DecoderBeginFrame`/`SubmitDecoderBuffers`/`DecoderEndFrame`), which needs a
//! bitstream parser to fill DXVA picture params. This module ships the
//! **MF-decoder-MFT** alternative from §5c — still GPU-backed via a shared
//! `IMFDXGIDeviceManager` and put in low-latency mode — because it is the
//! correctness baseline that works without a hand-written slice parser. The
//! decoded frame is handed back as a shared `ID3D11Texture2D` slice for the
//! renderer to sample directly (§7 NV12→RGB shader).
//!
//! Low-latency correctness (§5c): the stream carries zero reordering
//! (guaranteed by the encoder's zero B-frames) AND the decoder is in low-latency
//! mode; both are required or a frame of delay remains. Note the H.264 **decoder**
//! quirk — `CODECAPI_AVLowLatencyMode` is set as `VT_UI4`, not `VT_BOOL`.

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;

use crate::{Codec, Error};

/// A decoded frame as a GPU texture slice (§5c texture-array pitfall: decoded
/// frames arrive as a slice of a texture array + array index).
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub array_index: u32,
    pub timestamp: i64,
}

/// A configured MF decoder. Hardware (DXVA) decoders output GPU textures
/// directly; the software fallback outputs system-memory NV12 that this decoder
/// uploads into `upload_tex` so the renderer sees the same `ID3D11Texture2D`.
pub struct Decoder {
    transform: IMFTransform,
    input_id: u32,
    output_id: u32,
    width: u32,
    height: u32,
    _dxgi_mgr: Option<IMFDXGIDeviceManager>,
    /// True when running the software decode path (no GPU decoder present).
    software: bool,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Software path (created on first frame): a mappable STAGING texture the CPU
    /// writes, copied into the sampleable DEFAULT texture the renderer reads. Two
    /// textures because NV12 + DYNAMIC + shader-resource Map is not universally
    /// supported, but STAGING Map + CopyResource always is.
    upload_staging: Option<ID3D11Texture2D>,
    upload_tex: Option<ID3D11Texture2D>,
}

impl Decoder {
    /// Enumerate/instantiate the decoder MFT for `codec`, bind it to `device`
    /// (shared with the renderer), and configure input/output + low latency. Falls
    /// back to a software decoder + texture upload when no GPU decoder exists.
    pub fn new(
        device: &ID3D11Device,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, Error> {
        super::encode::ensure_mf_startup();
        let subtype = input_subtype(codec);

        let (transform, is_hw) = enumerate_decoder(subtype)?.ok_or(Error::NoDecoder(codec))?;

        // Hardware decoders write GPU surfaces via the DXGI device manager. A
        // software decoder ignores (and may reject) SET_D3D_MANAGER, so skip it and
        // take the system-memory → texture-upload path instead.
        let dxgi_mgr = if is_hw {
            let mgr = super::encode::create_dxgi_manager(device)?;
            // SAFETY: manager is a valid IUnknown.
            unsafe {
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;
            }
            Some(mgr)
        } else {
            tracing::warn!(
                "no hardware {codec:?} decoder — using software decode (system memory → texture upload)"
            );
            None
        };

        // Low latency via the transform attributes; the decoder-side quirk (set
        // as VT_UI4) is handled by SetUINT32.
        // SAFETY: valid transform.
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }

        let (input_id, output_id) = super::encode::stream_ids(&transform);

        // Compressed input type first, then request the NV12 output type.
        set_decoder_input_type(&transform, input_id, subtype, width, height)?;
        set_decoder_output_type(&transform, output_id, width, height)?;

        // SAFETY: configured transform.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        // Immediate context for the software upload path's Map/Unmap.
        // SAFETY: valid device.
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext()? };

        Ok(Self {
            transform,
            input_id,
            output_id,
            width,
            height,
            _dxgi_mgr: dxgi_mgr,
            software: !is_hw,
            device: device.clone(),
            context,
            upload_staging: None,
            upload_tex: None,
        })
    }

    /// Feed one compressed access unit and return any decoded frame it produced.
    /// Returns `Ok(None)` when the decoder needs more input (normal for the
    /// first few packets of a stream).
    pub fn decode(&mut self, au: &[u8], timestamp: i64) -> Result<Option<DecodedFrame>, Error> {
        let sample = make_input_sample(au, timestamp)?;
        // SAFETY: valid transform + sample.
        unsafe {
            match self.transform.ProcessInput(self.input_id, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // Drain first, then retry once.
                    if let Some(f) = self.drain_output()? {
                        // Re-feed after draining.
                        self.transform.ProcessInput(self.input_id, &sample, 0)?;
                        return Ok(Some(f));
                    }
                    self.transform.ProcessInput(self.input_id, &sample, 0)?;
                }
                Err(e) => return Err(Error::Win(e)),
            }
        }
        self.drain_output()
    }

    fn drain_output(&mut self) -> Result<Option<DecodedFrame>, Error> {
        let mut out_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: single-element output-buffer array.
        let hr = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut out_buffer), &mut status)
        };
        // SAFETY: taken exactly once.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut out_buffer.pSample) };
        let _ = unsafe { std::mem::ManuallyDrop::take(&mut out_buffer.pEvents) };

        if let Err(e) = hr {
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                // The decoder announced its real output format (common on the very
                // first frame, especially for software decoders). Re-negotiate the
                // NV12 output type; the next ProcessOutput then yields a frame.
                set_decoder_output_type(&self.transform, self.output_id, self.width, self.height)?;
                return Ok(None);
            }
            return Err(Error::Win(e));
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        let timestamp = unsafe { sample.GetSampleTime().unwrap_or(0) };
        // Hardware: the sample already wraps a GPU texture. Software: upload the
        // system-memory NV12 into a shared texture for the renderer.
        let (texture, array_index) = if self.software {
            (self.upload_software_sample(&sample)?, 0)
        } else {
            sample_to_texture(&sample)?
        };
        Ok(Some(DecodedFrame {
            texture,
            array_index,
            timestamp,
        }))
    }

    /// Software path: copy an NV12 system-memory sample into a reusable DYNAMIC
    /// D3D11 texture so the renderer samples it exactly like a hardware-decoded
    /// frame. Uses the sample's real stride (`IMF2DBuffer`/`MF_MT_DEFAULT_STRIDE`),
    /// not an assumed one, so padded pitches don't shear the image.
    fn upload_software_sample(&mut self, sample: &IMFSample) -> Result<ID3D11Texture2D, Error> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.upload_tex.is_none() {
            self.upload_staging = Some(create_nv12_texture(
                &self.device,
                self.width,
                self.height,
                D3D11_USAGE_STAGING,
                0,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
            )?);
            self.upload_tex = Some(create_nv12_texture(
                &self.device,
                self.width,
                self.height,
                D3D11_USAGE_DEFAULT,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
                0,
            )?);
        }
        let staging = self.upload_staging.clone().ok_or(Error::NoDecoder(Codec::H264))?;
        let tex = self.upload_tex.clone().ok_or(Error::NoDecoder(Codec::H264))?;

        // SAFETY: MF buffer lock + D3D map, both released before returning.
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            let dst = mapped.pData as *mut u8;
            let dst_pitch = mapped.RowPitch as usize;

            // Prefer IMF2DBuffer (authoritative stride; chroma at stride*height).
            let copied = if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch: i32 = 0;
                two_d
                    .Lock2D(&mut scan0, &mut pitch)
                    .ok()
                    .map(|()| {
                        let sp = pitch.unsigned_abs() as usize;
                        copy_nv12(scan0, sp, dst, dst_pitch, w, h);
                        let _ = two_d.Unlock2D();
                    })
                    .is_some()
            } else {
                false
            };

            if !copied {
                // Fallback: contiguous lock with the negotiated default stride.
                let mut ptr: *mut u8 = std::ptr::null_mut();
                if buffer.Lock(&mut ptr, None, None).is_ok() {
                    let sp = self.src_stride();
                    copy_nv12(ptr, sp, dst, dst_pitch, w, h);
                    let _ = buffer.Unlock();
                }
            }

            self.context.Unmap(&staging, 0);
            // Push the freshly-written NV12 to the sampleable GPU texture.
            self.context.CopyResource(&tex, &staging);
        }
        Ok(tex)
    }

    /// The decoder's negotiated NV12 row stride (bytes), for the contiguous-lock
    /// fallback. Falls back to the frame width when unavailable.
    fn src_stride(&self) -> usize {
        // SAFETY: reads a UINT32 attribute off the current output type.
        unsafe {
            self.transform
                .GetOutputCurrentType(self.output_id)
                .ok()
                .and_then(|t| t.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok())
                .map(|s| s as usize)
                .filter(|&s| s >= self.width as usize)
                .unwrap_or(self.width as usize)
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Copy an NV12 image (Y plane `h` rows then interleaved UV plane `h/2` rows,
/// each `w` bytes wide) between buffers with independent row pitches. The chroma
/// plane starts at `pitch * h` in both source and destination (NV12 layout).
///
/// SAFETY: `src` readable and `dst` writable for the NV12 extent at their pitches.
unsafe fn copy_nv12(
    src: *const u8,
    src_pitch: usize,
    dst: *mut u8,
    dst_pitch: usize,
    w: usize,
    h: usize,
) {
    let row = w.min(src_pitch).min(dst_pitch);
    // SAFETY: caller guarantees src/dst are valid for the NV12 extent.
    unsafe {
        for y in 0..h {
            std::ptr::copy_nonoverlapping(src.add(y * src_pitch), dst.add(y * dst_pitch), row);
        }
        let src_uv = src.add(src_pitch * h);
        let dst_uv = dst.add(dst_pitch * h);
        for y in 0..(h / 2) {
            std::ptr::copy_nonoverlapping(src_uv.add(y * src_pitch), dst_uv.add(y * dst_pitch), row);
        }
    }
}

/// Create an NV12 texture with the given usage/bind/CPU-access flags (software
/// decode path: one STAGING for CPU writes, one DEFAULT+shader-resource to sample).
fn create_nv12_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D, Error> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: valid device + desc; out-param initialized.
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
    }
    tex.ok_or(Error::NoDecoder(Codec::H264))
}

fn input_subtype(codec: Codec) -> windows::core::GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

/// Cheap probe: is there a hardware decoder for `codec` on this machine? Mirrors
/// [`crate::encode::can_encode`] — counts `MFTEnumEx` results without activating
/// them. The viewer uses this so it advertises only codecs it can actually
/// decode; otherwise the host may negotiate a codec (e.g. AV1) the viewer cannot
/// decode, which black-screens the session (§3 negotiation).
pub fn can_decode(codec: Codec) -> bool {
    super::encode::ensure_mf_startup();
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: input_subtype(codec),
    };
    // Count hardware MFTs first, then software (the Microsoft H.264 decoder is a
    // sync software MFT). Either kind means we can decode this codec.
    for flags in [
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
    ] {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        // SAFETY: out-array/count pair per MFTEnumEx; we free the array below.
        unsafe {
            if MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input_info),
                None,
                &mut activates,
                &mut count,
            )
            .is_ok()
            {
                if !activates.is_null() {
                    let slice = std::slice::from_raw_parts_mut(activates, count as usize);
                    for a in slice.iter_mut() {
                        let _ = a.take();
                    }
                    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
                }
                if count > 0 {
                    return true;
                }
            }
        }
    }
    false
}

/// The codecs this viewer can hardware-decode, in the plan's preference order
/// (§3). H.264 is always included as the guaranteed baseline so negotiation
/// against any host still resolves (the host defaults to H.264 regardless).
pub fn viewer_decodable() -> Vec<Codec> {
    let mut v = Vec::new();
    for c in [Codec::Av1, Codec::Hevc, Codec::H264] {
        if can_decode(c) {
            v.push(c);
        }
    }
    if !v.contains(&Codec::H264) {
        v.push(Codec::H264);
    }
    v
}

/// Instantiate a decoder MFT for `subtype`. Prefer a hardware (DXVA) decoder for
/// zero-copy GPU output; fall back to a software decoder (system-memory NV12,
/// which the caller uploads to a texture) so machines with no GPU video decoder —
/// VMs, basic-display adapters, some thin laptops — can still view. Returns the
/// transform paired with whether it is hardware.
fn enumerate_decoder(subtype: windows::core::GUID) -> Result<Option<(IMFTransform, bool)>, Error> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    for (flags, is_hw) in [
        (MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, true),
        (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, false),
    ] {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        // SAFETY: out-array/count pair per MFTEnumEx.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input_info),
                None,
                &mut activates,
                &mut count,
            )?;
        }
        if count == 0 || activates.is_null() {
            continue;
        }
        // SAFETY: `count` slots allocated.
        let slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let first = slice[0].clone();
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
        let Some(activate) = first else {
            continue;
        };
        // SAFETY: valid activate.
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };
        return Ok(Some((transform, is_hw)));
    }
    Ok(None)
}

fn set_decoder_input_type(
    transform: &IMFTransform,
    input_id: u32,
    subtype: windows::core::GUID,
    w: u32,
    h: u32,
) -> Result<(), Error> {
    // SAFETY: standard media-type construction.
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        transform.SetInputType(input_id, &mt, 0)?;
    }
    Ok(())
}

fn set_decoder_output_type(
    transform: &IMFTransform,
    output_id: u32,
    w: u32,
    h: u32,
) -> Result<(), Error> {
    // Pick the NV12 output type the MFT advertises (§5c).
    // SAFETY: iterate available output types.
    unsafe {
        let mut i = 0u32;
        while let Ok(mt) = transform.GetOutputAvailableType(output_id, i) {
            let sub = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
            if sub == MFVideoFormat_NV12 {
                let _ = mt.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64);
                transform.SetOutputType(output_id, &mt, 0)?;
                return Ok(());
            }
            i += 1;
        }
    }
    Err(Error::NoDecoder(Codec::H264))
}

/// Build an input `IMFSample` from a CPU byte slice (the AU reassembled from the
/// transport). Uses a memory buffer + copy; the compressed side is tiny.
fn make_input_sample(au: &[u8], timestamp: i64) -> Result<IMFSample, Error> {
    // SAFETY: standard MF buffer/sample construction; copy bounded by len.
    unsafe {
        let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max_len), None)?;
        std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(au.len() as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp)?;
        Ok(sample)
    }
}

/// Extract the shared texture + array index from a decoded GPU sample (§5c).
fn sample_to_texture(sample: &IMFSample) -> Result<(ID3D11Texture2D, u32), Error> {
    // SAFETY: decoded sample carries a single DXGI buffer.
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        let dxgi: IMFDXGIBuffer = buffer.cast()?;
        let mut texture: Option<ID3D11Texture2D> = None;
        dxgi.GetResource(
            &ID3D11Texture2D::IID,
            &mut texture as *mut _ as *mut *mut core::ffi::c_void,
        )?;
        let index = dxgi.GetSubresourceIndex().unwrap_or(0);
        Ok((texture.ok_or(Error::NoDecoder(Codec::H264))?, index))
    }
}
