//! wgpu (Metal backend) presenter for the macOS video view.
//!
//! * GPU frames ([`CvPixelBufferFrame`], from the VideoToolbox decoder) are imported
//!   zero-copy by [`Importer`].
//! * CPU frames (`FrameData::Cpu`, the software codec path) are uploaded by
//!   the shared [`surface_wgpu::Renderer`].
//! * Presentation: `Immediate` (= `displaySyncEnabled = false`, uncapped like
//!   the Windows `ALLOW_TEARING`), frame latency 1 (= `maximumDrawableCount =
//!   2`), `presentsWithTransaction = false` (set on the layer by the view).
//! * Drawable = the **full** surface size from [`SurfaceState`], never the
//!   area left while the toolbar is revealed (the view crops; see `view.rs`).
//!
//! ## References
//! * `CAMetalLayer` (`displaySyncEnabled`, `maximumDrawableCount`,
//!   `presentsWithTransaction`, rendering on a background thread):
//!   <https://developer.apple.com/documentation/quartzcore/cametallayer>
//! * `CATransaction` (explicit transactions on threads without a run loop):
//!   <https://developer.apple.com/documentation/quartzcore/catransaction>
//! * wgpu `Instance::create_surface_unsafe` with
//!   `SurfaceTargetUnsafe::CoreAnimationLayer`, `Surface::configure`,
//!   `PresentMode::Immediate`, `desired_maximum_frame_latency`,
//!   `Device::poll(PollType::Wait)`: <https://docs.rs/wgpu/30.0.1/wgpu/>
//! * wgpu-hal Metal `configure` (maps latency → `maximumDrawableCount =
//!   latency + 1`, `Immediate` → `displaySyncEnabled = false`):
//!   `wgpu-hal-30.0.1/src/metal/surface.rs`.

use std::any::Any;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_quartz_core::{CAMetalLayer, CATransaction};
use platform_api::{Frame, FrameData, Presenter, SurfaceError};
use surface_wgpu::{DrawParams, Renderer, Source, SurfaceState};

use crate::gpu::CvPixelBufferFrame;
use crate::import::Importer;

/// The view's `CAMetalLayer`, shareable with the viewer media thread.
pub(crate) struct SharedLayer(Retained<CAMetalLayer>);

// SAFETY: Apple documents rendering into a `CAMetalLayer` (`nextDrawable`,
// presenting) from a background thread, and wgpu-hal keeps the layer behind
// its own mutex. The only layer state set off the main thread is what wgpu's
// `configure` sets (device, pixel format, drawable size and count, display
// sync), inside an explicit `CATransaction`; the geometry AppKit owns (frame,
// gravity, scale) is only set by `VideoView` on the main thread.
unsafe impl Send for SharedLayer {}
// SAFETY: see `Send`.
unsafe impl Sync for SharedLayer {}

impl SharedLayer {
    pub(crate) fn new(layer: Retained<CAMetalLayer>) -> Self {
        Self(layer)
    }
}

/// How long to wait for the previous frame's GPU work. A healthy GPU finishes
/// a present in about a millisecond; a second means it is wedged, and waiting
/// longer would freeze the viewer's media loop.
const GPU_WAIT: Duration = Duration::from_secs(1);

pub(crate) struct MetalPresenter {
    state: Arc<SurfaceState>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    importer: Importer,
    config: wgpu::SurfaceConfiguration,
    /// Non-sRGB view of the drawable (see `Renderer::new`).
    view_format: wgpu::TextureFormat,
    configured: bool,
    last_submission: Option<wgpu::SubmissionIndex>,
    presented: u64,
}

impl MetalPresenter {
    pub(crate) fn new(layer: &SharedLayer, state: Arc<SurfaceState>) -> Result<Self, SurfaceError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        // SAFETY: the pointer is a live CAMetalLayer, and wgpu-hal retains it,
        // so the layer lives as long as the surface does.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                Retained::as_ptr(&layer.0) as *mut c_void,
            ))
        }
        .map_err(|e| SurfaceError::Backend(format!("cannot draw into the video view: {e}")))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            // The work is one full-screen quad. On dual-GPU MacBooks the
            // integrated GPU is plenty, and asking for the discrete one would
            // power it up (battery drain, and a display mux switch).
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| {
            SurfaceError::Backend(format!(
                "no Metal GPU available to show the remote screen ({e}); this Mac needs macOS 13 \
                 or later with Metal support"
            ))
        })?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ShareCtrl presenter"),
            // Large external displays (6K and up) exceed the WebGPU default
            // texture size; take what this GPU really supports.
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| SurfaceError::Backend(format!("the Metal GPU refused a device: {e}")))?;
        // wgpu's default reaction to a validation error is a panic, which
        // would take the whole app down over one bad frame. Log instead; the
        // engine also counts and reports failed presents.
        device.on_uncaptured_error(Arc::new(|e| {
            tracing::error!("presenter: wgpu error: {e}");
        }));

        let caps = surface.get_capabilities(&adapter);
        let format = pick_format(&caps.formats).ok_or_else(|| {
            SurfaceError::Backend("the video view offers no 8-bit colour format".into())
        })?;
        let view_format = format.remove_srgb_suffix();
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Srgb,
            width: 1,
            height: 1,
            present_mode,
            // One frame in flight → maximumDrawableCount = 2 (wgpu adds one).
            desired_maximum_frame_latency: 1,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: if view_format == format {
                Vec::new()
            } else {
                vec![view_format]
            },
        };
        let renderer = Renderer::new(&device, &queue, view_format);
        let importer = Importer::new(&device)?;
        tracing::info!(
            "presenter: Metal on {}, {format:?}, {present_mode:?}",
            adapter.get_info().name
        );
        Ok(Self {
            state,
            surface,
            device,
            queue,
            renderer,
            importer,
            config,
            view_format,
            configured: false,
            last_submission: None,
            presented: 0,
        })
    }

    /// Match the drawable to the full surface size (no-op when unchanged).
    fn sync_size(&mut self, size: (u32, u32)) {
        if self.configured && (self.config.width, self.config.height) == size {
            return;
        }
        self.config.width = size.0;
        self.config.height = size.1;
        // wgpu sets the layer's drawable size, format and count here, on this
        // non-main thread. Such a thread has no run loop to commit an implicit
        // transaction, so commit explicitly.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.surface.configure(&self.device, &self.config);
        CATransaction::commit();
        self.configured = true;
    }

    /// Wait for the previous frame's GPU work — the Metal analogue of the
    /// Windows wait-first-then-render: at most one frame queued, so latency
    /// never builds up behind a slow GPU.
    fn wait_previous(&mut self) -> Result<(), SurfaceError> {
        let Some(index) = self.last_submission.take() else {
            return Ok(());
        };
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(GPU_WAIT),
            })
            .map_err(|e| {
                SurfaceError::Backend(format!("the GPU did not finish the previous frame: {e}"))
            })?;
        // The poll freed the previous frame's imported planes; let the cache
        // recycle their mappings.
        self.importer.flush();
        Ok(())
    }

    /// The next drawable, or `None` when this frame should be skipped.
    fn acquire(&mut self) -> Result<Option<wgpu::SurfaceTexture>, SurfaceError> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Ok(Some(t)),
            // Window minimised or covered, or no drawable free right now:
            // skip; the next frame follows within a frame interval.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configured = false;
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Lost => Err(SurfaceError::Backend(
                "the video view's drawable was lost; reconnect to restore the picture".into(),
            )),
            wgpu::CurrentSurfaceTexture::Validation => Err(SurfaceError::Backend(
                "the GPU rejected the video view's drawable (details are in the log)".into(),
            )),
        }
    }

    /// Draw `source` into `drawable`, submit, present.
    fn render(
        &self,
        drawable: wgpu::SurfaceTexture,
        source: Source<'_>,
        video: (u32, u32),
        cursor: Option<(f64, f64)>,
    ) -> wgpu::SubmissionIndex {
        let view = drawable.texture.create_view(&wgpu::TextureViewDescriptor {
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
                target: (self.config.width, self.config.height),
                video,
                cursor,
                scale: self.state.scale(),
            },
        );
        let index = self.queue.submit([encoder.finish()]);
        self.queue.present(drawable);
        index
    }
}

impl Presenter for MetalPresenter {
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        self.wait_previous()?;
        let size = self.state.size();
        if size.0 == 0 || size.1 == 0 {
            // Not laid out yet: nothing to draw on.
            return Ok(());
        }
        self.sync_size(size);
        let video = (frame.width, frame.height);
        // Publish before drawing: input arriving while this frame is on screen
        // must normalize over its letterbox.
        self.state.set_video_size(video.0, video.1);
        // Import or upload before taking a drawable, so a bad frame never
        // holds one of the two drawables hostage.
        let index = match frame.data {
            FrameData::Native(any) => {
                let buffer = any.downcast_ref::<CvPixelBufferFrame>().ok_or_else(|| {
                    SurfaceError::Backend(
                        "frame comes from a non-macOS backend; only CVPixelBuffer frames can be \
                         shown without a copy"
                            .into(),
                    )
                })?;
                let imported = self.importer.import(buffer)?;
                let Some(drawable) = self.acquire()? else {
                    return Ok(());
                };
                self.render(drawable, imported.source(), imported.size(), cursor)
            }
            FrameData::Cpu { .. } => {
                self.renderer.upload(frame)?;
                let Some(drawable) = self.acquire()? else {
                    return Ok(());
                };
                let Some(source) = self.renderer.uploaded() else {
                    return Err(SurfaceError::Backend("uploaded frame went missing".into()));
                };
                self.render(drawable, source, video, cursor)
            }
        };
        self.last_submission = Some(index);
        self.presented += 1;
        if self.presented == 1 {
            tracing::info!("presenter: first frame presented ({}x{})", size.0, size.1);
        }
        Ok(())
    }

    /// `None`: VideoToolbox decodes into IOSurface-backed pixel buffers that
    /// any Metal device can map, so there is no device to share (unlike the
    /// D3D11 path, where decoder and swapchain must use one device).
    fn device(&self) -> Option<&dyn Any> {
        None
    }
}

/// Prefer a plain `Bgra8Unorm` drawable (CAMetalLayer's native format, and
/// non-sRGB so the gamma-encoded video is stored as-is); otherwise any 8-bit
/// RGBA/BGRA format, rendered through its non-sRGB view.
fn pick_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    use wgpu::TextureFormat as F;
    if formats.contains(&F::Bgra8Unorm) {
        return Some(F::Bgra8Unorm);
    }
    formats
        .iter()
        .copied()
        .find(|f| matches!(f.remove_srgb_suffix(), F::Bgra8Unorm | F::Rgba8Unorm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_plain_bgra_and_falls_back_to_8_bit_formats() {
        use wgpu::TextureFormat as F;
        assert_eq!(
            pick_format(&[F::Bgra8UnormSrgb, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
        assert_eq!(
            pick_format(&[F::Rgba16Float, F::Bgra8UnormSrgb]),
            Some(F::Bgra8UnormSrgb)
        );
        assert_eq!(pick_format(&[F::Rgba16Float, F::Rgb10a2Unorm]), None);
    }
}
