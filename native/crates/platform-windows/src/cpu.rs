//! CPU ↔ D3D11 frame copies for the software codec (Cisco OpenH264), which
//! only reads and writes system memory:
//! * host: [`D3d11Readback`] copies a Desktop Duplication BGRA texture into a
//!   CPU frame for the encoder;
//! * viewer: [`Nv12Upload`] puts a CPU NV12 picture into a texture the D3D11
//!   renderer samples like a hardware-decoded one.
//!
//! Both go through a STAGING texture (Map) plus a GPU copy, the pattern the
//! Media Foundation adapters already use: NV12 + DYNAMIC + shader-resource Map
//! is not universally supported, STAGING Map + copy always is.

use std::any::Any;

use codec_openh264::{FrameReadback, ReadbackFactory};
use platform_api::{CodecError, Frame, FrameData, PixelFormat, SurfaceError};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

use crate::gpu::{D3d11Device, D3d11Texture};

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: valid device and descriptor; `tex` is the out-parameter.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .map_err(|e| format!("{width}x{height} texture: {e}"))?;
    tex.ok_or_else(|| format!("{width}x{height} texture: driver returned none"))
}

// ---- host: GPU capture → CPU BGRA ---------------------------------------------

/// Creates a [`StagingReadback`] on the capture device.
pub struct D3d11Readback;

impl ReadbackFactory for D3d11Readback {
    fn new_readback(&self, device: Option<&dyn Any>) -> Result<Box<dyn FrameReadback>, CodecError> {
        let device = device
            .and_then(|d| d.downcast_ref::<D3d11Device>())
            .ok_or_else(|| {
                CodecError::Unsupported("GPU readback needs the capture's D3D11 device".into())
            })?
            .0
            .clone();
        // SAFETY: a live device always has an immediate context.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| CodecError::Backend(format!("D3D11 context: {e}")))?;
        Ok(Box::new(StagingReadback {
            device,
            context,
            staging: None,
            pixels: Vec::new(),
        }))
    }
}

struct StagingReadback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Reused while the capture size stays the same.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pixels: Vec<u8>,
}

impl FrameReadback for StagingReadback {
    fn read<'a>(&'a mut self, frame: &Frame<'_>) -> Result<Frame<'a>, CodecError> {
        let FrameData::Native(any) = frame.data else {
            return Err(CodecError::Unsupported("readback of a CPU frame".into()));
        };
        let src = any
            .downcast_ref::<D3d11Texture>()
            .ok_or_else(|| CodecError::Unsupported("not a D3D11 texture".into()))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; `desc` is a plain out-struct.
        unsafe { src.texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CodecError::Unsupported(format!(
                "software encoder reads BGRA captures, got DXGI format {}",
                desc.Format.0
            )));
        }
        let (w, h) = (desc.Width, desc.Height);
        let staging = match &self.staging {
            Some((t, sw, sh)) if (*sw, *sh) == (w, h) => t.clone(),
            _ => {
                let t = texture(
                    &self.device,
                    w,
                    h,
                    desc.Format,
                    D3D11_USAGE_STAGING,
                    0,
                    D3D11_CPU_ACCESS_READ.0 as u32,
                )
                .map_err(|e| CodecError::Backend(format!("readback staging {e}")))?;
                self.staging = Some((t.clone(), w, h));
                t
            }
        };
        let row = w as usize * 4;
        self.pixels.resize(row * h as usize, 0);
        // Subresource of slice `array_index`, mip 0 (D3D11CalcSubresource).
        let subresource = src.array_index * desc.MipLevels.max(1);
        // SAFETY: GPU copy into our staging texture, then a CPU map that is
        // released before returning; rows are copied within the mapped pitch.
        unsafe {
            self.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &src.texture,
                subresource,
                None,
            );
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CodecError::Backend(format!("map capture texture: {e}")))?;
            let base = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            for (y, out) in self.pixels.chunks_exact_mut(row).enumerate() {
                std::ptr::copy_nonoverlapping(base.add(y * pitch), out.as_mut_ptr(), row);
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(Frame::bgra(w, h, &self.pixels, row))
    }
}

// ---- viewer: CPU NV12 → presentable texture ---------------------------------

/// Uploads CPU NV12 pictures into a sampleable texture on the presenter's
/// device. Created lazily: only a software-decoded session needs it.
#[derive(Default)]
pub struct Nv12Upload {
    /// (staging, sampleable, width, height) — rebuilt when the size changes.
    textures: Option<(ID3D11Texture2D, ID3D11Texture2D, u32, u32)>,
}

impl Nv12Upload {
    /// Copy `frame` (CPU NV12) to the GPU; returns the texture to render.
    pub fn upload(
        &mut self,
        device: &ID3D11Device,
        frame: &Frame<'_>,
    ) -> Result<ID3D11Texture2D, SurfaceError> {
        let FrameData::Cpu {
            format: PixelFormat::Nv12,
            plane0: y,
            plane1: Some(uv),
        } = frame.data
        else {
            return Err(SurfaceError::Backend(
                "the D3D11 presenter takes D3D11 or CPU NV12 frames".into(),
            ));
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if w == 0
            || h == 0
            || y.stride < w
            || y.data.len() < y.stride * (h - 1) + w
            || uv.stride < cw * 2
            || uv.data.len() < uv.stride * (ch - 1) + cw * 2
        {
            return Err(SurfaceError::Backend("NV12 planes too small".into()));
        }
        // NV12 textures need even dimensions; an odd picture gets one padding
        // row/column, which is never visible in practice (hosts encode even).
        let (tw, th) = ((w as u32 + 1) & !1, (h as u32 + 1) & !1);
        let fail = |what: &str| SurfaceError::Backend(format!("NV12 upload: {what}"));
        let (staging, sampled) = match &self.textures {
            Some((s, t, sw, sh)) if (*sw, *sh) == (tw, th) => (s.clone(), t.clone()),
            _ => {
                let make = |usage, bind, cpu| {
                    texture(device, tw, th, DXGI_FORMAT_NV12, usage, bind, cpu)
                        .map_err(|e| fail(&e))
                };
                let s = make(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
                let t = make(D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE.0 as u32, 0)?;
                self.textures = Some((s.clone(), t.clone(), tw, th));
                (s, t)
            }
        };
        // SAFETY: the staging texture is mapped for writing and unmapped before
        // the GPU copy; rows stay inside both the source slices (checked above)
        // and the mapped pitch. A mapped NV12 surface has its UV plane right
        // after `RowPitch * Height` bytes of luma.
        unsafe {
            let ctx = device
                .GetImmediateContext()
                .map_err(|e| fail(&e.to_string()))?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .map_err(|e| fail(&e.to_string()))?;
            let base = mapped.pData as *mut u8;
            let pitch = mapped.RowPitch as usize;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    y.data.as_ptr().add(row * y.stride),
                    base.add(row * pitch),
                    w,
                );
            }
            let uv_base = base.add(pitch * th as usize);
            for row in 0..ch {
                std::ptr::copy_nonoverlapping(
                    uv.data.as_ptr().add(row * uv.stride),
                    uv_base.add(row * pitch),
                    cw * 2,
                );
            }
            ctx.Unmap(&staging, 0);
            ctx.CopyResource(&sampled, &staging);
        }
        Ok(sampled)
    }
}
