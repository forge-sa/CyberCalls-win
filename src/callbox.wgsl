// The Edgerunners CALL box: translucent scanlined panel, "+" grid, corner brackets and a
// two-arc ring, with horizontal slice glitches + chromatic split while popping in or out.
// Everything is computed in the box's own pixel space; output is premultiplied alpha.

struct Globals {
    screen: vec2<f32>,
    time: f32,
    _pad: f32,
};
@group(0) @binding(0) var<uniform> g: Globals;

struct Inst {
    @location(0) rect: vec4<f32>,   // x, y, size, opacity
    @location(1) fill: vec4<f32>,   // rgb, panel alpha
    @location(2) ring: vec4<f32>,   // rgb, ring rotation (radians)
    @location(3) accent: vec4<f32>, // rgb of corner brackets, glitch 0..1
    @location(4) misc: vec4<f32>,   // seed, scale, -, -
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) rect: vec4<f32>,
    @location(2) @interpolate(flat) fill: vec4<f32>,
    @location(3) @interpolate(flat) ring: vec4<f32>,
    @location(4) @interpolate(flat) accent: vec4<f32>,
    @location(5) @interpolate(flat) misc: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    let size = inst.rect.z;
    // room around the box for glitch slices that get shoved sideways
    let margin = size * 0.35;
    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let local = corner * (size + 2.0 * margin) - vec2<f32>(margin);
    let center = inst.rect.xy + vec2<f32>(size * 0.5);
    let p = center + (local - vec2<f32>(size * 0.5)) * inst.misc.y;

    var out: VOut;
    out.pos = vec4<f32>(p.x / g.screen.x * 2.0 - 1.0, 1.0 - p.y / g.screen.y * 2.0, 0.0, 1.0);
    out.local = local;
    out.rect = inst.rect;
    out.fill = inst.fill;
    out.ring = inst.ring;
    out.accent = inst.accent;
    out.misc = inst.misc;
    return out;
}

fn hash(n: f32) -> f32 {
    return fract(sin(n) * 43758.5453);
}

// premultiplied "src over dst"
fn over(dst: vec4<f32>, src: vec4<f32>) -> vec4<f32> {
    return src + dst * (1.0 - src.a);
}

const TAU: f32 = 6.2831853;

fn arc_mask(f: f32, a0: f32, a1: f32, circumference: f32) -> f32 {
    // distance in pixels along the circle to the arc's ends, for soft caps
    let d = min(f - a0, a1 - f) * circumference;
    return clamp(d + 0.5, 0.0, 1.0);
}

fn panel(q: vec2<f32>, v: VOut) -> vec4<f32> {
    let s = v.rect.z;
    let h = vec2<f32>(s * 0.5);
    let rel = q - h;
    // box SDF, 1px antialiased edge
    let dq = abs(rel) - h;
    let d = length(max(dq, vec2<f32>(0.0))) + min(max(dq.x, dq.y), 0.0);
    let inside = clamp(0.5 - d, 0.0, 1.0);
    if inside <= 0.0 {
        return vec4<f32>(0.0);
    }

    // panel with scanlines every 3px
    let scan = 0.72 + 0.28 * step(0.5, fract(q.y / 3.0));
    var c = vec4<f32>(v.fill.rgb, 1.0) * v.fill.a * scan * inside;

    // light inner border
    let border = inside * (1.0 - smoothstep(1.0, 2.2, -d));
    c = over(c, vec4<f32>(mix(v.fill.rgb, vec3<f32>(1.0), 0.45), 1.0) * 0.8 * border);

    // faint "+" marks on a 4x4 grid
    let cell = s * 0.25;
    let m = abs(fract(q / cell) - vec2<f32>(0.5)) * cell;
    let plus = select(0.0, 1.0, (m.x < 0.7 && m.y < 3.5) || (m.y < 0.7 && m.x < 3.5));
    c = over(c, vec4<f32>(1.0) * 0.28 * plus * inside);

    // corner brackets
    let e = min(q, vec2<f32>(s) - q);
    let inset = 2.5;
    let arm = s * 0.085;
    let thick = max(1.5, s * 0.016);
    let in_corner = e.x >= inset && e.y >= inset && e.x < inset + arm && e.y < inset + arm;
    let bracket = select(0.0, 1.0, in_corner && (e.x < inset + thick || e.y < inset + thick));
    c = over(c, vec4<f32>(v.accent.rgb, 1.0) * 0.95 * bracket);

    // two-arc ring
    let r = length(rel);
    let radius = s * 0.355;
    let width = s * 0.10;
    let band = clamp(width * 0.5 - abs(r - radius) + 0.5, 0.0, 1.0);
    if band > 0.0 {
        let ang = atan2(rel.y, rel.x) + v.ring.a;
        let f = fract(ang / TAU);
        let circ = TAU * r;
        let arcs = max(
            select(0.0, arc_mask(f, 0.03, 0.29, circ), f > 0.03 && f < 0.29),
            select(0.0, arc_mask(f, 0.47, 0.80, circ), f > 0.47 && f < 0.80),
        );
        c = over(c, vec4<f32>(v.ring.rgb, 1.0) * 0.95 * band * arcs);
    }
    return c;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4<f32> {
    let s = v.rect.z;
    let glitch = v.accent.a;
    let seed = v.misc.x;
    var q = v.local;

    var col: vec4<f32>;
    if glitch > 0.001 {
        // shove random horizontal slices sideways, re-rolled ~24 times a second
        let tick = floor(g.time * 24.0);
        let band = floor(q.y / (s * 0.07));
        let roll = hash(band * 12.9898 + tick * 78.233 + seed);
        var dx = 0.0;
        if roll < glitch * 0.55 {
            dx = (hash(band * 3.17 + tick * 1.71 + seed * 5.3) - 0.5) * s * 0.45 * glitch;
        }
        // red/cyan split
        let ca = glitch * s * 0.035;
        let cr = panel(q - vec2<f32>(dx + ca, 0.0), v);
        let cg = panel(q - vec2<f32>(dx, 0.0), v);
        let cb = panel(q - vec2<f32>(dx - ca, 0.0), v);
        col = vec4<f32>(cr.r, cg.g, cb.b, max(cg.a, max(cr.a, cb.a)));
    } else {
        col = panel(q, v);
    }
    return col * v.rect.w;
}
