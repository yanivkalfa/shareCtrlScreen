// NV12 (BT.709 limited range) → RGB, aspect-preserving letterbox with black
// bars, then a cursor sprite — the Presenter contract in
// native/crates/platform-api/src/surface.rs. Task L5 moves this file to
// platform-api/shaders/nv12.wgsl, shared with macOS (M4).
//
// References: ITU-R BT.709-6 §3 (Kr = 0.2126, Kb = 0.0722; 8-bit limited range
// Y 16..235, Cb/Cr 16..240); WGSL `textureSampleLevel` (allowed in non-uniform
// control flow, unlike `textureSample`): https://www.w3.org/TR/WGSL/

struct Params {
    // Video rectangle inside the target, in target pixels.
    video_origin: vec2<f32>,
    video_size: vec2<f32>,
    // Cursor centre in target pixels; flags.x > 0 = draw it, flags.y = radius.
    cursor: vec2<f32>,
    flags: vec2<f32>,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_uv: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

// One triangle covering the whole target.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let local = (pos.xy - p.video_origin) / p.video_size;
    var rgb = vec3<f32>(0.0, 0.0, 0.0);
    if all(local >= vec2<f32>(0.0)) && all(local < vec2<f32>(1.0)) {
        let y = (textureSampleLevel(tex_y, samp, local, 0.0).r - 16.0 / 255.0) * (255.0 / 219.0);
        let c = (textureSampleLevel(tex_uv, samp, local, 0.0).rg - 128.0 / 255.0) * (255.0 / 224.0);
        rgb = vec3<f32>(
            y + 1.5748 * c.y,
            y - 0.1873 * c.x - 0.4681 * c.y,
            y + 1.8556 * c.x,
        );
    }
    if p.flags.x > 0.0 {
        let d = distance(pos.xy, p.cursor);
        if d < p.flags.y {
            // White dot with a dark ring: visible on any picture.
            rgb = select(vec3<f32>(1.0), vec3<f32>(0.0), d > p.flags.y * 0.6);
        }
    }
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
