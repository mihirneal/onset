// Everything the participant sees, drawn by one full-screen shader:
// gray background, red fixation dot, radial checkerboard and the
// photodiode square in the top-left corner.

struct Params {
    // Screen size in pixels.
    size: vec2<f32>,
    // 1 = draw the checkerboard, 0 = plain gray.
    checker: u32,
    // Flips at every contrast reversal.
    phase: u32,
    // Photodiode square brightness: 0 black, 1 white.
    diode: f32,
    // Photodiode square side, in pixels.
    diode_px: f32,
    // Background gray level.
    gray: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> p: Params;

const PI: f32 = 3.14159265;
const WEDGES: f32 = 24.0;
const RINGS_PER_OCTAVE: f32 = 4.0;

// One triangle big enough to cover the whole screen; no vertex buffer needed.
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(i & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

// `pos.xy` is the pixel position, with (0, 0) at the top-left corner.
@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    if (pos.x < p.diode_px && pos.y < p.diode_px) {
        return vec4<f32>(vec3<f32>(p.diode), 1.0);
    }

    let d = pos.xy - p.size * 0.5;
    let r = length(d);
    let radius = min(p.size.x, p.size.y) * 0.5;

    if (r < radius * 0.012) {
        return vec4<f32>(1.0, 0.0, 0.0, 1.0);
    }

    var level = p.gray;
    // Radial checkerboard whose rings widen away from the center. A small
    // gray disk around fixation keeps the innermost rings from getting finer
    // than a pixel.
    if (p.checker == 1u && r > radius * 0.04) {
        let ring = i32(floor(log2(r / radius) * RINGS_PER_OCTAVE));
        let wedge = i32(floor((atan2(d.y, d.x) + PI) / (2.0 * PI) * WEDGES));
        level = f32((ring + wedge + i32(p.phase)) & 1);
    }
    return vec4<f32>(vec3<f32>(level), 1.0);
}
