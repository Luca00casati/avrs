// Draws axis-aligned rectangles as instanced triangle strips.
// Each instance is a rect in window pixels (origin top-left) plus a colour.

struct Globals {
    screen: vec2<f32>,
}

@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    // x, y, width, height in pixels
    @location(0) rect: vec4<f32>,
    // linear RGBA, straight alpha
    @location(1) color: vec4<f32>,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32, inst: Instance) -> VertexOut {
    let corner = vec2<f32>(f32(index & 1u), f32(index >> 1u));
    let px = inst.rect.xy + corner * inst.rect.zw;
    let ndc = vec2<f32>(px.x / globals.screen.x * 2.0 - 1.0, 1.0 - px.y / globals.screen.y * 2.0);

    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
