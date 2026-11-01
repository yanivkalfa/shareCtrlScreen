//! wgpu presenter for both Linux video surfaces: Vulkan first, GL (EGL) when
//! no Vulkan driver can draw into the window (VMs, WSLg) — decision #2.
//!
//! * CPU frames (`FrameData::Cpu`, NV12 from the decoders, BGRA) are uploaded
//!   and drawn by the shared [`surface_wgpu::Renderer`] (one WGSL pass: BT.709
//!   limited → RGB, letterbox, cursor). GPU frames from Linux decoders are a
//!   later task; anything else is refused with a reportable error.
//! * Low latency: Mailbox > Immediate > FIFO, one frame in flight
//!   (`desired_maximum_frame_latency = 1`, and the previous submission is
//!   waited for before the next frame is drawn).
//! * **Never presents while hidden.** An unmapped Wayland surface gets no
//!   frame callbacks, so a FIFO swapchain would block in its next acquire
//!   forever; on X11 it would be wasted work. The backend's [`Target::commit`]
//!   makes the hidden check and the present atomic with its own state.
//! * The buffer is always the **full** surface size from [`SurfaceState`],
//!   never the area left while the toolbar is revealed (the backends crop).
//! * The last frame can be drawn again ([`Repaint`]): X11 windows lose their
//!   contents when unmapped or covered (no compositor), and a static remote
//!   screen sends no new frame to fix that.
//!
//! ## References
//! * wgpu 30: `InstanceDescriptor::new_with_display_handle` ("on GLES this is
//!   required … especially for Wayland"), `InstanceDescriptor::with_env`
//!   (`WGPU_BACKEND`), `Instance::create_surface_unsafe` with
//!   `SurfaceTargetUnsafe::RawHandle`, `Instance::enumerate_adapters`,
//!   `Adapter::is_surface_supported`, `CurrentSurfaceTexture`,
//!   `Device::poll(PollType::Wait)`: <https://docs.rs/wgpu/30.0.1/wgpu/>
//! * wgpu-hal 30 GLES surface capabilities (FIFO only on Linux, non-sRGB
//!   `Rgba8Unorm`/`Bgra8Unorm` offered): `wgpu-hal-30.0.1/src/gles/adapter.rs`.
//! * Mesa Vulkan WSI, Wayland (FIFO waits on `wl_surface.frame`) and X11
//!   (Present extension): <https://gitlab.freedesktop.org/mesa/mesa/-/tree/main/src/vulkan/wsi>
//! * raw-window-handle 0.6 `HasDisplayHandle`: <https://docs.rs/raw-window-handle/0.6/>

use std::any::Any;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use platform_api::{Frame, FrameData, Presenter, RawDisplayHandle, RawWindowHandle, SurfaceError};
use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle};
use surface_wgpu::{DrawParams, Renderer, SurfaceState};

use super::lock;

/// How long to wait for the previous frame's GPU work. A healthy GPU finishes
/// a present in about a millisecond (a software rasterizer in tens); a second
/// means it is wedged, and waiting longer would freeze the viewer's media loop.
const GPU_WAIT: Duration = Duration::from_secs(1);

/// What the presenter draws into: one backend's native surface.
pub(crate) trait Target: Send + Sync + 'static {
    /// The handles wgpu draws through; valid for as long as `self` lives.
    fn handles(&self) -> (RawDisplayHandle, RawWindowHandle);
    fn state(&self) -> &SurfaceState;
    /// Cheap check: false while hidden (the authoritative one is in `commit`).
    fn is_shown(&self) -> bool;
    /// Publish a finished buffer rendered at `size`: call `present` unless the
    /// surface is hidden, atomically with whatever protocol state must
    /// describe that buffer. Returns whether it presented.
    fn commit(&self, size: (u32, u32), present: &mut dyn FnMut()) -> bool;
    /// Receive the presenter's repaint hook (backends that lose contents).
    fn set_repaint(&self, _hook: Weak<dyn Repaint>) {}
}

/// Draw the last frame again (contents lost, frame unchanged).
pub(crate) trait Repaint: Send + Sync {
    fn repaint(&self);
}

/// A display handle for the GL instance (EGL picks its platform display from
/// it at instance creation).
#[derive(Debug)]
struct DisplayHolder(RawDisplayHandle);

// SAFETY: the handle lives for the whole process and its users are
// serialized: on X11 it is tao's private Xlib `Display` (never closed, used by
// nobody but this presenter, whose draws run under the `Core` mutex — so EGL /
// Vulkan WSI never use it from two threads at once, which libX11 < 1.8 without
// `XInitThreads` would not survive); on Wayland it is GDK's `wl_display`,
// which libwayland makes thread-safe (one mutex per display) and GDK never
// closes while the app runs.
unsafe impl Send for DisplayHolder {}
// SAFETY: as above.
unsafe impl Sync for DisplayHolder {}

impl HasDisplayHandle for DisplayHolder {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: the display outlives every borrow (see the Send impl).
        Ok(unsafe { DisplayHandle::borrow_raw(self.0) })
    }
}

/// The frame drawn last, for [`Repaint`].
#[derive(Debug, Clone, Copy)]
struct Last {
    video: (u32, u32),
    cursor: Option<(f64, f64)>,
}

struct Core<T: Target> {
    target: Arc<T>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    config: wgpu::SurfaceConfiguration,
    /// Non-sRGB view of the swapchain texture (see `Renderer::new`).
    view_format: wgpu::TextureFormat,
    configured: bool,
    last_submission: Option<wgpu::SubmissionIndex>,
    last: Option<Last>,
    presented: u64,
}

/// The presenter. The GPU state sits behind a mutex shared with the
/// surface's repaint hook; the media thread and the repaint caller (the X11
/// event thread) take turns.
pub(crate) struct WgpuPresenter<T: Target> {
    core: Arc<Mutex<Core<T>>>,
}

/// Lower is better: real GPUs first, software rasterizers last; Vulkan before
/// GL at equal rank (decision #2: Vulkan, GL fallback).
fn adapter_rank(device_type: wgpu::DeviceType, backend: wgpu::Backend) -> (u8, u8) {
    let device = match device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    let backend = match backend {
        wgpu::Backend::Vulkan => 0,
        _ => 1,
    };
    (device, backend)
}

/// Prefer a plain (non-sRGB) 8-bit format, which stores the gamma-encoded
/// video as-is; otherwise an sRGB one, rendered through its non-sRGB view.
fn pick_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    use wgpu::TextureFormat as F;
    [F::Bgra8Unorm, F::Rgba8Unorm]
        .into_iter()
        .find(|f| formats.contains(f))
        .or_else(|| {
            formats
                .iter()
                .copied()
                .find(|f| matches!(f.remove_srgb_suffix(), F::Bgra8Unorm | F::Rgba8Unorm))
        })
}

/// Mailbox and Immediate never wait for the compositor (the contract's "no
/// vsync wait"); FIFO is the last resort (wgpu's GL backend offers only it).
fn pick_present_mode(modes: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
        .into_iter()
        .find(|m| modes.contains(m))
        .unwrap_or(wgpu::PresentMode::Fifo)
}

impl<T: Target> WgpuPresenter<T> {
    pub(crate) fn new(target: Arc<T>) -> Result<Self, SurfaceError> {
        let (display, window) = target.handles();
        // WGPU_BACKEND=vulkan|gl overrides the choice: the documented way to
        // try the other path on a machine where one misbehaves.
        let desc = wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(DisplayHolder(display)))
        }
        .with_env();
        let instance = wgpu::Instance::new(desc);
        // SAFETY: both handles stay valid as long as `target` lives (Target
        // contract), and the surface is dropped with `Core`, which holds it.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(display),
                raw_window_handle: window,
            })
        }
        .map_err(|e| SurfaceError::Backend(format!("cannot draw into the video surface: {e}")))?;

        let mut adapters: Vec<wgpu::Adapter> =
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
                .into_iter()
                .filter(|a| a.is_surface_supported(&surface))
                .collect();
        adapters.sort_by_key(|a| {
            let info = a.get_info();
            adapter_rank(info.device_type, info.backend)
        });
        let adapter = adapters.into_iter().next().ok_or_else(|| {
            SurfaceError::Backend(
                "no Vulkan or OpenGL driver can draw the remote screen into this window; install \
                 the Mesa Vulkan and EGL drivers (mesa-vulkan-drivers, libegl-mesa0) or your \
                 GPU vendor's driver"
                    .into(),
            )
        })?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ShareCtrl presenter"),
            // GL adapters only guarantee the WebGL2 baseline; large displays
            // need more than the default texture size, so take what this GPU
            // really supports.
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| {
            SurfaceError::Backend(format!("the GPU \"{}\" refused a device: {e}", info.name))
        })?;
        // wgpu's default reaction to a validation error is a panic, which
        // would take the whole app down over one bad frame. Log instead; the
        // engine also counts and reports failed presents.
        device.on_uncaptured_error(Arc::new(|e| {
            tracing::error!("presenter: wgpu error: {e}");
        }));

        let caps = surface.get_capabilities(&adapter);
        let format = pick_format(&caps.formats).ok_or_else(|| {
            SurfaceError::Backend("the video surface offers no 8-bit colour format".into())
        })?;
        let view_format = format.remove_srgb_suffix();
        let present_mode = pick_present_mode(&caps.present_modes);
        if present_mode == wgpu::PresentMode::Fifo {
            tracing::warn!("presenter: only FIFO is available; presents are paced by the display");
        }
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Srgb,
            width: 1,
            height: 1,
            present_mode,
            // One frame of queue depth (Presenter contract).
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: if view_format == format {
                Vec::new()
            } else {
                vec![view_format]
            },
        };
        let renderer = Renderer::new(&device, &queue, view_format);
        tracing::info!(
            "presenter: {:?} adapter \"{}\" ({:?}, driver {}), {format:?}, {present_mode:?}",
            info.backend,
            info.name,
            info.device_type,
            info.driver_info
        );
        let core = Arc::new(Mutex::new(Core {
            target: target.clone(),
            surface,
            device,
            queue,
            renderer,
            config,
            view_format,
            configured: false,
            last_submission: None,
            last: None,
            presented: 0,
        }));
        let weak = Arc::downgrade(&core);
        let hook: Weak<dyn Repaint> = weak;
        target.set_repaint(hook);
        Ok(Self { core })
    }
}

impl<T: Target> Core<T> {
    /// Wait for the previous frame's GPU work: at most one frame queued, so
    /// latency never builds up behind a slow GPU.
    fn wait_previous(&mut self) -> Result<(), SurfaceError> {
        let Some(index) = self.last_submission.take() else {
            return Ok(());
        };
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(GPU_WAIT),
            })
            .map(|_| ())
            .map_err(|e| {
                SurfaceError::Backend(format!("the GPU did not finish the previous frame: {e}"))
            })
    }

    /// Match the swapchain to the full surface size (no-op when unchanged).
    fn sync_size(&mut self, size: (u32, u32)) {
        if self.configured && (self.config.width, self.config.height) == size {
            return;
        }
        self.config.width = size.0;
        self.config.height = size.1;
        self.surface.configure(&self.device, &self.config);
        self.configured = true;
    }

    /// The next swapchain texture, or `None` when this frame should be skipped.
    fn acquire(&mut self) -> Result<Option<wgpu::SurfaceTexture>, SurfaceError> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Ok(Some(t)),
            // Window covered or minimised, or no buffer free right now: skip;
            // the next frame follows within a frame interval.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configured = false;
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                // Recreated on the next frame; reconfiguring is how wgpu
                // recovers a lost swapchain.
                self.configured = false;
                Err(SurfaceError::Backend(
                    "the video surface's swapchain was lost; it is recreated on the next frame"
                        .into(),
                ))
            }
            wgpu::CurrentSurfaceTexture::Validation => Err(SurfaceError::Backend(
                "the GPU rejected the video surface's swapchain (details are in the log)".into(),
            )),
        }
    }

    /// Draw the uploaded frame at the current size and publish it.
    fn draw(&mut self, last: Last) -> Result<(), SurfaceError> {
        let size = self.target.state().size();
        if size.0 == 0 || size.1 == 0 {
            // Not laid out yet: nothing to draw on.
            return Ok(());
        }
        self.sync_size(size);
        let Some(texture) = self.acquire()? else {
            return Ok(());
        };
        let Some(source) = self.renderer.uploaded() else {
            return Err(SurfaceError::Backend("uploaded frame went missing".into()));
        };
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        self.renderer.draw(
            &mut encoder,
            &view,
            source,
            DrawParams {
                target: size,
                video: last.video,
                cursor: last.cursor,
                scale: self.target.state().scale(),
            },
        );
        self.last_submission = Some(self.queue.submit([encoder.finish()]));
        let queue = &self.queue;
        let mut texture = Some(texture);
        // Not presented (hidden meanwhile): the texture is dropped unpresented,
        // which wgpu allows; `show` maps the last committed buffer.
        self.target.commit(size, &mut || {
            if let Some(t) = texture.take() {
                queue.present(t);
            }
        });
        self.last = Some(last);
        Ok(())
    }

    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        self.wait_previous()?;
        if let FrameData::Native(_) = frame.data {
            return Err(SurfaceError::Backend(
                "this frame lives on the GPU of another backend; the Linux presenter takes CPU \
                 frames"
                    .into(),
            ));
        }
        let video = (frame.width, frame.height);
        // Publish before drawing: input arriving while this frame is on screen
        // must normalize over its letterbox.
        self.target.state().set_video_size(video.0, video.1);
        if !self.target.is_shown() {
            return Ok(());
        }
        self.renderer.upload(frame)?;
        self.draw(Last { video, cursor })?;
        self.presented += 1;
        if self.presented == 1 {
            let (w, h) = self.target.state().size();
            tracing::info!("presenter: first frame presented ({w}x{h})");
        }
        Ok(())
    }

    fn repaint(&mut self) {
        let Some(last) = self.last else {
            return;
        };
        if !self.target.is_shown() {
            return;
        }
        if let Err(e) = self.wait_previous().and_then(|()| self.draw(last)) {
            tracing::warn!("presenter: repaint failed: {e}");
        }
    }
}

impl<T: Target> Repaint for Mutex<Core<T>> {
    fn repaint(&self) {
        lock(self).repaint();
    }
}

impl<T: Target> Presenter for WgpuPresenter<T> {
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        lock(&self.core).present(frame, cursor)
    }

    /// `None`: a CPU presenter (v1); decoders hand it CPU frames.
    fn device(&self) -> Option<&dyn Any> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_gpus_beat_software_and_vulkan_beats_gl() {
        use wgpu::{Backend as B, DeviceType as D};
        let mut adapters = vec![
            (D::Cpu, B::Vulkan),
            (D::IntegratedGpu, B::Gl),
            (D::IntegratedGpu, B::Vulkan),
            (D::Cpu, B::Gl),
            (D::DiscreteGpu, B::Gl),
        ];
        adapters.sort_by_key(|&(d, b)| adapter_rank(d, b));
        assert_eq!(
            adapters,
            vec![
                (D::DiscreteGpu, B::Gl),
                (D::IntegratedGpu, B::Vulkan),
                (D::IntegratedGpu, B::Gl),
                (D::Cpu, B::Vulkan),
                (D::Cpu, B::Gl),
            ]
        );
    }

    #[test]
    fn prefers_plain_8_bit_formats() {
        use wgpu::TextureFormat as F;
        assert_eq!(
            pick_format(&[F::Bgra8UnormSrgb, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
        // GL offers Rgba8Unorm first.
        assert_eq!(
            pick_format(&[F::Rgba8Unorm, F::Bgra8Unorm, F::Rgba8UnormSrgb]),
            Some(F::Bgra8Unorm)
        );
        assert_eq!(
            pick_format(&[F::Rgba16Float, F::Rgba8UnormSrgb]),
            Some(F::Rgba8UnormSrgb)
        );
        assert_eq!(pick_format(&[F::Rgba16Float, F::Rgb10a2Unorm]), None);
    }

    #[test]
    fn present_mode_never_waits_when_it_can_avoid_it() {
        use wgpu::PresentMode as P;
        assert_eq!(pick_present_mode(&[P::Fifo, P::Immediate]), P::Immediate);
        assert_eq!(
            pick_present_mode(&[P::Fifo, P::Immediate, P::Mailbox]),
            P::Mailbox
        );
        assert_eq!(pick_present_mode(&[P::Fifo]), P::Fifo);
        assert_eq!(pick_present_mode(&[]), P::Fifo);
    }
}
