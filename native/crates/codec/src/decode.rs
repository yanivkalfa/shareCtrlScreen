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
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;

use crate::{Codec, Error};

/// A decoded frame as a GPU texture slice (§5c texture-array pitfall: decoded
/// frames arrive as a slice of a texture array + array index).
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub array_index: u32,
    pub timestamp: i64,
}

/// A configured MF hardware decoder bound to the shared D3D11 device.
pub struct Decoder {
    transform: IMFTransform,
    input_id: u32,
    output_id: u32,
    width: u32,
    height: u32,
    _dxgi_mgr: IMFDXGIDeviceManager,
}

impl Decoder {
    /// Enumerate/instantiate the HW decoder MFT for `codec`, bind it to `device`
    /// (shared with the renderer), and configure input/output + low latency.
    pub fn new(
        device: &ID3D11Device,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, Error> {
        super::encode::ensure_mf_startup();
        let subtype = input_subtype(codec);

        let transform = enumerate_decoder(subtype)?.ok_or(Error::NoDecoder(codec))?;

        // Bind the shared D3D11 device for GPU-surface output.
        let dxgi_mgr = super::encode::create_dxgi_manager(device)?;
        // SAFETY: manager is a valid IUnknown.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dxgi_mgr.as_raw() as usize)?;
        }

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

        Ok(Self {
            transform,
            input_id,
            output_id,
            width,
            height,
            _dxgi_mgr: dxgi_mgr,
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

    fn drain_output(&self) -> Result<Option<DecodedFrame>, Error> {
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
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT
                || e.code() == MF_E_TRANSFORM_STREAM_CHANGE
            {
                return Ok(None);
            }
            return Err(Error::Win(e));
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        let timestamp = unsafe { sample.GetSampleTime().unwrap_or(0) };
        let (texture, array_index) = sample_to_texture(&sample)?;
        Ok(Some(DecodedFrame {
            texture,
            array_index,
            timestamp,
        }))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
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
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
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
        .is_err()
        {
            return false;
        }
        if !activates.is_null() {
            let slice = std::slice::from_raw_parts_mut(activates, count as usize);
            for a in slice.iter_mut() {
                let _ = a.take();
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        }
    }
    count > 0
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

fn enumerate_decoder(subtype: windows::core::GUID) -> Result<Option<IMFTransform>, Error> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
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
        return Ok(None);
    }
    // SAFETY: `count` slots allocated.
    let slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let first = slice[0].clone();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
    let Some(activate) = first else {
        return Ok(None);
    };
    // SAFETY: valid activate.
    let transform: IMFTransform = unsafe { activate.ActivateObject()? };
    Ok(Some(transform))
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
