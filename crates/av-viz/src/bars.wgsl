// Draws rounded rectangles with a vertical gradient and an optional soft glow,
// one instance per shape. Each instance's quad is grown by the glow radius and
// the fragment shader shades it from a signed distance to the rounded box.
// Positions are window pixels (origin top-left); colours are linear RGBA,
// output premultiplied.

struct Globals {
    screen: vec2<f32>,
}

@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    // x, y, width, height in pixels
    @location(0) rect: vec4<f32>,
    // corner radius, glow radius (0 = none), unused, unused
    @location(1) params: vec4<f32>,
    // colour at the top and bottom edge
    @location(2) top: vec4<f32>,
    @location(3) bottom: vec4<f32>,
    // glow colour; alpha is the glow strength at the edge
    @location(4) glow: vec4<f32>,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) rect: vec4<f32>,
    @location(1) params: vec4<f32>,
    @location(2) top: vec4<f32>,
    @location(3) bottom: vec4<f32>,
    @location(4) glow: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32, inst: Instance) -> VertexOut {
    let corner = vec2<f32>(f32(index & 1u), f32(index >> 1u));
    // The glow radius is 2 sigma; 3 sigma of room lets it fade out fully.
    let margin = inst.params.y * 1.5 + 1.0;
    let origin = inst.rect.xy - vec2<f32>(margin);
    let size = inst.rect.zw + vec2<f32>(2.0 * margin);
    let px = origin + corner * size;
    let ndc = vec2<f32>(px.x / globals.screen.x * 2.0 - 1.0, 1.0 - px.y / globals.screen.y * 2.0);

    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.rect = inst.rect;
    out.params = inst.params;
    out.top = inst.top;
    out.bottom = inst.bottom;
    out.glow = inst.glow;
    return out;
}

// erf(x), Abramowitz & Stegun 7.1.26 (max error 1.5e-7).
fn erf(x: f32) -> f32 {
    let s = sign(x);
    let a = abs(x);
    let t = 1.0 / (1.0 + 0.3275911 * a);
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * exp(-a * a);
    return s * y;
}

// Signed distance from p to a box of half size b with corner radius r.
fn rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let p = in.position.xy;
    let half = in.rect.zw * 0.5;
    let center = in.rect.xy + half;
    let radius = min(in.params.x, min(half.x, half.y));
    let d = rounded_box(p - center, half, radius);

    let t = clamp((p.y - in.rect.y) / max(in.rect.w, 1.0), 0.0, 1.0);
    let fill = mix(in.top, in.bottom, t);
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    let fill_a = fill.a * coverage;

    // Glow: the shape blurred by a Gaussian (like a CSS/canvas shadow), which
    // across an edge falls off as 0.5 * erfc(d / (sigma * sqrt 2)).
    var glow_a = 0.0;
    let sigma = in.params.y * 0.5;
    if (sigma > 0.0) {
        let blurred = 0.5 * (1.0 - erf(d / (sigma * 1.41421356)));
        glow_a = in.glow.a * blurred * (1.0 - fill_a);
    }

    let rgb = fill.rgb * fill_a + in.glow.rgb * glow_a;
    return vec4<f32>(rgb, fill_a + glow_a);
}
