//! Zero-copy import of an IOSurface-backed `CVPixelBuffer` into wgpu: each
//! plane becomes an `MTLTexture` through `CVMetalTextureCache`, wrapped with
//! `wgpu-hal`'s `texture_from_raw` + `Device::create_texture_from_hal`. No
//! pixel is copied; the GPU samples the decoder's buffer directly.
//!
//! Lifetime rule (Apple: "retain CVMetalTexture objects until you are done
//! using the images in them"): each wgpu texture owns its `CVMetalTexture`
//! through the hal drop callback, and wgpu-core destroys a texture only after
//! the last submission using it has completed.
//!
//! ## References
//! * `CVMetalTextureCache`, `CVMetalTextureCacheCreateTextureFromImage`
//!   (R8 luma / RG8 half-size chroma for `420v`):
//!   <https://developer.apple.com/documentation/corevideo/cvmetaltexturecache>
//! * `CVMetalTextureCacheFlush`:
//!   <https://developer.apple.com/documentation/corevideo/cvmetaltexturecacheflush(_:_:)>
//! * `wgpu::Device::create_texture_from_hal`:
//!   <https://docs.rs/wgpu/30.0.1/wgpu/struct.Device.html#method.create_texture_from_hal>
//! * `wgpu_hal::metal::Device::texture_from_raw`:
//!   <https://docs.rs/wgpu-hal/30.0.1/wgpu_hal/metal/struct.Device.html#method.texture_from_raw>
//! * Prior art (plan 01 §4): moq-video `src/render/metal.rs`, frust-gpu
//!   `src/import/apple.rs`.

use std::ptr::NonNull;

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVReturnSuccess, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture,
};
use objc2_metal::{MTLPixelFormat, MTLTextureType};
use platform_api::SurfaceError;
use surface_wgpu::Source;

use crate::gpu::{fourcc, CvPixelBufferFrame, PixelLayout};

/// What an imported plane must outlive: its `CVMetalTexture`. Holding it is
/// how CoreVideo is told the image is still in use — the binding to the
/// buffer stays live and the buffer's pool does not recycle it. No extra
/// reference to the buffer itself is kept, so it goes back to its pool (the
/// decoder's, or ScreenCaptureKit's, which stalls when it runs dry) as soon
/// as the GPU has finished with it.
struct PlaneKeepAlive {
    _texture: CFRetained<CVMetalTexture>,
}

// SAFETY: only released from wgpu-hal's drop callback, possibly on another
// thread; CoreFoundation retain/release is thread-safe and nothing reads
// through this reference.
unsafe impl Send for PlaneKeepAlive {}
// SAFETY: see `Send`.
unsafe impl Sync for PlaneKeepAlive {}

/// The planes of one imported frame.
pub(crate) struct Imported {
    plane0: wgpu::TextureView,
    plane1: Option<wgpu::TextureView>,
    full_range: bool,
    size: (u32, u32),
}

impl Imported {
    pub(crate) fn source(&self) -> Source<'_> {
        match &self.plane1 {
            Some(chroma) => Source::Nv12 {
                luma: &self.plane0,
                chroma,
                full_range: self.full_range,
            },
            None => Source::Rgb(&self.plane0),
        }
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.size
    }
}

/// A `CVMetalTextureCache` bound to one wgpu Metal device.
pub(crate) struct Importer {
    device: wgpu::Device,
    cache: CFRetained<CVMetalTextureCache>,
}

impl Importer {
    pub(crate) fn new(device: &wgpu::Device) -> Result<Self, SurfaceError> {
        // SAFETY: only reads the raw MTLDevice handle; nothing is destroyed.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.ok_or_else(|| {
            SurfaceError::Backend("the presenter's GPU is not a Metal device".into())
        })?;
        let mut out: *mut CVMetalTextureCache = std::ptr::null_mut();
        // SAFETY: valid MTLDevice; `out` is a valid out-pointer; default
        // cache and texture attributes.
        let rc = unsafe {
            CVMetalTextureCache::create(None, None, hal.raw_device(), None, NonNull::from(&mut out))
        };
        let raw = NonNull::new(out)
            .filter(|_| rc == kCVReturnSuccess)
            .ok_or_else(|| {
                SurfaceError::Backend(format!(
                    "CoreVideo could not create a Metal texture cache (error {rc})"
                ))
            })?;
        Ok(Self {
            device: device.clone(),
            // SAFETY: a CoreFoundation "Create" function returns a +1
            // reference, which we now own.
            cache: unsafe { CFRetained::from_raw(raw) },
        })
    }

    /// Let the cache recycle mappings of textures that were freed. Apple asks
    /// for this to be called periodically; once per presented frame is the
    /// usual cadence.
    pub(crate) fn flush(&self) {
        self.cache.flush(0);
    }

    pub(crate) fn import(&self, buffer: &CvPixelBufferFrame) -> Result<Imported, SurfaceError> {
        let size = buffer.size();
        if size.0 == 0 || size.1 == 0 {
            return Err(SurfaceError::Backend(format!(
                "decoder produced an empty {}x{} frame",
                size.0, size.1
            )));
        }
        let layout = buffer.layout().ok_or_else(|| {
            SurfaceError::Backend(format!(
                "decoder produced pixel format '{}', which the presenter cannot draw",
                fourcc(buffer.pixel_format_type())
            ))
        })?;
        let chroma = (size.0.div_ceil(2), size.1.div_ceil(2));
        Ok(match layout {
            PixelLayout::Nv12Limited | PixelLayout::Nv12Full => Imported {
                plane0: self.plane(buffer, 0, wgpu::TextureFormat::R8Unorm, size)?,
                plane1: Some(self.plane(buffer, 1, wgpu::TextureFormat::Rg8Unorm, chroma)?),
                full_range: layout == PixelLayout::Nv12Full,
                size,
            },
            PixelLayout::Bgra => Imported {
                plane0: self.plane(buffer, 0, wgpu::TextureFormat::Bgra8Unorm, size)?,
                plane1: None,
                full_range: true,
                size,
            },
        })
    }

    fn plane(
        &self,
        buffer: &CvPixelBufferFrame,
        plane: usize,
        format: wgpu::TextureFormat,
        (w, h): (u32, u32),
    ) -> Result<wgpu::TextureView, SurfaceError> {
        let mtl_format = match format {
            wgpu::TextureFormat::R8Unorm => MTLPixelFormat::R8Unorm,
            wgpu::TextureFormat::Rg8Unorm => MTLPixelFormat::RG8Unorm,
            _ => MTLPixelFormat::BGRA8Unorm,
        };
        let mut out: *mut CVMetalTexture = std::ptr::null_mut();
        // SAFETY: valid cache and pixel buffer; `out` is a valid out-pointer;
        // no texture attributes. A buffer that is not IOSurface-backed makes
        // the call fail with an error code, which is handled below.
        let rc = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                buffer.pixel_buffer(),
                None,
                mtl_format,
                w as usize,
                h as usize,
                plane,
                NonNull::from(&mut out),
            )
        };
        let raw = NonNull::new(out)
            .filter(|_| rc == kCVReturnSuccess)
            .ok_or_else(|| {
                SurfaceError::Backend(format!(
                "cannot map decoded frame plane {plane} to the GPU (CoreVideo error {rc}); the \
                 decoder must output Metal-compatible IOSurface buffers"
            ))
            })?;
        // SAFETY: a CoreFoundation "Create" function returns a +1 reference.
        let cv_texture = unsafe { CFRetained::from_raw(raw) };
        let mtl = CVMetalTextureGetTexture(&cv_texture).ok_or_else(|| {
            SurfaceError::Backend(format!(
                "CoreVideo returned no Metal texture for plane {plane}"
            ))
        })?;
        let keep = PlaneKeepAlive {
            _texture: cv_texture,
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("decoded plane"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: `mtl` is a 2D, single-mip, single-layer texture of exactly
        // this format and size, created on this device's MTLDevice by the
        // cache, and fully written by the decoder before it handed the buffer
        // on. Metal has no layouts, so `RESOURCE` is a valid initial state.
        let texture = unsafe {
            let hal = wgpu::hal::metal::Device::texture_from_raw(
                mtl,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width: w,
                    height: h,
                    depth: 1,
                },
                Some(Box::new(move || drop(keep))),
            );
            self.device
                .create_texture_from_hal::<wgpu::hal::api::Metal>(
                    hal,
                    &desc,
                    wgpu::TextureUses::RESOURCE,
                )
        };
        Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
    }
}

#[cfg(test)]
mod tests {
    //! Runs on any Mac with Metal, including GitHub's macOS runners (no
    //! window, no permissions): a real `420v` IOSurface buffer goes through
    //! the zero-copy import and the shared shader into an offscreen texture.

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::{CFDictionary, CFString};
    use objc2_core_video::{
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferCreate,
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    use surface_wgpu::{DrawParams, Renderer};

    use super::*;

    fn gpu() -> (wgpu::Device, wgpu::Queue) {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
            .unwrap_or_else(|e| panic!("no Metal adapter: {e}"));
        pollster::block_on(adapter.request_device(&Default::default()))
            .unwrap_or_else(|e| panic!("no Metal device: {e}"))
    }

    /// Toll-free bridge: a CFString constant is an NSString.
    fn ns(key: &CFString) -> &NSString {
        // SAFETY: CFString and NSString are toll-free bridged.
        unsafe { &*(key as *const CFString).cast::<NSString>() }
    }

    /// A solid `420v` IOSurface-backed pixel buffer.
    fn nv12_buffer(w: usize, h: usize, (y, cb, cr): (u8, u8, u8)) -> CvPixelBufferFrame {
        let empty: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
        let yes = NSNumber::new_bool(true);
        // SAFETY: CoreVideo key constants, valid for the process lifetime.
        let (io_key, metal_key) = unsafe {
            (
                ns(kCVPixelBufferIOSurfacePropertiesKey),
                ns(kCVPixelBufferMetalCompatibilityKey),
            )
        };
        let attrs = NSDictionary::<NSString, AnyObject>::from_slices(
            &[io_key, metal_key],
            &[&*empty as &AnyObject, &*yes as &AnyObject],
        );
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary; `out` is
        // a valid out-pointer.
        let rc = unsafe {
            CVPixelBufferCreate(
                None,
                w,
                h,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(&*Retained::as_ptr(&attrs).cast::<CFDictionary>()),
                NonNull::from(&mut out),
            )
        };
        assert_eq!(rc, kCVReturnSuccess, "CVPixelBufferCreate");
        let raw = NonNull::new(out).unwrap_or_else(|| panic!("CVPixelBufferCreate gave null"));
        // SAFETY: "Create" returns a +1 reference.
        let buffer = unsafe { CFRetained::from_raw(raw) };
        // SAFETY: lock/unlock bracket CPU writes into the buffer's planes,
        // each written within its reported bytes-per-row × rows.
        unsafe {
            assert_eq!(
                CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)),
                kCVReturnSuccess
            );
            for (plane, rows, fill) in [(0usize, h, None), (1, h.div_ceil(2), Some((cb, cr)))] {
                let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                let bytes = std::slice::from_raw_parts_mut(base, stride * rows);
                match fill {
                    None => bytes.fill(y),
                    Some((cb, cr)) => {
                        for pair in bytes.chunks_exact_mut(2) {
                            pair[0] = cb;
                            pair[1] = cr;
                        }
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        CvPixelBufferFrame::new(buffer)
    }

    #[test]
    fn zero_copy_nv12_import_renders_bt709_colours() {
        let (device, queue) = gpu();
        let importer = Importer::new(&device).unwrap_or_else(|e| panic!("{e}"));
        let renderer = Renderer::new(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        // BT.709 red, limited range.
        let buffer = nv12_buffer(16, 16, (63, 102, 240));
        let imported = importer.import(&buffer).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(imported.size(), (16, 16));

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: 256 * 64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.draw(
            &mut encoder,
            &view,
            imported.source(),
            DrawParams {
                target: (64, 64),
                video: imported.size(),
                cursor: None,
                scale: 1.0,
            },
        );
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            target.size(),
        );
        queue.submit([encoder.finish()]);
        drop(imported);
        readback.map_async(wgpu::MapMode::Read, .., |r| {
            if let Err(e) = r {
                panic!("map failed: {e}");
            }
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap_or_else(|e| panic!("GPU wait failed: {e}"));
        importer.flush();
        let px = readback
            .get_mapped_range(..)
            .unwrap_or_else(|e| panic!("readback: {e}"));
        let at = |x: usize, y: usize| {
            let i = y * 256 + x * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        for (x, y) in [(1, 1), (32, 32), (62, 62)] {
            let [r, g, b] = at(x, y);
            assert!(r >= 252 && g <= 3 && b <= 3, "({x},{y}) = {:?}", at(x, y));
        }
    }

    #[test]
    fn unsupported_pixel_formats_are_reported_not_drawn() {
        let (device, _queue) = gpu();
        let importer = Importer::new(&device).unwrap_or_else(|e| panic!("{e}"));
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // '2vuy' (packed 4:2:2), which the presenter does not draw.
        const K_2VUY: u32 = 0x3276_7579;
        // SAFETY: valid out-pointer, no attributes.
        let rc = unsafe { CVPixelBufferCreate(None, 8, 8, K_2VUY, None, NonNull::from(&mut out)) };
        assert_eq!(rc, kCVReturnSuccess);
        let raw = NonNull::new(out).unwrap_or_else(|| panic!("null buffer"));
        // SAFETY: "Create" returns a +1 reference.
        let buffer = CvPixelBufferFrame::new(unsafe { CFRetained::from_raw(raw) });
        let err = importer.import(&buffer).err().map(|e| e.to_string());
        assert!(err.is_some_and(|e| e.contains("2vuy")));
    }
}
