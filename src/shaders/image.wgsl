// One textured quad. Positions are physical pixels, center origin, +y up.

struct Globals {
    size: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) center: vec2<f32>,
    @location(1) half_size: vec2<f32>,
) -> VsOut {
    let c = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let p = center + (c * 2.0 - 1.0) * half_size;
    var out: VsOut;
    out.pos = vec4<f32>(p.x / (g.size.x * 0.5), p.y / (g.size.y * 0.5), 0.0, 1.0);
    out.uv = vec2<f32>(c.x, 1.0 - c.y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
