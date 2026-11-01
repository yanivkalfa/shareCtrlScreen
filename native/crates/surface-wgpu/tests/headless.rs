//! Headless GPU tests of the shared presenter shader: render synthetic frames
//! into an offscreen texture, read it back, and compare known pixels. This is
//! the presenter's real coverage on machines without a display (CI runners)
//! and on OSes whose surface cannot run there (macOS code checked on Windows).
//!
//! Needs one wgpu adapter: DX12 (hardware or WARP) on Windows, Metal on
//! macOS, Vulkan on Linux (Mesa lavapipe on GPU-less runners). A missing
//! adapter fails the test with instructions rather than skipping silently.

use platform_api::{CursorSprite, Frame, Point};
use surface_wgpu::{DrawParams, Renderer};

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn gpu() -> (wgpu::Device, wgpu::Queue) {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .unwrap_or_else(|e| {
                panic!(
                "no wgpu adapter for the headless presenter tests ({e}). Windows: DX12 or WARP; \
                 Linux: install mesa-vulkan-drivers (lavapipe); macOS: Metal."
            )
            });
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .unwrap_or_else(|e| panic!("wgpu device creation failed: {e}"))
}

/// Render `frame` into a `target`-sized texture and return its RGBA pixels.
fn render(frame: &Frame<'_>, target: (u32, u32), cursor: Option<(f64, f64)>) -> Image {
    render_with(frame, target, cursor, None)
}

/// [`render`] with the host's cursor image installed (task D9).
fn render_with(
    frame: &Frame<'_>,
    target: (u32, u32),
    cursor: Option<(f64, f64)>,
    sprite: Option<&CursorSprite>,
) -> Image {
    let (device, queue) = gpu();
    let mut renderer = Renderer::new(&device, &queue, TARGET_FORMAT);
    renderer
        .upload(frame)
        .unwrap_or_else(|e| panic!("upload failed: {e}"));
    renderer
        .set_cursor_sprite(sprite)
        .unwrap_or_else(|e| panic!("cursor image refused: {e}"));
    let Some(source) = renderer.uploaded() else {
        panic!("upload produced no source");
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen target"),
        size: wgpu::Extent3d {
            width: target.0,
            height: target.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let row = target.0 * 4;
    let padded_row =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded_row * target.1),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    renderer.draw(
        &mut encoder,
        &view,
        source,
        DrawParams {
            target,
            video: (frame.width, frame.height),
            cursor,
            scale: 1.0,
        },
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: target.0,
            height: target.1,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    readback.map_async(wgpu::MapMode::Read, .., |r| {
        if let Err(e) = r {
            panic!("readback map failed: {e}");
        }
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap_or_else(|e| panic!("GPU wait failed: {e}"));
    let mapped = readback
        .get_mapped_range(..)
        .unwrap_or_else(|e| panic!("readback range: {e}"));
    let mut pixels = Vec::with_capacity((row * target.1) as usize);
    for y in 0..target.1 {
        let start = (y * padded_row) as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    Image {
        width: target.0,
        pixels,
    }
}

struct Image {
    width: u32,
    pixels: Vec<u8>,
}

impl Image {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    #[track_caller]
    fn assert_rgb(&self, x: u32, y: u32, want: [u8; 3]) {
        let got = self.at(x, y);
        let close = got[..3]
            .iter()
            .zip(want)
            .all(|(&g, w)| (i16::from(g) - i16::from(w)).abs() <= 3);
        assert!(
            close && got[3] == 255,
            "pixel ({x},{y}) = {got:?}, want ≈ {want:?}"
        );
    }
}

/// A solid NV12 frame (limited-range Y'CbCr).
struct Nv12 {
    width: u32,
    height: u32,
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl Nv12 {
    fn solid(width: u32, height: u32, (y, cb, cr): (u8, u8, u8)) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        Self {
            width,
            height,
            y: vec![y; (width * height) as usize],
            uv: [cb, cr].repeat((cw * ch) as usize),
        }
    }

    fn frame(&self) -> Frame<'_> {
        Frame::nv12(
            self.width,
            self.height,
            &self.y,
            self.width as usize,
            &self.uv,
            (self.width.div_ceil(2) * 2) as usize,
        )
    }
}

/// Name, limited-range (Y', Cb, Cr), expected (R, G, B).
type Case = (&'static str, (u8, u8, u8), [u8; 3]);

#[test]
fn nv12_bt709_limited_range_primaries_and_greys() {
    // Reference Y'CbCr values: BT.709 primaries quantized to 8-bit limited
    // range (Y' = 16 + 219·E'Y, C = 128 + 224·E'C), from ITU-R BT.709-6.
    let cases: [Case; 6] = [
        ("white", (235, 128, 128), [255, 255, 255]),
        ("black", (16, 128, 128), [0, 0, 0]),
        ("grey", (126, 128, 128), [128, 128, 128]),
        ("red", (63, 102, 240), [255, 0, 0]),
        ("green", (173, 42, 26), [0, 255, 0]),
        ("blue", (32, 240, 118), [0, 0, 255]),
    ];
    for (name, ycbcr, rgb) in cases {
        let src = Nv12::solid(8, 8, ycbcr);
        let img = render(&src.frame(), (8, 8), None);
        for (x, y) in [(0, 0), (4, 4), (7, 7)] {
            let got = img.at(x, y);
            let ok = got[..3]
                .iter()
                .zip(rgb)
                .all(|(&g, w)| (i16::from(g) - i16::from(w)).abs() <= 3);
            assert!(ok, "{name}: pixel ({x},{y}) = {got:?}, want ≈ {rgb:?}");
        }
    }
}

#[test]
fn letterbox_bars_are_black_and_the_picture_is_centred() {
    // 2:1 video in a square target: 32×16 picture at y ∈ [8, 24).
    let src = Nv12::solid(16, 8, (235, 128, 128));
    let img = render(&src.frame(), (32, 32), None);
    for x in [0, 15, 31] {
        img.assert_rgb(x, 2, [0, 0, 0]);
        img.assert_rgb(x, 7, [0, 0, 0]);
        img.assert_rgb(x, 8, [255, 255, 255]);
        img.assert_rgb(x, 16, [255, 255, 255]);
        img.assert_rgb(x, 23, [255, 255, 255]);
        img.assert_rgb(x, 24, [0, 0, 0]);
        img.assert_rgb(x, 31, [0, 0, 0]);
    }
}

#[test]
fn pillarbox_bars_for_a_wide_target() {
    let src = Nv12::solid(8, 8, (235, 128, 128));
    let img = render(&src.frame(), (32, 16), None);
    // 16×16 picture at x ∈ [8, 24).
    img.assert_rgb(3, 8, [0, 0, 0]);
    img.assert_rgb(8, 8, [255, 255, 255]);
    img.assert_rgb(23, 8, [255, 255, 255]);
    img.assert_rgb(28, 8, [0, 0, 0]);
}

#[test]
fn left_and_right_halves_keep_their_colours() {
    // Proves the chroma plane is sampled at half resolution and in the right
    // place: left half red, right half blue.
    let (w, h) = (16u32, 8u32);
    let mut src = Nv12::solid(w, h, (0, 0, 0));
    for row in 0..h {
        for x in 0..w {
            src.y[(row * w + x) as usize] = if x < w / 2 { 63 } else { 32 };
        }
    }
    let cw = w / 2;
    for row in 0..h / 2 {
        for cx in 0..cw {
            let (cb, cr) = if cx < cw / 2 { (102, 240) } else { (240, 118) };
            let i = ((row * cw + cx) * 2) as usize;
            src.uv[i] = cb;
            src.uv[i + 1] = cr;
        }
    }
    let img = render(&src.frame(), (w, h), None);
    img.assert_rgb(2, 4, [255, 0, 0]);
    img.assert_rgb(13, 4, [0, 0, 255]);
}

#[test]
fn odd_sized_nv12_frames_upload_and_render() {
    let src = Nv12::solid(7, 5, (235, 128, 128));
    let img = render(&src.frame(), (7, 5), None);
    img.assert_rgb(3, 2, [255, 255, 255]);
    img.assert_rgb(6, 4, [255, 255, 255]);
}

#[test]
fn bgra_frames_render_with_true_channel_order() {
    // B, G, R, A = pure blue.
    let (w, h) = (4u32, 4u32);
    let data = [255u8, 0, 0, 255].repeat((w * h) as usize);
    let frame = Frame::bgra(w, h, &data, (w * 4) as usize);
    let img = render(&frame, (w, h), None);
    img.assert_rgb(1, 1, [0, 0, 255]);
}

#[test]
fn cursor_sprite_is_drawn_with_its_tip_on_the_hotspot() {
    // Grey picture so both the white fill and the black outline stand out.
    let src = Nv12::solid(64, 64, (126, 128, 128));
    let img = render(&src.frame(), (64, 64), Some((0.5, 0.5)));
    // The 18×26 sprite spans x ∈ [32, 50), y ∈ [32, 58).
    img.assert_rgb(35, 47, [255, 255, 255]); // inside the arrow
    img.assert_rgb(32, 45, [0, 0, 0]); // left outline
    img.assert_rgb(46, 37, [128, 128, 128]); // right of the diagonal: video
    img.assert_rgb(31, 40, [128, 128, 128]); // left of the hotspot: video
    img.assert_rgb(20, 20, [128, 128, 128]); // far away: video
}

/// An 8×8 cursor image (premultiplied BGRA) for a 64×64 host frame: a 4×4
/// opaque red block at image pixels [2, 6), one half-transparent white pixel
/// at (6, 6), the rest transparent; hotspot (2, 2), the block's corner.
fn red_block_sprite() -> CursorSprite {
    let mut bgra = vec![0u8; 8 * 8 * 4];
    for y in 2..6 {
        for x in 2..6 {
            let i = (y * 8 + x) * 4;
            bgra[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let i = (6 * 8 + 6) * 4;
    bgra[i..i + 4].copy_from_slice(&[128, 128, 128, 128]);
    CursorSprite {
        width: 8,
        height: 8,
        hotspot: Point { x: 2, y: 2 },
        bgra,
        frame: (64, 64),
    }
}

#[test]
fn host_cursor_image_lands_hotspot_on_the_pointer_and_blends_premultiplied() {
    let src = Nv12::solid(64, 64, (126, 128, 128)); // grey ≈ 128
    let sprite = red_block_sprite();
    // 1:1. Pointer at host pixel (32, 32) = the hotspot, so the image's
    // top-left is (30, 30) and the red block covers [32, 36).
    let img = render_with(&src.frame(), (64, 64), Some((0.5, 0.5)), Some(&sprite));
    for (x, y) in [(32, 32), (35, 35), (33, 34)] {
        img.assert_rgb(x, y, [255, 0, 0]);
    }
    for (x, y) in [(31, 31), (30, 30), (36, 33), (37, 37), (20, 20)] {
        img.assert_rgb(x, y, [128, 128, 128]); // transparent texels: video
    }
    // Premultiplied over: 128 (image) + 128 · (1 − 128/255) (video) ≈ 192.
    img.assert_rgb(36, 36, [192, 192, 192]);

    // The same host frame shown twice as large: the image scales with the
    // video (16×16 on screen) and the hotspot still sits on the pointer.
    let big = render_with(&src.frame(), (128, 128), Some((0.5, 0.5)), Some(&sprite));
    for (x, y) in [(65, 65), (70, 70)] {
        big.assert_rgb(x, y, [255, 0, 0]);
    }
    for (x, y) in [(62, 62), (75, 66)] {
        big.assert_rgb(x, y, [128, 128, 128]);
    }

    // No image: the built-in arrow (white inside) is back.
    let arrow = render(&src.frame(), (64, 64), Some((0.5, 0.5)));
    arrow.assert_rgb(35, 47, [255, 255, 255]);
}

#[test]
fn malformed_cursor_images_are_refused() {
    let (device, queue) = gpu();
    let mut renderer = Renderer::new(&device, &queue, TARGET_FORMAT);
    let mut sprite = red_block_sprite();
    sprite.bgra.truncate(10);
    assert!(renderer.set_cursor_sprite(Some(&sprite)).is_err());
    assert!(renderer.set_cursor_sprite(None).is_ok());
}

#[test]
fn undersized_planes_are_rejected_not_rendered() {
    let (device, queue) = gpu();
    let mut renderer = Renderer::new(&device, &queue, TARGET_FORMAT);
    let y = vec![0u8; 10];
    let uv = vec![0u8; 8];
    let frame = Frame {
        width: 4,
        height: 4,
        data: platform_api::FrameData::Cpu {
            format: platform_api::PixelFormat::Nv12,
            plane0: platform_api::CpuPlane {
                data: &y,
                stride: 4,
            },
            plane1: Some(platform_api::CpuPlane {
                data: &uv,
                stride: 4,
            }),
        },
    };
    assert!(renderer.upload(&frame).is_err());
    assert!(renderer.uploaded().is_none());
}
