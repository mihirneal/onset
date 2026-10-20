// The welcome screen's animated background ("grid" style): a dark horizon
// with a slowly approaching perspective grid, a soft scan beam, drifting
// particles, a glass panel behind the instructions and a glow line under the
// title. One full-screen triangle, drawn only on the screens before t = 0.
// Pixel coordinates here are physical pixels from the top-left corner.

struct Welcome {
    size: vec2<f32>,
    // Seconds, from frame flip times.
    time: f32,
    // Physical pixels per logical pixel.
    scale: f32,
    accent: vec4<f32>,
    // Panel box x0, y0, x1, y1; empty (x1 <= x0) when there's no text.
    panel: vec4<f32>,
    // Title box x0, y0, x1, y1; the glow line runs under it.
    title: vec4<f32>,
    // Standby beacon x, y, unused, on (1 on the waiting screen).
    beacon: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Welcome;

const TAU: f32 = 6.283185307179586;
// Horizon height, as a fraction of the screen from the top.
const HORIZON: f32 = 0.64;

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

// Antialiased grid lines at the integers of `v`, about `width` px wide, with
// a soft glow. `fw` is fwidth(v); lines fade out where they get too dense.
fn grid_line(v: f32, fw: f32, width: f32, glow: f32) -> f32 {
    let dist = abs(fract(v + 0.5) - 0.5) / max(fw, 1e-6);
    let core = clamp(width * 0.5 + 0.5 - dist, 0.0, 1.0);
    let halo = exp(-dist * dist / (2.0 * glow * glow)) * 0.35;
    return (core + halo) * (1.0 - smoothstep(0.06, 0.3, fw));
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

    // Sky: near-black at the top, a deep accent-tinted blue at the horizon,
    // with a faint violet haze drifting across.
    let up = clamp(uv.y / HORIZON, 0.0, 1.0);
    var col = mix(vec3<f32>(0.008, 0.010, 0.026), vec3<f32>(0.030, 0.040, 0.095) + accent * 0.05, up * up);
    let haze = (0.5 + 0.5 * sin(q.x * 2.3 + t * 0.05)) * (0.5 + 0.5 * sin(q.y * 3.1 - q.x * 1.2 - t * 0.04));
    col += vec3<f32>(0.045, 0.014, 0.080) * haze * (1.0 - up * 0.5);

    // Floor below the horizon: depth z = 1/d, so lines converge on the
    // horizon's center; the grid moves slowly toward the viewer.
    let d = uv.y - HORIZON;
    let z = 1.0 / max(d, 1e-3);
    let gx = q.x * z * 3.0;
    let gz = z * 3.0 + t * 0.35;
    // Derivatives stay in uniform control flow (before any branch).
    let fwx = fwidth(gx);
    let fwz = fwidth(gz);
    let floor_mask = smoothstep(0.0, 0.004, d);
    let fog = clamp(exp(-(z - 2.8) * 0.11), 0.0, 1.0);
    let grid = max(grid_line(gx, fwx, 1.3 * s, 2.5 * s), grid_line(gz, fwz, 1.3 * s, 2.5 * s));
    col = mix(col, vec3<f32>(0.004, 0.006, 0.016), floor_mask * 0.6);
    col += accent * grid * fog * floor_mask * 0.42 * exp(-q.x * q.x * 0.9);

    // Horizon: a broad glow and a thin bright line, fading to the sides.
    let side = exp(-q.x * q.x * 2.2);
    col += (accent * 0.55 + vec3<f32>(0.10, 0.04, 0.16)) * exp(-abs(d) * 14.0) * 0.32 * side;
    col += vec3<f32>(0.16, 0.05, 0.30) * exp(-abs(d) * 8.0) * 0.22 * (1.0 - 0.7 * side);
    col += accent * exp(-sq(d * h / (1.1 * s))) * 0.75 * side;

    // Drifting particles: three layers of one dot per cell, rising slowly.
    var dots = 0.0;
    for (var i = 0; i < 3; i++) {
        let layer = f32(i);
        let cell = 0.075 + 0.04 * layer;
        let p = (q + vec2<f32>(0.0, t * (0.006 + 0.005 * layer))) / cell;
        let id = floor(p) + vec2<f32>(layer * 37.0, layer * 11.0);
        let r = hash12(id);
        let off = (hash22(id) - 0.5) * 0.55 + vec2<f32>(sin(t * 0.45 + r * TAU) * 0.08, 0.0);
        let dist = length(fract(p) - 0.5 - off) * cell * h;
        let rad = (0.7 + 1.3 * r) * s * (1.0 + 0.35 * layer);
        let twinkle = 0.55 + 0.45 * sin(t * (0.7 + 1.3 * r) + r * 40.0);
        let present = step(0.5, hash12(id + 5.17));
        dots += (exp(-dist * dist / (rad * rad)) + 0.12 * exp(-dist * dist / (6.0 * rad * rad)))
            * twinkle * present * (0.30 + 0.12 * layer);
    }
    col += mix(accent, vec3<f32>(1.0, 1.0, 1.0), 0.5) * dots;

    // A soft scan beam sweeps down every 9 s, plus faint fixed scan lines.
    let beam = fract(t / 9.0) * 1.3 - 0.15;
    col += accent * exp(-sq((uv.y - beam) / 0.035)) * 0.035;
    col *= 0.95 + 0.05 * cos(px.y * TAU / (3.0 * s));

    // Vignette.
    let v = uv * (1.0 - uv);
    col *= mix(0.45, 1.0, pow(clamp(v.x * v.y * 16.0, 1e-6, 1.0), 0.25));

    // Glass panel behind the text: darkens what's behind it so the text
    // stays readable, with a faint edge and brighter HUD corner brackets.
    let has_panel = step(u.panel.x + 1.0, u.panel.z);
    let pc = 0.5 * (u.panel.xy + u.panel.zw);
    let ph = 0.5 * (u.panel.zw - u.panel.xy);
    let lp = px - pc;
    let sd = round_box(lp, ph, 14.0 * s);
    let inside = 1.0 - smoothstep(-0.75, 0.75, sd);
    col *= 1.0 - 0.45 * exp(-max(sd, 0.0) / (28.0 * s)) * has_panel;
    col = mix(col, vec3<f32>(0.010, 0.014, 0.030) + accent * 0.012, inside * 0.9 * has_panel);
    let edge = exp(-sq(sd / (0.75 * s)));
    let bracket_len = 26.0 * s;
    let corner = step(ph.x - bracket_len, abs(lp.x)) * step(ph.y - bracket_len, abs(lp.y));
    col += accent * (edge * (0.16 + 0.7 * corner) + exp(-abs(sd) / (9.0 * s)) * 0.035) * has_panel;

    // Glow line under the title, with a highlight gliding along it.
    let tc = 0.5 * (u.title.x + u.title.z);
    let line_y = u.title.w + 0.022 * h;
    let reach = 0.5 * (u.title.z - u.title.x) + 0.09 * u.size.x;
    let has_title = step(u.title.x + 1.0, u.title.z);
    let taper = pow(clamp(1.0 - abs(px.x - tc) / max(reach, 1.0), 1e-6, 1.0), 1.5);
    let glide = tc + sin(t * 0.35) * reach * 0.7;
    let shine = 1.0 + 1.6 * exp(-sq((px.x - glide) / (90.0 * s)));
    let dy = px.y - line_y;
    col += accent * taper * shine * has_title
        * (exp(-sq(dy / (0.9 * s))) * 0.9 + exp(-sq(dy / (12.0 * s))) * 0.12);

    // Standby beacon while waiting for the scanner: a dot with rings
    // spreading out from it every 2.4 s.
    let br = length(px - u.beacon.xy) / s;
    var rings = 0.0;
    for (var k = 0; k < 3; k++) {
        let ph_k = fract(t / 2.4 + f32(k) / 3.0);
        rings += exp(-sq((br - ph_k * 80.0) / 1.5)) * (1.0 - ph_k) * (1.0 - ph_k);
    }
    let beat = 0.7 + 0.3 * sin(t * TAU / 2.4);
    col += accent * (exp(-br * br / 18.0) * beat + rings * 0.9) * u.beacon.w;

    // Dither so the gradients don't band on 8-bit output.
    col += (hash12(px) - 0.5) / 255.0;
    return vec4<f32>(col, 1.0);
}
