// Video presenter shader shared by the macOS (Metal) and Linux (Vulkan/GL)
// surfaces. Port of the D3D11 HLSL in render/src/lib.rs: NV12 BT.709
// limited-range -> RGB, the aspect-preserving letterbox, and the client-side
// cursor sprite drawn at the out-of-band pointer position.
//
// Geometry comes in target pixels (top-left origin) and is turned into clip
// space here, so a quad may hang past the target edge (the GPU clips) — the
// presenter never needs a viewport outside the attachment, which WebGPU forbids.

struct Params {
    // Video destination rectangle in target pixels: x, y, width, height.
    video_rect: vec4<f32>,
    // Cursor sprite rectangle in target pixels (top-left = hotspot).
    cursor_rect: vec4<f32>,
    // Render-target size in pixels.
    target_size: vec2<f32>,
    // 1 when the NV12 source is full range (0-255), 0 for limited (16-235/240).
    full_range: u32,
    _pad: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var samp: sampler;
// NV12: luma (R8). RGB sources: the colour texture.
@group(0) @binding(2) var plane0: texture_2d<f32>;
// NV12: interleaved Cb/Cr at half resolution (RG8). Unused for RGB sources.
@group(0) @binding(3) var plane1: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// Two triangles covering the unit square; uv (0,0) = top-left.
fn corner(i: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return c[i];
}

// Target pixels (y down) -> clip space (y up).
fn to_clip(p: vec2<f32>) -> vec4<f32> {
    let n = p / params.target_size * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0);
    return vec4<f32>(n, 0.0, 1.0);
}

fn quad(rect: vec4<f32>, i: u32) -> VsOut {
    let c = corner(i);
    var o: VsOut;
    o.pos = to_clip(rect.xy + c * rect.zw);
    o.uv = c;
    return o;
}

@vertex
fn vs_video(@builtin(vertex_index) i: u32) -> VsOut {
    return quad(params.video_rect, i);
}

// BT.709 Y'CbCr -> R'G'B'. Output stays gamma-encoded: the presenter renders
// into a non-sRGB target so the hardware never applies a second transfer curve.
fn bt709_to_rgb(y_in: f32, cbcr_in: vec2<f32>) -> vec3<f32> {
    var y = y_in;
    var c = cbcr_in - vec2<f32>(128.0 / 255.0);
    if params.full_range == 0u {
        // Limited range: luma 16-235, chroma 16-240 around 128.
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = c * (255.0 / 224.0);
    }
    let r = y + 1.5748 * c.y;
    let g = y - 0.1873 * c.x - 0.4681 * c.y;
    let b = y + 1.8556 * c.x;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_nv12(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(plane0, samp, in.uv).r;
    let cbcr = textureSample(plane1, samp, in.uv).rg;
    return vec4<f32>(bt709_to_rgb(y, cbcr), 1.0);
}

@fragment
fn fs_rgb(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(plane0, samp, in.uv).rgb, 1.0);
}

@vertex
fn vs_cursor(@builtin(vertex_index) i: u32) -> VsOut {
    return quad(params.cursor_rect, i);
}

// An arrow: the triangle with its tip at the hotspot (top-left), a vertical
// left edge and a diagonal right edge; white with a dark outline so it reads
// on any background.
@fragment
fn fs_cursor(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.uv;
    if p.x > p.y {
        discard;
    }
    let edge = p.x < 0.12 || p.y > 0.88 || (p.y - p.x) < 0.12;
    let v = select(1.0, 0.0, edge);
    return vec4<f32>(v, v, v, 1.0);
}
