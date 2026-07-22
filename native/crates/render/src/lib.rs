//! D3D11 flip-model presentation of decoded NV12 frames (Plan 04 §7). Lowest
//! latency present path: `DXGI_SWAP_EFFECT_FLIP_DISCARD`, BufferCount 2,
//! `FRAME_LATENCY_WAITABLE_OBJECT | ALLOW_TEARING`,
//! `SetMaximumFrameLatency(1)`, and per-frame **wait-first-then-render**
//! (`WaitForSingleObjectEx(waitable)` → render → `Present(0, ALLOW_TEARING)`).
//!
//! The NV12→RGB conversion is a pixel shader sampling two SRVs (`R8_UNORM` luma
//! and `R8G8_UNORM` chroma) with the BT.709 matrix + range scale (§7). The
//! cursor is drawn as a **separate sprite** at the out-of-band `PointerPosition`
//! so it never lags the video stream.

#![cfg(windows)]

pub mod window;
pub use window::VideoWindow;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGIFactory5, IDXGISwapChain2, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("shader compile: {0}")]
    Shader(String),
}

/// Fullscreen-triangle VS (no vertex buffer — uses SV_VertexID) + NV12→RGB PS
/// with the BT.709 matrix and limited-range scaling (§7).
const SHADER_HLSL: &str = r#"
Texture2D<float>  LumaTex   : register(t0);
Texture2D<float2> ChromaTex : register(t1);
SamplerState      Samp      : register(s0);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    // Oversized triangle covering the viewport.
    o.uv  = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float4 ps_main(VSOut i) : SV_Target {
    float  y  = LumaTex.Sample(Samp, i.uv);
    float2 uv = ChromaTex.Sample(Samp, i.uv) - float2(0.5, 0.5);
    // BT.709, limited range (16-235 luma / 16-240 chroma).
    y = (y - 16.0/255.0) * (255.0/219.0);
    float r = y + 1.5748 * uv.y;
    float g = y - 0.1873 * uv.x - 0.4681 * uv.y;
    float b = y + 1.8556 * uv.x;
    return float4(saturate(float3(r, g, b)), 1.0);
}

// Cursor sprite (§5a/§7): DDA delivers the desktop WITHOUT the mouse cursor, so
// the viewer draws it client-side at the out-of-band PointerPosition — it feels
// instant and never lags the stream. A simple arrow-ish quad; center + halfsize
// come in NDC via a constant buffer.
cbuffer Cursor : register(b0) { float2 center; float2 halfsize; float2 pad; }

struct CurOut { float4 pos : SV_Position; float2 luv : TEXCOORD0; };

CurOut cur_vs(uint id : SV_VertexID) {
    float2 corner[6] = {
        float2(0,0), float2(1,0), float2(0,1),
        float2(0,1), float2(1,0), float2(1,1)
    };
    float2 c = corner[id];
    CurOut o;
    o.luv = c;
    // Quad from center to center + 2*halfsize (top-left anchored, like a pointer).
    float2 p = center + c * halfsize * 2.0;
    o.pos = float4(p, 0, 1);
    return o;
}

float4 cur_ps(CurOut i) : SV_Target {
    // A white arrow with a dark edge: filled lower-left triangle of the quad.
    if (i.luv.x + i.luv.y > 1.0) discard;
    float edge = (i.luv.x < 0.12 || i.luv.y < 0.12 || (i.luv.x + i.luv.y) > 0.88) ? 0.0 : 1.0;
    return float4(edge, edge, edge, 1.0);
}
"#;

/// A flip-model swapchain bound to the shared D3D11 device, rendering NV12
/// frames from the decoder.
pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain2,
    rtv: Option<ID3D11RenderTargetView>,
    waitable: HANDLE,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cursor_vs: ID3D11VertexShader,
    cursor_ps: ID3D11PixelShader,
    cursor_cb: ID3D11Buffer,
    allow_tearing: bool,
    /// The child HWND we present into — used to track its client size so the
    /// swapchain matches it (no DXGI stretch-scaling) and the video is letterboxed
    /// to the real window, scaled exactly once by our shader.
    hwnd: HWND,
    /// Current swapchain (backbuffer) size = the window client size.
    width: u32,
    height: u32,
    /// Diagnostics: successful Present count (first one logged).
    presented: u64,
    /// SRV cache keyed by (texture ptr, array slice): creating two SRVs per frame
    /// costs measurable CPU (§5c note); decode textures are a small stable set
    /// (software path: one; hardware: the decoder's texture-array slices).
    srv_cache: std::collections::HashMap<
        (usize, u32),
        (ID3D11ShaderResourceView, ID3D11ShaderResourceView),
    >,
}

impl Renderer {
    /// Create the swapchain for `hwnd` (Option A native child HWND, §7) using the
    /// device shared with the decoder so decoded textures need no copy. The
    /// swapchain is sized to the window's CLIENT area (not the video resolution)
    /// so DXGI never stretch-scales; the video is fit into it, aspect-preserved,
    /// by the shader — one clean scale instead of two lossy ones.
    pub fn new(device: &ID3D11Device, hwnd: HWND, _vw: u32, _vh: u32) -> Result<Self, Error> {
        let (width, height) = client_size(hwnd);
        let context = unsafe { device.GetImmediateContext()? };

        // Get the DXGI factory that made this device.
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };

        // Tearing support gates ALLOW_TEARING (§7).
        let allow_tearing = check_tearing(&factory);

        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        if allow_tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: windows::Win32::Graphics::Dxgi::DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
        };

        // SAFETY: valid device/hwnd; desc fully initialized.
        let swapchain1 =
            unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)? };
        let swapchain: IDXGISwapChain2 = swapchain1.cast()?;
        // 1-frame latency (§7).
        unsafe { swapchain.SetMaximumFrameLatency(1)? };
        let waitable = unsafe { swapchain.GetFrameLatencyWaitableObject() };

        let rtv = Some(create_rtv(device, &swapchain)?);
        let (vs, ps) = compile_shaders(device)?;
        let (cursor_vs, cursor_ps) = compile_cursor_shaders(device)?;
        let cursor_cb = create_cursor_cb(device)?;
        let sampler = create_sampler(device)?;

        Ok(Self {
            device: device.clone(),
            context,
            swapchain,
            rtv,
            waitable,
            vs,
            ps,
            sampler,
            cursor_vs,
            cursor_ps,
            cursor_cb,
            allow_tearing,
            hwnd,
            width,
            height,
            presented: 0,
            srv_cache: std::collections::HashMap::new(),
        })
    }

    /// Present one decoded NV12 frame. `array_index` selects the decoder texture-
    /// array slice (§5c). `cursor` is the out-of-band pointer position normalized
    /// `[0,1]` (`None` ⇒ don't draw). Wait-first-then-render for minimum queued
    /// latency (§7).
    pub fn render_frame(
        &mut self,
        nv12: &ID3D11Texture2D,
        array_index: u32,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), Error> {
        // Keep the swapchain matched to the window client size so DXGI never
        // stretch-scales (the biggest source of blur); resize is a no-op when
        // unchanged.
        self.sync_size();

        // Block until the swapchain can accept a new frame (1-deep).
        // SAFETY: waitable handle from the swapchain.
        unsafe { WaitForSingleObjectEx(self.waitable, 1000, false) };

        // Video's real pixel size, for aspect-preserving letterbox. Publish it so
        // input normalization maps clicks over the same letterbox rect.
        let (vw, vh) = texture_size(nv12);
        window::set_video_size(vw, vh);

        // SRVs from the per-slice cache (created once per texture/slice).
        let key = (nv12.as_raw() as usize, array_index);
        if !self.srv_cache.contains_key(&key) {
            let luma = self.srv(nv12, DXGI_FORMAT_R8_UNORM, array_index)?;
            let chroma = self.srv(nv12, DXGI_FORMAT_R8G8_UNORM, array_index)?;
            // Bound: decoders recycle a small texture set; if something churns
            // textures (e.g. re-init), don't let dead entries pile up.
            if self.srv_cache.len() > 64 {
                self.srv_cache.clear();
            }
            self.srv_cache.insert(key, (luma, chroma));
        }
        let (luma, chroma) = self.srv_cache[&key].clone();

        // SAFETY: all resources valid; single-threaded present.
        unsafe {
            let rtvs = [self.rtv.clone()];
            // Clear before draw (flip-discard backbuffer is undefined) — also the
            // letterbox color when the frame doesn't cover the full target.
            if let Some(rtv) = &rtvs[0] {
                self.context
                    .ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
            self.context.OMSetRenderTargets(Some(&rtvs), None);
            // Aspect-preserving letterbox: fit the video rect inside the window,
            // centered, with black bars — no stretch/distortion. The shader maps
            // the full texture (uv 0..1) into this viewport, so one clean scale.
            let vp = letterbox(self.width, self.height, vw, vh);
            self.context.RSSetViewports(Some(&[vp]));
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetShaderResources(0, Some(&[Some(luma), Some(chroma)]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.Draw(3, 0);

            // Cursor sprite on top (§5a/§7).
            if let Some((nx, ny)) = cursor {
                self.draw_cursor(nx, ny)?;
            }

            let present_flags = if self.allow_tearing {
                DXGI_PRESENT_ALLOW_TEARING
            } else {
                windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0)
            };
            let hr = self.swapchain.Present(0, present_flags);
            if hr.is_err() {
                tracing::warn!("renderer: Present failed hr={:#010x}", hr.0);
            }
            hr.ok()?;
            self.presented += 1;
            if self.presented == 1 {
                tracing::info!("renderer: first frame presented");
            }
        }
        Ok(())
    }

    /// Draw the client-side cursor sprite at normalized position (§5a/§7).
    fn draw_cursor(&mut self, nx: f64, ny: f64) -> Result<(), Error> {
        // Normalized [0,1] → NDC (top-left origin; y flips).
        let cx = (nx as f32) * 2.0 - 1.0;
        let cy = 1.0 - (ny as f32) * 2.0;
        // Fixed pixel-ish size in NDC (~18×26 px on 1080p).
        let hx = 18.0 / self.width as f32;
        let hy = 26.0 / self.height as f32;
        let cb = [cx, cy, hx, hy, 0.0, 0.0, 0.0, 0.0];

        // SAFETY: dynamic constant buffer written via a discard map.
        unsafe {
            let mut mapped =
                windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(
                &self.cursor_cb,
                0,
                windows::Win32::Graphics::Direct3D11::D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(cb.as_ptr(), mapped.pData as *mut f32, cb.len());
            self.context.Unmap(&self.cursor_cb, 0);

            self.context.VSSetShader(&self.cursor_vs, None);
            self.context.PSSetShader(&self.cursor_ps, None);
            self.context
                .VSSetConstantBuffers(0, Some(&[Some(self.cursor_cb.clone())]));
            self.context.Draw(6, 0);
        }
        Ok(())
    }

    /// Match the swapchain to the window's current client size (no-op if same).
    /// Called each frame so mid-session window resizes stay crisp (1:1, no DXGI
    /// stretch). Errors are swallowed — a failed resize keeps the old buffers.
    fn sync_size(&mut self) {
        let (w, h) = client_size(self.hwnd);
        if w != self.width || h != self.height {
            let _ = self.resize(w, h);
        }
    }

    /// Resize the swapchain (window/stream resolution change).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), Error> {
        self.width = width;
        self.height = height;
        // Release the RTV before ResizeBuffers (all back-buffer refs must drop).
        self.rtv = None;
        unsafe { self.context.ClearState() };
        // SAFETY: standard resize sequence.
        unsafe {
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            if self.allow_tearing {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            }
            self.swapchain.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(flags as i32),
            )?;
        }
        self.rtv = Some(create_rtv(&self.device, &self.swapchain)?);
        Ok(())
    }

    /// Cache-per-slice SRV creation (§5c notes creating an SRV per frame is ~3×
    /// the CPU; a production build caches one SRV per pool slice — kept simple
    /// here, called at most twice per frame).
    fn srv(
        &self,
        tex: &ID3D11Texture2D,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        array_index: u32,
    ) -> Result<ID3D11ShaderResourceView, Error> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: array_index,
                    ArraySize: 1,
                },
            },
        };
        let mut srv = None;
        // SAFETY: valid texture + desc.
        unsafe {
            self.device
                .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))?
        };
        srv.ok_or_else(|| Error::Shader("null SRV".into()))
    }
}

/// Window client-area size in physical pixels (each at least 1).
fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = windows::Win32::Foundation::RECT::default();
    // SAFETY: valid child HWND.
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc) };
    (
        (rc.right - rc.left).max(1) as u32,
        (rc.bottom - rc.top).max(1) as u32,
    )
}

/// A texture's pixel dimensions.
fn texture_size(tex: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture.
    unsafe { tex.GetDesc(&mut desc) };
    (desc.Width.max(1), desc.Height.max(1))
}

/// Aspect-preserving letterbox: the largest (vw×vh)-aspect rect that fits inside
/// (ww×wh), centered. The shader maps the whole texture into this viewport.
fn letterbox(ww: u32, wh: u32, vw: u32, vh: u32) -> D3D11_VIEWPORT {
    let (ww, wh, vw, vh) = (ww as f32, wh as f32, vw as f32, vh as f32);
    let scale = (ww / vw).min(wh / vh);
    let (w, h) = (vw * scale, vh * scale);
    D3D11_VIEWPORT {
        TopLeftX: ((ww - w) * 0.5).max(0.0),
        TopLeftY: ((wh - h) * 0.5).max(0.0),
        Width: w,
        Height: h,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn check_tearing(factory: &IDXGIFactory2) -> bool {
    let Ok(f5) = factory.cast::<IDXGIFactory5>() else {
        return false;
    };
    let mut allow: windows::core::BOOL = false.into();
    // SAFETY: out param sized correctly.
    let ok = unsafe {
        f5.CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            &mut allow as *mut _ as *mut _,
            std::mem::size_of::<windows::core::BOOL>() as u32,
        )
    };
    ok.is_ok() && allow.as_bool()
}

fn create_rtv(
    device: &ID3D11Device,
    swapchain: &IDXGISwapChain2,
) -> Result<ID3D11RenderTargetView, Error> {
    // SAFETY: buffer 0 is the back buffer.
    unsafe {
        let backbuffer: ID3D11Texture2D = swapchain.GetBuffer(0)?;
        let mut rtv = None;
        device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv))?;
        rtv.ok_or_else(|| Error::Shader("null RTV".into()))
    }
}

fn compile_shaders(
    device: &ID3D11Device,
) -> Result<(ID3D11VertexShader, ID3D11PixelShader), Error> {
    let vs_blob = compile(SHADER_HLSL, "vs_main", "vs_5_0")?;
    let ps_blob = compile(SHADER_HLSL, "ps_main", "ps_5_0")?;
    // SAFETY: valid bytecode blobs.
    unsafe {
        let mut vs = None;
        device.CreateVertexShader(blob_slice(&vs_blob), None, Some(&mut vs))?;
        let mut ps = None;
        device.CreatePixelShader(blob_slice(&ps_blob), None, Some(&mut ps))?;
        Ok((
            vs.ok_or_else(|| Error::Shader("null VS".into()))?,
            ps.ok_or_else(|| Error::Shader("null PS".into()))?,
        ))
    }
}

fn compile_cursor_shaders(
    device: &ID3D11Device,
) -> Result<(ID3D11VertexShader, ID3D11PixelShader), Error> {
    let vs_blob = compile(SHADER_HLSL, "cur_vs", "vs_5_0")?;
    let ps_blob = compile(SHADER_HLSL, "cur_ps", "ps_5_0")?;
    // SAFETY: valid bytecode blobs.
    unsafe {
        let mut vs = None;
        device.CreateVertexShader(blob_slice(&vs_blob), None, Some(&mut vs))?;
        let mut ps = None;
        device.CreatePixelShader(blob_slice(&ps_blob), None, Some(&mut ps))?;
        Ok((
            vs.ok_or_else(|| Error::Shader("null cursor VS".into()))?,
            ps.ok_or_else(|| Error::Shader("null cursor PS".into()))?,
        ))
    }
}

/// A 32-byte dynamic constant buffer for the cursor sprite (center + halfsize).
fn create_cursor_cb(device: &ID3D11Device) -> Result<ID3D11Buffer, Error> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_USAGE_DYNAMIC,
    };
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 32, // 8 floats, 16-byte aligned
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut cb = None;
    // SAFETY: valid desc; no initial data.
    unsafe { device.CreateBuffer(&desc, None, Some(&mut cb))? };
    cb.ok_or_else(|| Error::Shader("null cursor cbuffer".into()))
}

fn blob_slice(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: blob owns its buffer for its lifetime.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn compile(src: &str, entry: &str, target: &str) -> Result<ID3DBlob, Error> {
    let entry_c = std::ffi::CString::new(entry).unwrap();
    let target_c = std::ffi::CString::new(target).unwrap();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: source buffer + names outlive the call.
    let hr = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        let msg = errors
            .as_ref()
            .map(|b| unsafe {
                let s = std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                );
                String::from_utf8_lossy(s).into_owned()
            })
            .unwrap_or_else(|| e.to_string());
        return Err(Error::Shader(msg));
    }
    code.ok_or_else(|| Error::Shader("no bytecode".into()))
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState, Error> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut sampler = None;
    // SAFETY: valid desc.
    unsafe { device.CreateSamplerState(&desc, Some(&mut sampler))? };
    sampler.ok_or_else(|| Error::Shader("null sampler".into()))
}
