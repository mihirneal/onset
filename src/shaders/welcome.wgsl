// The welcome screen ("monochrome" style), white on black, drawn only on the
// screens before t = 0. Two passes share these uniforms: a full-screen
// triangle for the hairline rule and the standby dot (`vs_main`/`fs_main`),
// and a slowly turning point-cloud brain, one small quad per surface point
// from `brain.rs` (`vs_brain`/`fs_brain`). Pixel coordinates are physical
// pixels from the top-left corner.

struct Welcome {
    size: vec2<f32>,
    // Seconds, from frame flip times.
    time: f32,
    // Physical pixels per logical pixel.
    scale: f32,
    // Brain: center x, y, pixels per unit (about half its length), strength (0 = none).
    brain: vec4<f32>,
    // Hairline rule: x0, y, x1, on.
    rule: vec4<f32>,
    // Standby dot: x, y, radius in logical px, on.
    dot: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Welcome;

const TAU: f32 = 6.283185307179586;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // (-1,-1) (3,-1) (-1,3): one triangle that covers the screen.
    let p = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u)) * 2.0 - 1.0;
    return vec4<f32>(p, 0.0, 1.0);
}

fn sq(x: f32) -> f32 {
    return x * x;
}

fn hashu(n: u32) -> f32 {
    var x = n;
    x = (x ^ (x >> 16u)) * 0x7feb352du;
    x = (x ^ (x >> 15u)) * 0x846ca68bu;
    x = x ^ (x >> 16u);
    return f32(x) / 4294967295.0;
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let px = frag.xy;
    let s = u.scale;
    let t = u.time;
    var v = 0.0;

    // Hairline rule with a soft light gliding along it every 7 s.
    let along = step(u.rule.x, px.x) * step(px.x, u.rule.z);
    let on_rule = (1.0 - smoothstep(0.5, 1.0, abs(px.y - u.rule.y) / s)) * along * u.rule.w;
    let len = max(u.rule.z - u.rule.x, 1.0);
    let head = u.rule.x - 0.1 * len + fract(t / 7.0) * len * 1.3;
    v += on_rule * (0.20 + 0.70 * exp(-sq((px.x - head) / (70.0 * s))));

    // Standby dot: breathing, with a ring spreading out every 2 s.
    let dd = length(px - u.dot.xy) / s;
    let phase = fract(t / 2.0);
    let core = 1.0 - smoothstep(u.dot.z - 0.6, u.dot.z + 0.6, dd);
    let wave = exp(-sq((dd - u.dot.z - phase * 12.0) / 1.0)) * sq(1.0 - phase);
    v += (core * (0.8 + 0.2 * sin(t * TAU / 2.0)) + wave * 0.55) * u.dot.w;
    return vec4<f32>(v, v, v, 1.0);
}

struct BrainOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) level: f32,
    @location(2) @interpolate(flat) radius: f32,
};

// One surface point of the brain (`at`: position and a 0-1 seed; `normal`:
// the outward normal and the point's relief, 1 on a gyral crown, 0 in a
// sulcus). It turns slowly about the vertical axis, seen from a little
// above, in perspective. Crowns are bright and sulci dark, so the folds show;
// the outline glows where the surface turns away; the far side is faint; and
// the cortex flickers: random spikes, more of them inside a slow wave that
// drifts from front to back.
@vertex
fn vs_brain(@builtin(vertex_index) vi: u32, @location(0) at: vec4<f32>, @location(1) normal: vec4<f32>) -> BrainOut {
    let t = u.time;
    let yaw = 0.7 + t * 0.11;
    let cy = cos(yaw);
    let sy = sin(yaw);
    let cp = cos(0.3);
    let sp = sin(0.3);
    let turn = vec3<f32>(cy * at.x + sy * at.z, at.y, -sy * at.x + cy * at.z);
    let p = vec3<f32>(turn.x, cp * turn.y - sp * turn.z, sp * turn.y + cp * turn.z);
    let nt = vec3<f32>(cy * normal.x + sy * normal.z, normal.y, -sy * normal.x + cy * normal.z);
    let n = vec3<f32>(nt.x, cp * nt.y - sp * nt.z, sp * nt.y + cp * nt.z);
    let screen = u.brain.xy + vec2<f32>(p.x, -p.y) * u.brain.z * (4.0 / (4.0 - p.z));

    let rim = 1.0 - abs(n.z);
    let near = clamp(0.5 + 0.5 * p.z, 0.0, 1.0);
    let wave = smoothstep(0.7, 1.0, 0.5 + 0.5 * sin(at.z * 3.0 + at.y * 1.5 + t * 0.9));
    let k = t * 0.55 + at.w * 17.0;
    let fire = step(hashu(u32(at.w * 1.0e6) ^ (u32(floor(k)) * 0x9e3779b9u)), 0.04 + 0.2 * wave);
    let spike = fire * exp(-fract(k) * 6.0);
    let relief = normal.w * normal.w * normal.w;
    // Points on the far side, facing away, are faint, so it reads as a solid surface.
    let front = mix(0.1, 1.0, smoothstep(-0.25, 0.2, n.z));
    let level = (0.012 + 0.5 * relief + 0.24 * rim * rim * (0.3 + 0.7 * relief) + 0.08 * wave)
        * mix(0.35, 1.0, near) * front + 0.75 * spike * front;
    let radius = (0.7 + 0.35 * near + 0.8 * spike) * u.scale;

    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let at_px = screen + corner * (radius + 1.0);
    var out: BrainOut;
    out.pos = vec4<f32>(at_px.x / u.size.x * 2.0 - 1.0, 1.0 - at_px.y / u.size.y * 2.0, 0.0, 1.0);
    out.local = corner * (radius + 1.0);
    out.level = level * u.brain.w;
    out.radius = radius;
    return out;
}

// A soft round point, added to what's under it.
@fragment
fn fs_brain(in: BrainOut) -> @location(0) vec4<f32> {
    let v = in.level * (1.0 - smoothstep(in.radius - 0.7, in.radius + 0.7, length(in.local)));
    return vec4<f32>(v, v, v, 1.0);
}
