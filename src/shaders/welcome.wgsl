// The welcome screen's animated background ("halo" style): slowly flowing
// light on near-black, a thin halo ring with a gliding highlight and fine
// tick marks, drifting dust, a soft glow behind the name and a frosted-glass
// card behind the text. One full-screen triangle, drawn only on the screens
// before t = 0. Pixel coordinates are physical pixels from the top-left.

struct Welcome {
    size: vec2<f32>,
    // Seconds, from frame flip times.
    time: f32,
    // Physical pixels per logical pixel.
    scale: f32,
    accent: vec4<f32>,
    // Boxes are x0, y0, x1, y1; an empty box (x1 <= x0) draws nothing.
    // The glass card behind the instructions or the waiting text.
    card: vec4<f32>,
    // The name (or title); a soft glow sits behind it.
    hero: vec4<f32>,
    // Halo ring: center x, y, radius, strength (0 = none).
    ring: vec4<f32>,
    // Standby dot: x, y, radius in logical px, on.
    dot: vec4<f32>,
    // x: the card's corner radius.
    misc: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Welcome;

const TAU: f32 = 6.283185307179586;
const VIOLET: vec3<f32> = vec3<f32>(0.42, 0.30, 1.0);

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

fn hash22(p: vec2<f32>) -> vec2<f32> {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * vec3<f32>(0.1031, 0.1030, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
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

fn round_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let d = abs(p) - half + vec2<f32>(r, r);
    return length(max(d, vec2<f32>(0.0, 0.0))) + min(max(d.x, d.y), 0.0) - r;
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let px = frag.xy;
    let h = u.size.y;
    let s = u.scale;
    let t = u.time;
    let accent = u.accent.rgb;
    let uv = px / u.size;
    // Centered, in screen heights.
    let q = (px - 0.5 * u.size) / h;

    // Near-black navy, a little lighter at the top.
    var col = mix(vec3<f32>(0.020, 0.025, 0.048), vec3<f32>(0.008, 0.010, 0.022), uv.y);

    // Flowing light: two soft fields of color, top left and bottom right,
    // drifting slowly and shaped by domain-warped noise.
    let warp = vec2<f32>(fbm(q * 1.1 + vec2<f32>(0.0, t * 0.020)), fbm(q * 1.1 + vec2<f32>(4.7, -t * 0.017)));
    let flow = fbm(q * 1.6 + warp * 1.7 + vec2<f32>(t * 0.012, -t * 0.008));
    let ca = vec2<f32>(-0.62 + 0.10 * sin(t * 0.031), -0.32 + 0.06 * cos(t * 0.027));
    let cb = vec2<f32>(0.68 + 0.08 * cos(t * 0.023), 0.36 + 0.07 * sin(t * 0.029));
    let da = (q - ca) * vec2<f32>(0.9, 1.4);
    let db = (q - cb) * vec2<f32>(0.9, 1.3);
    let fa = exp(-dot(da, da) * 2.6);
    let fb = exp(-dot(db, db) * 2.4);
    col += accent * fa * (0.06 + 0.38 * flow * flow);
    col += VIOLET * fb * (0.05 + 0.28 * flow * flow);
    col += mix(accent, VIOLET, uv.x) * 0.035 * smoothstep(0.45, 0.85, flow);

    // A soft glow behind the name.
    let has_hero = step(u.hero.x + 1.0, u.hero.z);
    let hc = 0.5 * (u.hero.xy + u.hero.zw);
    let hh = max(0.5 * (u.hero.zw - u.hero.xy), vec2<f32>(1.0, 1.0));
    let he = (px - hc) / (hh * vec2<f32>(1.25, 2.6));
    col += mix(accent, VIOLET, 0.3) * exp(-dot(he, he) * 1.4) * 0.07 * has_hero;

    // Dust: sparse soft points in two depths, drifting up and to the right.
    var dust = 0.0;
    for (var i = 0; i < 2; i++) {
        let layer = f32(i);
        let cell = 0.09 + 0.06 * layer;
        let p = (q + vec2<f32>(-t * 0.003, t * (0.005 + 0.004 * layer))) / cell;
        let id = floor(p) + vec2<f32>(layer * 37.0, layer * 11.0);
        let r = hash12(id);
        let off = (hash22(id) - 0.5) * 0.6;
        let dist = length(fract(p) - 0.5 - off) * cell * h;
        let rad = (0.6 + 1.1 * r) * s * (1.0 + 0.6 * layer);
        let twinkle = 0.6 + 0.4 * sin(t * (0.5 + r) + r * 40.0);
        let present = step(0.62, hash12(id + 5.17));
        dust += exp(-dist * dist / (rad * rad)) * twinkle * present * (0.22 + 0.10 * layer);
    }
    col += mix(accent, vec3<f32>(1.0, 1.0, 1.0), 0.6) * dust;

    // Halo ring: a thin line with a highlight gliding around it, a soft
    // glow, and a ring of fine ticks just outside, every 15th one longer.
    let rv = px - u.ring.xy;
    let r = length(rv);
    let radius = u.ring.z;
    let ang = atan2(rv.y, rv.x);
    let dr = (r - radius) / s;
    // How far behind each highlight's head this point is (0-1 of a turn);
    // the heads fade in over a few degrees so there's no hard edge.
    let behind = fract((t * 0.42 - ang) / TAU);
    let second = fract(behind + 0.5);
    let comet = exp(-behind * 7.0) * smoothstep(0.0, 0.03, behind)
        + 0.45 * exp(-second * 11.0) * smoothstep(0.0, 0.03, second);
    let line = exp(-dr * dr / 1.1);
    let glow = exp(-abs(dr) / 26.0);
    col += mix(accent, vec3<f32>(1.0, 1.0, 1.0), 0.35 * comet)
        * (line * (0.13 + 0.85 * comet) + glow * (0.025 + 0.06 * comet)) * u.ring.w;
    let tick_r = radius + 26.0 * s;
    let ta = (ang - t * 0.012) / TAU;
    let arc = abs(fract(ta * 180.0 + 0.5) - 0.5) / 180.0 * TAU * r / s;
    let major = 1.0 - step(0.5 / 15.0, abs(fract(ta * 12.0 + 0.5) - 0.5));
    let tick_len = mix(4.0, 10.0, major);
    let along = abs(r - tick_r) / s;
    let tick = (1.0 - smoothstep(0.35, 0.95, arc))
        * (1.0 - smoothstep(tick_len * 0.5 - 0.5, tick_len * 0.5 + 0.5, along));
    col += vec3<f32>(0.70, 0.82, 1.0) * tick * mix(0.07, 0.22, major) * u.ring.w;

    // Vignette.
    let v = uv * (1.0 - uv);
    col *= mix(0.5, 1.0, pow(clamp(v.x * v.y * 16.0, 1e-6, 1.0), 0.3));

    // Frosted-glass card: darkens and softens what's behind it, a faint
    // edge that's brightest along the top, and a soft shadow.
    let has_card = step(u.card.x + 1.0, u.card.z);
    let cc = 0.5 * (u.card.xy + u.card.zw);
    let ch = 0.5 * (u.card.zw - u.card.xy);
    let sd = round_box(px - cc, ch, u.misc.x);
    let inside = (1.0 - smoothstep(-0.75, 0.75, sd)) * has_card;
    col *= 1.0 - 0.45 * exp(-max(sd, 0.0) / (34.0 * s)) * has_card;
    col = mix(col, col * 0.3 + vec3<f32>(0.022, 0.028, 0.052), inside);
    let down = clamp((px.y - u.card.y) / max(u.card.w - u.card.y, 1.0), 0.0, 1.0);
    let edge = exp(-sq(sd / (0.6 * s)));
    col += vec3<f32>(0.80, 0.88, 1.0) * edge * mix(0.24, 0.05, down) * has_card;
    col += vec3<f32>(0.60, 0.75, 1.0) * inside * exp(-max(px.y - u.card.y, 0.0) / (36.0 * s)) * 0.03;

    // Standby dot: solid, breathing, with a ring spreading out every 2 s.
    let dd = length(px - u.dot.xy) / s;
    let phase = fract(t / 2.0);
    let core = 1.0 - smoothstep(u.dot.z - 0.75, u.dot.z + 0.75, dd);
    let wave = exp(-sq((dd - u.dot.z - phase * 16.0) / 1.2)) * sq(1.0 - phase);
    let breathe = 0.8 + 0.2 * sin(t * TAU / 2.0);
    col = mix(col, accent * breathe, core * u.dot.w);
    col += accent * (wave * 0.8 + exp(-dd * dd / max(sq(u.dot.z * 2.6), 1.0)) * 0.3) * u.dot.w;

    // Fine static grain, which also keeps the gradients from banding.
    col += (hash12(px) - 0.5) * (2.0 / 255.0);
    return vec4<f32>(col, 1.0);
}
