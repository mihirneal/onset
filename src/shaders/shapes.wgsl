// Rectangles, circles and polar checkerboards, one instanced quad each.
// Positions are physical pixels with (0, 0) at the screen center and +y up.

struct Globals {
    size: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> g: Globals;

const KIND_RECT: u32 = 0u;
const KIND_CIRCLE: u32 = 1u;
const KIND_CHECKER: u32 = 2u;
const TAU: f32 = 6.283185307179586;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_size: vec2<f32>,
    @location(2) @interpolate(flat) color_a: vec4<f32>,
    @location(3) @interpolate(flat) color_b: vec4<f32>,
    @location(4) @interpolate(flat) params: vec4<f32>,
    @location(5) @interpolate(flat) kind: u32,
    @location(6) @interpolate(flat) phase: u32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) center: vec2<f32>,
    @location(1) half_size: vec2<f32>,
    @location(2) color_a: vec4<f32>,
    @location(3) color_b: vec4<f32>,
    @location(4) params: vec4<f32>,
    @location(5) kind: u32,
    @location(6) phase: u32,
) -> VsOut {
    // Triangle-strip corners (-1,-1) (1,-1) (-1,1) (1,1), padded 1px for antialiasing.
    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let local = corner * (half_size + vec2<f32>(1.0, 1.0));
    let p = center + local;
    var out: VsOut;
    out.pos = vec4<f32>(p.x / (g.size.x * 0.5), p.y / (g.size.y * 0.5), 0.0, 1.0);
    out.local = local;
    out.half_size = half_size;
    out.color_a = color_a;
    out.color_b = color_b;
    out.params = params;
    out.kind = kind;
    out.phase = phase;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var color = in.color_a;
    var coverage = 1.0;
    if in.kind == KIND_RECT {
        let d = in.half_size - abs(in.local);
        coverage = clamp(min(d.x, d.y) + 0.5, 0.0, 1.0);
    } else if in.kind == KIND_CIRCLE {
        let r = length(in.local);
        coverage = clamp(in.half_size.x - r + 0.5, 0.0, 1.0);
        // Optional border ring of width params.x, blended at its inner edge.
        if in.params.x > 0.0 {
            let t = clamp(r - (in.half_size.x - in.params.x) + 0.5, 0.0, 1.0);
            color = mix(in.color_a, in.color_b, t);
        }
    } else if in.kind == KIND_CHECKER {
        let inner = in.params.x;
        let outer = in.params.y;
        let rings = in.params.z;
        let wedges = in.params.w;
        let r = length(in.local);
        coverage = clamp(outer - r + 0.5, 0.0, 1.0) * clamp(r - inner + 0.5, 0.0, 1.0);
        // Log-spaced rings so checks scale with cortical magnification.
        var ring = 0.0;
        if inner > 0.0 {
            ring = floor(log(max(r, inner) / inner) / log(outer / inner) * rings);
        } else {
            ring = floor(r / outer * rings);
        }
        let wedge = floor((atan2(in.local.y, in.local.x) / TAU + 0.5) * wedges);
        let parity = (u32(ring + wedge) + in.phase) % 2u;
        if parity == 1u {
            color = in.color_b;
        }
    }
    return vec4<f32>(color.rgb, color.a * coverage);
}
