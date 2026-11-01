//! wgpu `Presenter` for the Wayland video subsurface: CPU NV12 upload (the L5 v1
//! path), letterbox + BT.709 conversion + cursor in one pass, low-latency
//! present mode, and the viewport sent in the same commit as the frame.
//!
//! ## References
//! * wgpu 30 `Instance::create_surface_unsafe` / `SurfaceTargetUnsafe::RawHandle`
//!   (raw Wayland display + surface), `InstanceDescriptor::display` ("on GLES
//!   this is required … especially for Wayland"), `CurrentSurfaceTexture`:
//!   <https://docs.rs/wgpu/30/wgpu/>
//! * raw-window-handle 0.6 `WaylandWindowHandle` / `WaylandDisplayHandle`:
//!   <https://docs.rs/raw-window-handle/0.6/raw_window_handle/>
//! * Mesa's Vulkan Wayland WSI (FIFO waits on `wl_surface.frame` callbacks —
//!   which an unmapped surface never receives; MAILBOX does not):
//!   <https://gitlab.freedesktop.org/mesa/mesa/-/blob/main/src/vulkan/wsi/wsi_common_wayland.c>

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use platform_api::{CpuPlane, Frame, FrameData, PixelFormat, Presenter, SurfaceError};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, RawDisplayHandle, RawWindowHandle,
    WaylandDisplayHandle, WaylandWindowHandle,
};

use crate::error::SpikeError;
use crate::geometry::letterbox;
use crate::surface::Shared;

/// GDK's `wl_display`, as the display handle wgpu's GL backend needs at
/// instance creation (EGL picks its platform display from it).
#[derive(Debug)]
struct GdkDisplay(std::ptr::NonNull<std::ffi::c_void>);

// SAFETY: a libwayland display pointer, valid for the process lifetime (GDK
// never closes its display while the app runs) and only handed to EGL/Vulkan,
// which are thread-safe for display use.
unsafe impl Send for GdkDisplay {}
// SAFETY: as above.
unsafe impl Sync for GdkDisplay {}

impl HasDisplayHandle for GdkDisplay {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(self.0));
        // SAFETY: the display outlives every borrow (see the Send impl).
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

/// Drive a wgpu future to completion on this thread. wgpu-core's native
/// backends complete adapter/device requests synchronously, so the first poll
/// is ready; the loop only guards against a backend that is not.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

struct Planes {
    size: (u32, u32),
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
}

pub struct WgpuPresenter {
    shared: Arc<Shared>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    config: wgpu::SurfaceConfiguration,
    planes: Option<Planes>,
}

/// Lower is better: real GPUs first, software rasterizers last; Vulkan before
/// GL at equal rank (decision #2: Vulkan, GL fallback).
fn adapter_rank(info: &wgpu::AdapterInfo) -> (u8, u8) {
    let device = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    let backend = match info.backend {
        wgpu::Backend::Vulkan => 0,
        _ => 1,
    };
    (device, backend)
}

impl WgpuPresenter {
    pub fn new(shared: Arc<Shared>) -> Result<Self, SpikeError> {
        let display = GdkDisplay(shared.raw_display());
        // WGPU_BACKEND=vulkan|gl overrides the choice (with_env), which is how
        // both paths are measured on one machine.
        let desc = wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display))
        }
        .with_env();
        let instance = wgpu::Instance::new(desc);
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                shared.raw_display(),
            ))),
            raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                shared.raw_video_surface(),
            )),
        };
        // SAFETY: both pointers live for the process (wl::RawPtrs), which
        // outlives this Surface.
        let surface = unsafe { instance.create_surface_unsafe(target) }
            .map_err(|e| SpikeError::NoAdapter(format!("create surface: {e}")))?;

        let mut adapters: Vec<wgpu::Adapter> =
            block_on(instance.enumerate_adapters(wgpu::Backends::all()))
                .into_iter()
                .filter(|a| a.is_surface_supported(&surface))
                .collect();
        adapters.sort_by_key(|a| adapter_rank(&a.get_info()));
        let adapter = adapters.into_iter().next().ok_or_else(|| {
            SpikeError::NoAdapter(
                "no Vulkan or OpenGL ES adapter supports this Wayland surface \
                 (install the Mesa Vulkan/EGL drivers for your GPU)"
                    .into(),
            )
        })?;
        let info = adapter.get_info();
        tracing::info!(
            "presenter: {:?} adapter \"{}\" ({:?}, driver {})",
            info.backend,
            info.name,
            info.device_type,
            info.driver_info
        );

        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("wayland video presenter"),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|e| SpikeError::Device(e.to_string()))?;

        let caps = surface.get_capabilities(&adapter);
        // A UNORM target: the shader outputs gamma-encoded R'G'B' already, so an
        // sRGB view would encode twice and wash the picture out.
        let format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|f| caps.formats.contains(f))
        .or_else(|| caps.formats.first().copied())
        .ok_or_else(|| SpikeError::NoAdapter("the surface offers no formats".into()))?;
        // Mailbox/Immediate never wait for a frame callback (the contract's "no
        // vsync wait"); FIFO is the last resort and is logged, because it paces
        // presents to the compositor and stalls while the window is occluded.
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        if present_mode == wgpu::PresentMode::Fifo {
            tracing::warn!(
                "presenter: only FIFO is available; presents are paced by the compositor"
            );
        }
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        let (width, height) = shared.lock_geo().physical();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            // One frame of queue depth (Presenter contract).
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        tracing::info!("presenter: {format:?}, {present_mode:?}, {alpha_mode:?}");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(include_str!("nv12.wgsl").into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[Some(&layout)],
            ..wgpu::PipelineLayoutDescriptor::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12 params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            shared,
            surface,
            device,
            queue,
            pipeline,
            layout,
            sampler,
            params,
            config,
            planes: None,
        })
    }

    fn upload(&mut self, width: u32, height: u32, y: CpuPlane<'_>, uv: CpuPlane<'_>) {
        if self.planes.as_ref().map(|p| p.size) != Some((width, height)) {
            self.planes = Some(self.create_planes(width, height));
        }
        let Some(planes) = &self.planes else {
            return;
        };
        let write = |tex: &wgpu::Texture, plane: CpuPlane<'_>, w: u32, h: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                plane.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(plane.stride as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&planes.y, y, width, height);
        write(&planes.uv, uv, width.div_ceil(2), height.div_ceil(2));
    }

    fn create_planes(&self, width: u32, height: u32) -> Planes {
        let tex = |label, w, h, format| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = tex("nv12 y", width, height, wgpu::TextureFormat::R8Unorm);
        let uv = tex(
            "nv12 uv",
            width.div_ceil(2),
            height.div_ceil(2),
            wgpu::TextureFormat::Rg8Unorm,
        );
        let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Planes {
            size: (width, height),
            y,
            uv,
            bind,
        }
    }

    fn write_params(
        &self,
        target: (u32, u32),
        video: (u32, u32),
        cursor: Option<(f64, f64)>,
        scale: f64,
    ) {
        let (ox, oy, dw, dh) = letterbox(
            target.0 as f64,
            target.1 as f64,
            video.0.max(1) as f64,
            video.1.max(1) as f64,
        );
        let (cx, cy, on) = match cursor {
            Some((x, y)) => (ox + x * dw, oy + y * dh, 1.0),
            None => (0.0, 0.0, 0.0),
        };
        let values = [ox, oy, dw, dh, cx, cy, on, 5.0 * scale];
        let mut bytes = [0u8; 32];
        for (chunk, v) in bytes.chunks_exact_mut(4).zip(values) {
            chunk.copy_from_slice(&(v as f32).to_ne_bytes());
        }
        self.queue.write_buffer(&self.params, 0, &bytes);
    }
}

impl Presenter for WgpuPresenter {
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        let FrameData::Cpu {
            format: PixelFormat::Nv12,
            plane0,
            plane1: Some(plane1),
        } = frame.data
        else {
            return Err(SurfaceError::Backend(
                "the Wayland presenter takes CPU NV12 frames (GPU import is task L5b)".into(),
            ));
        };
        self.shared.set_video_dims(frame.width, frame.height);
        // Cheap early out; the authoritative check is under the lock below.
        if !self.shared.is_shown() {
            return Ok(());
        }
        self.upload(frame.width, frame.height, plane0, plane1);

        // Size and scale are read once; the buffer is rendered at exactly this
        // size and the viewport sent with it is computed from it, so a resize or
        // reveal that lands meanwhile only takes effect on the next frame. The
        // lock is NOT held while rendering: configure/acquire/draw send no
        // surface requests, and the input thread takes the same lock on every
        // pointer motion.
        let (target, scale) = {
            let geo = self.shared.lock_geo();
            (geo.physical(), geo.scale())
        };
        if (self.config.width, self.config.height) != target {
            self.config.width = target.0;
            self.config.height = target.1;
            self.surface.configure(&self.device, &self.config);
        }
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            // Nothing to draw into this time; the next frame tries again.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(())
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(SurfaceError::Backend("swapchain validation error".into()))
            }
        };
        let Some(planes) = &self.planes else {
            return Ok(());
        };
        self.write_params(target, (frame.width, frame.height), cursor, scale);
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("video"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &planes.bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);

        // The surface lock covers the Wayland requests: Mesa's attach + damage +
        // commit (inside `present`) must not interleave with a reveal or
        // relayout commit, and the viewport must describe the committed buffer.
        let mut geo = self.shared.lock_geo();
        // Hidden: never present. The unmapped surface gets no frame callbacks,
        // so a FIFO swapchain would block in its next acquire forever; `show`
        // re-maps the last committed buffer. Dropping the acquired texture
        // unpresented is allowed (wgpu discards it).
        if !self.shared.is_shown() {
            return Ok(());
        }
        geo.attached = Some(target);
        self.shared.send_viewport(&geo.video_layout());
        self.queue.present(texture);
        Ok(())
    }

    /// CPU presenter (v1): decoders hand it CPU NV12.
    fn device(&self) -> Option<&dyn std::any::Any> {
        None
    }
}
