//! GPU BGRA→NV12 conversion via the D3D11 Video Processor (Plan 04 §5b: "DDA
//! gives BGRA; every HW encoder wants NV12 — convert on-GPU via the Video
//! Processor MFT or a compute shader (never CPU)").
//!
//! `ID3D11VideoProcessorBlt` runs the colour-space conversion + chroma subsample
//! entirely on the GPU and works on every vendor's driver, so it is the portable
//! baseline the MF encode path needs. The NV12 output texture is allocated once
//! and reused; only the per-frame input view is created fresh.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};

use crate::Error;

/// Reusable BGRA→NV12 converter bound to the shared D3D11 device.
pub(crate) struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    nv12: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

impl Converter {
    pub(crate) fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, Error> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        // SAFETY: standard immediate-context fetch.
        let context = unsafe { device.GetImmediateContext()? };
        let video_context: ID3D11VideoContext = context.cast()?;

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        // SAFETY: content desc fully initialized.
        let (enumerator, processor) = unsafe {
            let en = video_device.CreateVideoProcessorEnumerator(&content)?;
            let pr = video_device.CreateVideoProcessor(&en, 0)?;
            (en, pr)
        };

        // NV12 output texture (RENDER_TARGET bind = video-processor output).
        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12 = None;
        // SAFETY: valid desc.
        unsafe { device.CreateTexture2D(&tex_desc, None, Some(&mut nv12))? };
        let nv12 = nv12.ok_or_else(|| Error::NoEncoder(crate::Codec::H264))?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        // SAFETY: nv12 + enumerator valid.
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &nv12,
                &enumerator,
                &out_desc,
                Some(&mut output_view),
            )?;
        }
        let output_view = output_view.ok_or_else(|| Error::NoEncoder(crate::Codec::H264))?;

        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            nv12,
            output_view,
        })
    }

    /// Convert a BGRA texture to the internal NV12 texture and return it. The
    /// returned texture is reused across calls (single encode thread).
    pub(crate) fn convert_to_nv12(
        &mut self,
        bgra: &ID3D11Texture2D,
    ) -> Result<&ID3D11Texture2D, Error> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        // SAFETY: bgra + enumerator valid.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                bgra,
                &self.enumerator,
                &in_desc,
                Some(&mut input_view),
            )?;
        }

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(input_view),
            ..Default::default()
        };

        // SAFETY: single stream, valid views.
        let blt = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                &[stream.clone()],
            )
        };
        // Reclaim and drop the per-frame input view exactly once.
        // SAFETY: taken once; the clone passed to Blt held its own ref.
        let _ = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
        blt?;

        Ok(&self.nv12)
    }
}
