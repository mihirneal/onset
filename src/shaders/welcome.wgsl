// The welcome screen's animated background ("monochrome" style), white on
// black: a dot-matrix brain (an axial slice) whose dots flicker with slow,
// drifting activity, a hairline rule with a light gliding along it, and a
// breathing standby dot. One full-screen triangle, drawn only on the
// screens before t = 0. Pixel coordinates are physical pixels from the
// top-left corner.

struct Welcome {
    size: vec2<f32>,
    // Seconds, from frame flip times.
    time: f32,
    // Physical pixels per logical pixel.
    scale: f32,
    // Dot-matrix brain: center x, y, half height, strength (0 = none).
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

fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let k = f * f * (3.0 - 2.0 * f);
    let a = mix(hash12(i), hash12(i + vec2<f32>(1.0, 0.0)), k.x);
    let b = mix(hash12(i + vec2<f32>(0.0, 1.0)), hash12(i + vec2<f32>(1.0, 1.0)), k.x);
    return mix(a, b, k.y);
}

fn fbm(p: vec2<f32>) -> f32 {
    var v = 0.0;
    var a = 0.5;
    var x = p;
    for (var i = 0; i < 4; i++) {
        v += a * noise(x);
        x = x * 2.03 + vec2<f32>(17.1, 9.2);
        a *= 0.5;
    }
    return v;
}

fn sd_ellipse(p: vec2<f32>, r: vec2<f32>) -> f32 {
    // Approximate, but good near the edge, which is all we need.
    return (length(p / r) - 1.0) * min(r.x, r.y);
}

fn sd_capsule(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let pa = p - a;
    let ba = b - a;
    return length(pa - ba * clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0)) - r;
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}

// Signed distance to a brain seen from the side, facing left (negative
// inside), in units of the figure's half height, y down: cerebrum, temporal
// lobe, cerebellum and brainstem, with a bumpy cortical surface (gyri).
fn brain_sd(q: vec2<f32>) -> f32 {
    let p = q + vec2<f32>(0.0, -0.06);
    let cerebrum = sd_ellipse(p - vec2<f32>(-0.04, -0.20), vec2<f32>(1.0, 0.66));
    let temporal = sd_ellipse(p - vec2<f32>(-0.30, 0.16), vec2<f32>(0.52, 0.30));
    let cerebellum = sd_ellipse(p - vec2<f32>(0.52, 0.40), vec2<f32>(0.38, 0.25));
    let stem = sd_capsule(p, vec2<f32>(0.12, 0.22), vec2<f32>(0.20, 0.86), 0.11);
    var d = smin(cerebrum, temporal, 0.12);
    d = smin(d, cerebellum, 0.03);
    d = smin(d, stem, 0.05);
    return d + 0.045 * (noise(p * 7.0) - 0.5);
}

// 1 on the big grooves, drawn as gaps between dots: the lateral (Sylvian)
// fissure, the central sulcus, and the notch above the cerebellum.
fn grooves(q: vec2<f32>) -> f32 {
    let p = q + vec2<f32>(0.0, -0.06);
    let bend = vec2<f32>(0.0, 0.06 * sin(p.x * 3.0));
    let sylvian = sd_capsule(p + bend, vec2<f32>(-0.64, 0.04), vec2<f32>(0.22, -0.14), 0.0);
    let central = sd_capsule(p, vec2<f32>(0.04, -0.88), vec2<f32>(-0.16, -0.18), 0.0);
    let notch = sd_capsule(p, vec2<f32>(0.20, 0.24), vec2<f32>(0.92, 0.16), 0.0);
    return step(min(min(sylvian, central), notch), 0.03);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let px = frag.xy;
    let s = u.scale;
    let t = u.time;
    var v = 0.0;

    // The brain: one dot per grid cell, 42 rows tall. Dots inside glow
    // faintly, brighter along the cortex and in slowly drifting patches of
    // "activity", and fire short spikes, more often where it's busy.
    let bh = max(u.brain.z, 1.0);
    let spacing = bh / 21.0;
    let lp = px - u.brain.xy;
    let cell = floor(lp / spacing + 0.5);
    let n = cell * spacing / bh;
    let sd = brain_sd(n);
    let mask = step(sd, 0.0) * (1.0 - grooves(n));
    let field = fbm(n * 1.6 + vec2<f32>(t * 0.045, -t * 0.035));
    let busy = smoothstep(0.48, 0.72, field);
    let cortex = 1.0 - smoothstep(0.0, 0.10, -sd);
    let k = t * 0.7 + hash12(cell + 3.1);
    let fire = step(hash12(cell * 1.31 + floor(k) * 7.7), 0.04 + 0.30 * busy);
    let spike = fire * exp(-fract(k) / 0.7 * 5.0);
    let level = 0.20 + 0.20 * cortex + 0.45 * busy + 0.55 * spike;
    let radius = 1.45 + 0.5 * spike + 0.3 * busy;
    let d = length(lp - cell * spacing) / s;
    v += (1.0 - smoothstep(radius - 0.6, radius + 0.6, d)) * min(level, 1.0) * mask * u.brain.w;
    // A faint hairline around it.
    let edge = brain_sd(lp / bh) * bh / s - 0.5 * spacing / s;
    v += exp(-sq(edge / 0.7)) * 0.10 * u.brain.w;

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

    // Dither, so the faint dots don't band.
    v += (hash12(px) - 0.5) / 255.0;
    return vec4<f32>(v, v, v, 1.0);
}
