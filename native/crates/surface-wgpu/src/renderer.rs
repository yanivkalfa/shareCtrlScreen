//! The wgpu half of a presenter: pipelines for the shared WGSL shader, CPU
//! frame upload, and one draw call sequence (clear → letterboxed video →
//! cursor sprite) into any render target. Surface acquisition, zero-copy GPU
//! import and presentation stay in the backend (`platform-macos`,
//! `platform-linux`), which hand this module texture views.
//!
//! ## References
//! * wgpu 30 API: <https://docs.rs/wgpu/30.0.1/wgpu/>
//! * `Queue::write_texture` (no row-alignment requirement, unlike buffer
//!   copies): <https://docs.rs/wgpu/30.0.1/wgpu/struct.Queue.html#method.write_texture>
//! * WebGPU viewport rules (why geometry is in the shader, not a viewport):
//!   <https://www.w3.org/TR/webgpu/#dom-gpurenderpassencoder-setviewport>
//! * BT.709 matrix: ITU-R BT.709-6, Table 3 items 3.2–3.3.
//! * Cursor image blending: `wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING`
//!   (`One`, `OneMinusSrcAlpha`), the over operator for premultiplied pixels
//!   (Porter & Duff 1984); the image arrives premultiplied (`protocol::cursor`).

use platform_api::{CursorSprite, Frame, FrameData, PixelFormat, SurfaceError};

use crate::geometry::{cursor_rect, letterbox, sprite_rect, RectF};

/// The presenter shader (NV12 BT.709 → RGB, letterbox, cursor). Public so a
/// backend can validate or reuse it.
pub const SHADER_WGSL: &str = include_str!("../shaders/present.wgsl");

/// Size of the `Params` uniform block in the shader: two `vec4<f32>`, one
/// `vec2<f32>` and two `u32` = 48 bytes (a multiple of 16, as uniform blocks
/// require).
const PARAMS_SIZE: usize = 48;

/// Where the pixels to draw are, as texture views the backend owns (imported
/// GPU planes) or the renderer owns (CPU uploads).
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// Two-plane 4:2:0: `luma` R8Unorm at full size, `chroma` Rg8Unorm (Cb, Cr)
    /// at half size.
    Nv12 {
        luma: &'a wgpu::TextureView,
        chroma: &'a wgpu::TextureView,
        /// `false` for BT.709 limited range (the codec default), `true` for
        /// full range.
        full_range: bool,
    },
    /// Any RGB(A) colour texture (BGRA capture frames, software decoders).
    Rgb(&'a wgpu::TextureView),
}

/// What one draw needs besides the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawParams {
    /// Render-target size in pixels (the full surface).
    pub target: (u32, u32),
    /// Source picture size in pixels (drives the letterbox aspect).
    pub video: (u32, u32),
    /// Cursor sprite position normalized over the video; `None` = no sprite.
    pub cursor: Option<(f64, f64)>,
    /// Backing scale factor, for a constant on-screen cursor size.
    pub scale: f64,
}

/// Textures the renderer owns for CPU frames, kept across frames and only
/// recreated when the size or format changes (allocation per frame would
/// churn GPU memory at 60 fps).
struct Upload {
    format: PixelFormat,
    size: (u32, u32),
    plane0: wgpu::Texture,
    view0: wgpu::TextureView,
    plane1: Option<(wgpu::Texture, wgpu::TextureView)>,
}

/// Pipelines and resources for one render-target format.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    nv12: wgpu::RenderPipeline,
    rgb: wgpu::RenderPipeline,
    cursor: wgpu::RenderPipeline,
    /// The host's cursor image (D9), blended premultiplied.
    sprite_pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    /// Bound as plane 1 for RGB sources and as the cursor image when there is
    /// none: every binding in the layout must be filled even when the fragment
    /// entry point does not read it.
    placeholder: wgpu::TextureView,
    upload: Option<Upload>,
    sprite: Option<Sprite>,
}

/// The host's cursor image on the GPU.
struct Sprite {
    view: wgpu::TextureView,
    /// Geometry for [`sprite_rect`] (size, hotspot, host frame size).
    image: CursorSprite,
}

impl Renderer {
    /// Build the pipelines for `target_format`.
    ///
    /// `target_format` must be a **non-sRGB** format (e.g. `Bgra8Unorm`): the
    /// shader outputs gamma-encoded video values, and an sRGB target would
    /// encode them a second time (washed-out picture). Backends whose surface
    /// only offers sRGB formats render through a non-sRGB view format.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("present.wgsl"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
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
            label: Some("present"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(PARAMS_SIZE as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
                texture_entry(3),
                texture_entry(4),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("present"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |vs: &str, fs: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    // Opaque (`None`): video replaces the cleared target, and
                    // the arrow is cut out with `discard`. Only the host's
                    // cursor image blends, premultiplied.
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let nv12 = pipeline("vs_video", "fs_nv12", None);
        let rgb = pipeline("vs_video", "fs_rgb", None);
        let cursor = pipeline("vs_cursor", "fs_cursor", None);
        let sprite_pipeline = pipeline(
            "vs_cursor",
            "fs_sprite",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("present"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("present params"),
            size: PARAMS_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let placeholder = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("present placeholder"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            device: device.clone(),
            queue: queue.clone(),
            layout,
            nv12,
            rgb,
            cursor,
            sprite_pipeline,
            sampler,
            uniforms,
            placeholder,
            upload: None,
            sprite: None,
        }
    }

    /// Install the host's cursor image (task D9), or `None` for the built-in
    /// arrow. Uploaded once here; every later [`draw`](Self::draw) with a
    /// cursor position draws it there.
    pub fn set_cursor_sprite(&mut self, sprite: Option<&CursorSprite>) -> Result<(), SurfaceError> {
        let Some(image) = sprite else {
            self.sprite = None;
            return Ok(());
        };
        if !image.is_valid() {
            // `write_texture` with a short buffer is a wgpu validation error (a
            // panic by default); refuse it as a reportable error instead.
            self.sprite = None;
            return Err(SurfaceError::Backend(format!(
                "cursor image {}x{} with {} bytes is malformed",
                image.width,
                image.height,
                image.bgra.len()
            )));
        }
        let size = (image.width, image.height);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cursor image"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Non-sRGB: the bytes are the host's display values, blended as-is
            // like the video (no transfer curve applied twice).
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_plane(
            &self.queue,
            &texture,
            &image.bgra,
            size.0 as usize * 4,
            size,
        );
        self.sprite = Some(Sprite {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            image: image.clone(),
        });
        Ok(())
    }

    /// Copy a CPU frame (`FrameData::Cpu`, NV12 or BGRA) into the renderer's
    /// own textures; draw it afterwards with [`uploaded`](Self::uploaded).
    pub fn upload(&mut self, frame: &Frame<'_>) -> Result<(), SurfaceError> {
        let FrameData::Cpu {
            format,
            plane0,
            plane1,
        } = frame.data
        else {
            return Err(SurfaceError::Backend(
                "this frame lives on the GPU of another backend and cannot be uploaded".into(),
            ));
        };
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 {
            return Err(SurfaceError::Backend(format!("empty frame {w}x{h}")));
        }
        let limit = self.device.limits().max_texture_dimension_2d;
        if w > limit || h > limit {
            return Err(SurfaceError::Backend(format!(
                "frame {w}x{h} is larger than this GPU can show ({limit} px per side)"
            )));
        }
        let chroma = (w.div_ceil(2), h.div_ceil(2));
        // Validate before touching wgpu: an undersized slice would be a wgpu
        // validation error (a panic by default) instead of a reportable failure.
        match format {
            PixelFormat::Bgra8 => check_plane("BGRA", plane0.data, plane0.stride, w, h, 4)?,
            PixelFormat::Nv12 => {
                check_plane("NV12 luma", plane0.data, plane0.stride, w, h, 1)?;
                let uv = plane1.ok_or_else(|| {
                    SurfaceError::Backend("NV12 frame without a chroma plane".into())
                })?;
                check_plane("NV12 chroma", uv.data, uv.stride, chroma.0, chroma.1, 2)?;
            }
        }
        let reuse = self
            .upload
            .as_ref()
            .is_some_and(|u| u.format == format && u.size == (w, h));
        if !reuse {
            self.upload = Some(self.create_upload(format, (w, h), chroma));
        }
        let Some(up) = self.upload.as_ref() else {
            return Err(SurfaceError::Backend("upload textures missing".into()));
        };
        write_plane(&self.queue, &up.plane0, plane0.data, plane0.stride, (w, h));
        if let (Some((tex, _)), Some(uv)) = (&up.plane1, plane1) {
            write_plane(&self.queue, tex, uv.data, uv.stride, chroma);
        }
        Ok(())
    }

    /// The last uploaded CPU frame as a drawable source.
    pub fn uploaded(&self) -> Option<Source<'_>> {
        let up = self.upload.as_ref()?;
        Some(match &up.plane1 {
            Some((_, chroma)) => Source::Nv12 {
                luma: &up.view0,
                chroma,
                full_range: false,
            },
            None => Source::Rgb(&up.view0),
        })
    }

    fn create_upload(&self, format: PixelFormat, size: (u32, u32), chroma: (u32, u32)) -> Upload {
        let make = |label, (w, h): (u32, u32), format| {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
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
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            (tex, view)
        };
        match format {
            PixelFormat::Bgra8 => {
                let (tex, view) = make("upload bgra", size, wgpu::TextureFormat::Bgra8Unorm);
                Upload {
                    format,
                    size,
                    plane0: tex,
                    view0: view,
                    plane1: None,
                }
            }
            PixelFormat::Nv12 => {
                let (tex, view) = make("upload luma", size, wgpu::TextureFormat::R8Unorm);
                Upload {
                    format,
                    size,
                    plane0: tex,
                    view0: view,
                    plane1: Some(make("upload chroma", chroma, wgpu::TextureFormat::Rg8Unorm)),
                }
            }
        }
    }

    /// Record clear → video → cursor into `encoder`, targeting `target`.
    ///
    /// Writes the uniform block through the queue, so call it at most once
    /// per `Queue::submit` (a second call would overwrite the first's
    /// geometry before it executes).
    pub fn draw(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        source: Source<'_>,
        params: DrawParams,
    ) {
        let video_rect = letterbox(params.target, params.video);
        // The host's image where there is one, else the arrow (older hosts,
        // or before the first image has arrived).
        let image = params.cursor.and_then(|c| {
            let s = self.sprite.as_ref()?;
            Some((sprite_rect(video_rect, &s.image, c)?, &s.view))
        });
        let cursor = match image {
            Some((rect, _)) => Some(rect),
            None => params
                .cursor
                .map(|c| cursor_rect(video_rect, c, params.scale)),
        };
        let cursor_view = image.map_or(&self.placeholder, |(_, view)| view);
        let full_range = matches!(
            source,
            Source::Nv12 {
                full_range: true,
                ..
            }
        );
        self.queue.write_buffer(
            &self.uniforms,
            0,
            &params_bytes(video_rect, cursor, params.target, full_range),
        );
        let (pipeline, plane0, plane1) = match source {
            Source::Nv12 { luma, chroma, .. } => (&self.nv12, luma, chroma),
            Source::Rgb(view) => (&self.rgb, view, &self.placeholder),
        };
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(plane0),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(plane1),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(cursor_view),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Black is the letterbox colour; clearing also defines
                    // every pixel of a fresh drawable.
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &bind_group, &[]);
        if video_rect.w > 0.0 && video_rect.h > 0.0 {
            pass.set_pipeline(pipeline);
            pass.draw(0..6, 0..1);
        }
        if cursor.is_some() {
            pass.set_pipeline(if image.is_some() {
                &self.sprite_pipeline
            } else {
                &self.cursor
            });
            pass.draw(0..6, 0..1);
        }
    }
}

/// Serialize the shader's `Params` block (little-endian, std140-compatible).
fn params_bytes(
    video: RectF,
    cursor: Option<RectF>,
    target: (u32, u32),
    full_range: bool,
) -> [u8; PARAMS_SIZE] {
    let cursor = cursor.unwrap_or(RectF {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    });
    let floats = [
        video.x as f32,
        video.y as f32,
        video.w as f32,
        video.h as f32,
        cursor.x as f32,
        cursor.y as f32,
        cursor.w as f32,
        cursor.h as f32,
        target.0.max(1) as f32,
        target.1.max(1) as f32,
    ];
    let mut out = [0u8; PARAMS_SIZE];
    for (chunk, v) in out.as_chunks_mut::<4>().0.iter_mut().zip(floats) {
        *chunk = v.to_le_bytes();
    }
    out[40..44].copy_from_slice(&u32::from(full_range).to_le_bytes());
    out
}

fn check_plane(
    what: &str,
    data: &[u8],
    stride: usize,
    w: u32,
    h: u32,
    bytes_per_pixel: usize,
) -> Result<(), SurfaceError> {
    let row = w as usize * bytes_per_pixel;
    let needed = stride
        .checked_mul(h as usize - 1)
        .and_then(|n| n.checked_add(row));
    match needed {
        Some(n) if stride >= row && data.len() >= n => Ok(()),
        _ => Err(SurfaceError::Backend(format!(
            "{what} plane is too small for {w}x{h}: {} bytes, stride {stride}",
            data.len()
        ))),
    }
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    stride: usize,
    (w, h): (u32, u32),
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            // `check_plane` proved stride ≤ data.len(), so it fits in u32 for
            // any frame within the texture-size limit; saturate defensively.
            bytes_per_row: Some(u32::try_from(stride).unwrap_or(u32::MAX)),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_layout_matches_the_shader_block() {
        let video = RectF {
            x: 1.0,
            y: 2.0,
            w: 3.0,
            h: 4.0,
        };
        let b = params_bytes(video, None, (640, 480), true);
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        assert_eq!((f(0), f(4), f(8), f(12)), (1.0, 2.0, 3.0, 4.0));
        assert_eq!((f(16), f(20), f(24), f(28)), (0.0, 0.0, 0.0, 0.0));
        assert_eq!((f(32), f(36)), (640.0, 480.0));
        assert_eq!(u32::from_le_bytes([b[40], b[41], b[42], b[43]]), 1);
        assert_eq!(&b[44..48], &[0, 0, 0, 0]);
    }

    #[test]
    fn plane_checks_reject_short_or_narrow_planes() {
        assert!(check_plane("t", &[0; 16], 4, 4, 4, 1).is_ok());
        // The last row needs only `row` bytes, not a full stride: 2·6 + 4.
        assert!(check_plane("t", &[0; 16], 6, 2, 3, 2).is_ok());
        assert!(check_plane("t", &[0; 15], 6, 2, 3, 2).is_err());
        assert!(check_plane("t", &[0; 15], 4, 4, 4, 1).is_err());
        assert!(check_plane("t", &[0; 64], 3, 4, 4, 1).is_err());
    }

    #[test]
    fn shader_parses_and_validates() {
        // Runs without a GPU, so a shader typo fails on every CI runner, not
        // only on those with an adapter.
        let module = naga::front::wgsl::parse_str(SHADER_WGSL)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(SHADER_WGSL)));
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        if let Err(e) = validator.validate(&module) {
            panic!("{}", e.emit_to_string(SHADER_WGSL));
        }
        let entries: Vec<&str> = module
            .entry_points
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        for entry in [
            "vs_video",
            "vs_cursor",
            "fs_nv12",
            "fs_rgb",
            "fs_cursor",
            "fs_sprite",
        ] {
            assert!(entries.contains(&entry), "missing entry point {entry}");
        }
    }
}
