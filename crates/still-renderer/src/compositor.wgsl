@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct FullOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn fullscreen_vertex(@builtin(vertex_index) i: u32) -> FullOut {
    let p = vec2<f32>(select(-1.0, 3.0, i == 1u), select(-1.0, 3.0, i == 2u));
    var out: FullOut; out.position = vec4(p, 0.0, 1.0); out.uv = p * vec2(0.5, -0.5) + vec2(0.5); return out;
}

@fragment fn composite_fragment(in: FullOut) -> @location(0) vec4<f32> {
    let linear = textureSample(source, source_sampler, in.uv).rgb * 1.12;
    let radius = length((in.uv - vec2(0.5)) * vec2(1.0, 0.88));
    let vignette = 1.0 - smoothstep(0.34, 0.9, radius);
    let mapped = vec3(1.0) - exp(-linear);
    let neutral = mapped / (mapped + vec3(0.12));
    return vec4(neutral * mix(0.7, 1.0, vignette), 1.0);
}
