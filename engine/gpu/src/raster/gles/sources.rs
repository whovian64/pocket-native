//! the GLSL the renderer builds its programs from: the fixed sources
//! for transfers, copies and presenting, and the vertex and fragment
//! sources generated from a draw's state.

use super::*;

pub(super) const FRAGMENT_SOURCE: &str = include_str!("../../../shaders/raster.frag");

/// the alpha test's compare as raster.frag has it, and as Adreno's compiler
/// takes it: it fails to link the switch where it folds to a constant
/// function with a discard after it. Only Adreno gets the second, so other
/// GPUs keep the programs they have cached.
pub(super) const SWITCH_COMPARE: &str = "    switch (function) {
        case 0u: return false;
        case 1u: return true;
        case 2u: return value == reference;
        case 3u: return value != reference;
        case 4u: return value < reference;
        case 5u: return value <= reference;
        case 6u: return value > reference;
        default: return value >= reference;
    }";
pub(super) const ADRENO_COMPARE: &str = "    if (function < 2u) {
        return function == 1u;
    }
    if (function < 4u) {
        return (value == reference) == (function == 2u);
    }
    if (function < 6u) {
        return value < reference || (function == 5u && value == reference);
    }
    return value > reference || (function == 7u && value == reference);";
pub(super) static ADRENO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) const VERTEX_SOURCE: &str = r#"#version 310 es
layout(location = 0) in vec4 position;
layout(location = 1) in vec4 color;
layout(location = 2) in vec4 texcoords01;
layout(location = 3) in vec4 texcoord2_depth;
layout(location = 4) in vec4 quaternion;
layout(location = 5) in vec4 view;

layout(location = 0) out vec4 out_color;
layout(location = 1) out vec4 out_texcoords01;
layout(location = 2) out vec2 out_texcoord2;
layout(location = 3) out float out_depth_w;
layout(location = 4) out vec4 out_quaternion;
layout(location = 5) out vec4 out_view;
layout(location = 6) out float out_w;

void main() {
    float depth = clamp(texcoord2_depth.z, 0.0, 1.0);
    gl_Position = vec4(position.xy, (depth * 2.0 - 1.0) * position.w, position.w);
    out_color = color;
    out_texcoords01 = texcoords01;
    out_texcoord2 = texcoord2_depth.xy;
    out_depth_w = texcoord2_depth.z * position.w;
    out_quaternion = quaternion;
    out_view = view;
    out_w = position.w;
}
"#;

pub(super) const TRANSFER_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D target;
uniform ivec2 size;
uniform int input_height;
uniform int output_height;
uniform ivec2 scale;
uniform int flip;
uniform int input_format;
uniform int output_format;
uniform int source_row;
uniform int source_column;
uniform int target_row;
uniform int source_height;

uint expand(uint value, uint bits) {
    return (value << (8u - bits)) | (value >> (2u * bits - 8u));
}

uvec4 quantize(uvec4 c, int format) {
    switch (format) {
    case 0:
        return c;
    case 1:
        return uvec4(c.rgb, 255u);
    case 2:
        return uvec4(expand(c.r >> 3, 5u), expand(c.g >> 2, 6u), expand(c.b >> 3, 5u), 255u);
    case 3:
        return uvec4(expand(c.r >> 3, 5u), expand(c.g >> 3, 5u), expand(c.b >> 3, 5u), c.a >= 128u ? 255u : 0u);
    default:
        return (c >> 4) * 17u;
    }
}

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= size.x || at.y >= size.y) {
        return;
    }
    uvec4 sum = uvec4(0u);
    for (int dy = 0; dy < scale.y; dy++) {
        for (int dx = 0; dx < scale.x; dx++) {
            int x = at.x * scale.x + dx;
            int y = at.y * scale.y + dy;
            if (flip != 0) {
                y = input_height - 1 - y;
            }
            int row = source_height - 1 - (source_row + y);
            vec4 texel = imageLoad(source, ivec2(source_column + x, row));
            sum += quantize(uvec4(round(texel * 255.0)), input_format);
        }
    }
    uint count = uint(scale.x * scale.y);
    uvec4 pixel = quantize((sum + count / 2u) / count, output_format);
    imageStore(target, ivec2(at.x, output_height - 1 - (target_row + at.y)), vec4(pixel) / 255.0);
}
"#;

/// a surface's pixels from another's drawn over the same memory in another
/// format or layout, through the bytes memory would hold, as a write back
/// and an upload would make them.
pub(super) const REINTERPRET_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D target;
// width, height, bytes a pixel and format of each, whether each is tiled
uniform ivec4 target_shape;
uniform ivec4 source_shape;
uniform ivec2 tiled;
// the target's first row and how many
uniform ivec2 rows;
// how far into the source's memory the target starts
uniform int delta;

int place(int x, int y, int width, int bpp, int is_tiled) {
    if (is_tiled == 0) {
        return (y * width + x) * bpp;
    }
    int m = (x & 1) | ((y & 1) << 1) | ((x & 2) << 1) | ((y & 2) << 2) | ((x & 4) << 2) | ((y & 4) << 3);
    return (((y >> 3) * (width >> 3) + (x >> 3)) * 64 + m) * bpp;
}

ivec2 pixel(int index, int width, int is_tiled) {
    if (is_tiled == 0) {
        return ivec2(index % width, index / width);
    }
    int m = index & 63;
    int x = (m & 1) | ((m >> 1) & 2) | ((m >> 2) & 4);
    int y = ((m >> 1) & 1) | ((m >> 2) & 2) | ((m >> 3) & 4);
    int tile = index >> 6;
    int across = width >> 3;
    return ivec2(tile % across * 8 + x, tile / across * 8 + y);
}

uint encode(uvec4 c, int format) {
    switch (format) {
    case 0:
        return c.a | (c.b << 8) | (c.g << 16) | (c.r << 24);
    case 1:
        return c.b | (c.g << 8) | (c.r << 16);
    case 2:
        return ((c.r >> 3) << 11) | ((c.g >> 2) << 5) | (c.b >> 3);
    case 3:
        return ((c.r >> 3) << 11) | ((c.g >> 3) << 6) | ((c.b >> 3) << 1) | (c.a >= 128u ? 1u : 0u);
    default:
        return ((c.r >> 4) << 12) | ((c.g >> 4) << 8) | ((c.b >> 4) << 4) | (c.a >> 4);
    }
}

uint expand(uint value, uint bits) {
    return (value << (8u - bits)) | (value >> (2u * bits - 8u));
}

uvec4 decode(uint v, int format) {
    switch (format) {
    case 0:
        return uvec4(v >> 24, (v >> 16) & 255u, (v >> 8) & 255u, v & 255u);
    case 1:
        return uvec4((v >> 16) & 255u, (v >> 8) & 255u, v & 255u, 255u);
    case 2:
        return uvec4(expand(v >> 11, 5u), expand((v >> 5) & 63u, 6u), expand(v & 31u, 5u), 255u);
    case 3:
        return uvec4(expand(v >> 11, 5u), expand((v >> 6) & 31u, 5u), expand((v >> 1) & 31u, 5u), (v & 1u) != 0u ? 255u : 0u);
    default:
        return uvec4((v >> 12) * 17u, ((v >> 8) & 15u) * 17u, ((v >> 4) & 15u) * 17u, (v & 15u) * 17u);
    }
}

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= target_shape.x || at.y >= rows.y) {
        return;
    }
    int y = rows.x + at.y;
    int start = delta + place(at.x, y, target_shape.x, target_shape.z, tiled.x);
    uint value = 0u;
    for (int k = 0; k < target_shape.z; k++) {
        int offset = start + k;
        ivec2 p = pixel(offset / source_shape.z, source_shape.x, tiled.y);
        uvec4 c = uvec4(round(imageLoad(source, ivec2(p.x, source_shape.y - 1 - p.y)) * 255.0));
        value |= ((encode(c, source_shape.w) >> uint(8 * (offset % source_shape.z))) & 255u) << uint(8 * k);
    }
    imageStore(target, ivec2(at.x, target_shape.y - 1 - y), vec4(decode(value, target_shape.w)) / 255.0);
}
"#;

pub(super) const UPRIGHT_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D upright;
uniform ivec2 size;
// the source drawn at this over 4 times the console's size
uniform int quarters;

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= size.x || at.y >= size.y) {
        return;
    }
    ivec2 from = ivec2(size.y - 1 - at.y, size.x - 1 - at.x);
    imageStore(upright, at, imageLoad(source, (from * quarters + quarters / 2) / 4));
}
"#;

pub(super) const PACK_VERTEX_SOURCE: &str = r#"#version 310 es
void main() {
    vec2 corner = vec2(float((gl_VertexID & 1) * 4 - 1), float((gl_VertexID & 2) * 2 - 1));
    gl_Position = vec4(corner, 0.0, 1.0);
}
"#;

pub(super) const PACK_DEPTH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D depth;
layout(location = 0) out vec4 color;
void main() {
    uint value = uint(round(texelFetch(depth, ivec2(gl_FragCoord.xy), 0).r * 16777215.0));
    color = vec4(float(value & 255u), float((value >> 8) & 255u), float(value >> 16), 0.0) / 255.0;
}
"#;

pub(super) const PACK_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(0.0, 0.0, 0.0, float(texelFetch(stencil, ivec2(gl_FragCoord.xy), 0).r) / 255.0);
}
"#;

pub(super) const COPY_COLOR_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D source;
uniform int base;
uniform bool opaque;
uniform vec4 levels;
layout(location = 0) out vec4 color;
void main() {
    vec4 texel = texelFetch(source, ivec2(int(gl_FragCoord.x), max(base - int(gl_FragCoord.y), 0)), 0);
    texel = floor(round(texel * 255.0) * (levels + 1.0) / 256.0) / levels;
    color = opaque ? vec4(texel.rgb, 1.0) : texel;
}
"#;

pub(super) const COPY_DEPTH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D depth;
uniform int base;
uniform bool stencil;
layout(location = 0) out vec4 color;
void main() {
    uint value = uint(round(texelFetch(depth, ivec2(int(gl_FragCoord.x), base - int(gl_FragCoord.y)), 0).r * 16777215.0));
    vec3 bytes = vec3(float(value >> 16), float((value >> 8) & 255u), float(value & 255u)) / 255.0;
    color = stencil ? vec4(0.0, bytes) : vec4(bytes, 1.0);
}
"#;

pub(super) const COPY_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
uniform int base;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(float(texelFetch(stencil, ivec2(int(gl_FragCoord.x), base - int(gl_FragCoord.y)), 0).r) / 255.0, 0.0, 0.0, 0.0);
}
"#;

pub(super) const SHADING_BLOCK: &str = "layout(std140, binding = 6) uniform Shading {
    vec4 floats[96];
    ivec4 integers[4];
    uvec4 shading_misc;
    vec4 depth_map;
    vec4 viewport;
};
#define bools shading_misc.x
";

pub(super) fn vertex_es(source: &str) -> Result<String, String> {
    let rules = [
        ("#version 450\n", "#version 310 es\nprecision highp float;\nprecision highp int;\n"),
        ("layout(location = 3) noperspective out float out_depth;", "layout(location = 3) out float out_depth_w;\nlayout(location = 6) out float out_w;"),
        ("gl_Position = vec4(position.xy, -position.z, position.w);", "gl_Position = vec4(position.xy, -2.0 * position.z - position.w, position.w);"),
        (
            "    out_depth = position.z / position.w * depth_map.x + depth_map.y;",
            "    out_depth_w = (position.z / position.w * depth_map.x + depth_map.y) * position.w;\n    out_w = position.w;",
        ),
        ("layout(std430, binding = 7) readonly buffer Inputs {\n    vec4 inputs[];\n};\n", ""),
    ];
    let mut out = source.replace("set = 0, ", "");
    for (from, to) in rules {
        if !out.contains(from) {
            return Err(format!("the translation has no {from:?}"));
        }
        out = out.replace(from, to);
    }
    let start = out.find("layout(std430, binding = 6) readonly buffer Shading {").ok_or("the translation has no Shading block")?;
    let end = start + out[start..].find("};\n").ok_or("the Shading block does not end")? + 3;
    out.replace_range(start..end, SHADING_BLOCK);
    let lines: Vec<String> = out.lines().filter(|line| !line.trim_start().starts_with("uint base = uint(gl_VertexIndex)")).map(str::to_owned).collect();
    out = lines.join("\n") + "\n";
    let mut inputs = std::collections::BTreeSet::new();
    let mut rewritten = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find("inputs[base + ") {
        rewritten.push_str(&rest[..at]);
        let after = &rest[at + "inputs[base + ".len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let slot: usize = digits.parse().map_err(|_| "an input read the translation did not number")?;
        if !after[digits.len()..].starts_with("u]") {
            return Err("an input read the translation did not number".into());
        }
        inputs.insert(slot);
        rewritten.push_str(&format!("in_{slot}"));
        rest = &after[digits.len() + 2..];
    }
    rewritten.push_str(rest);
    if rewritten.contains("gl_VertexIndex") || rewritten.contains("inputs[") {
        return Err("the translation reads its inputs some other way".into());
    }
    let declarations: String = inputs.iter().map(|slot| format!("layout(location = {slot}) in vec4 in_{slot};\n")).collect();
    let header_end = rewritten.find("precision highp int;\n").map(|at| at + "precision highp int;\n".len()).unwrap_or(0);
    rewritten.insert_str(header_end, &declarations);
    Ok(rewritten)
}

/// the generic fragment program's constants, which draws whatever the
/// combination through uniforms.
/// the registers fragment_constants reads, which the manifest keeps whole,
/// so a later build works its own constants out from them.
pub(super) fn key_registers() -> impl Iterator<Item = usize> {
    STAGE_REGISTERS
        .into_iter()
        .flat_map(|base| [base, base + 1, base + 2, base + 4])
        .chain(TEXTURE_UNIT_BASES.map(|base| base + 2))
        .chain([REG_UPDATE_BUFFER, REG_TEXTURE_CONFIG, REG_ALPHA_TEST, REG_COLOR_OPERATION, REG_BLEND_FUNC, REG_LOGIC_OP, REG_DEPTH_COLOR_MASK, REG_COLOR_BUFFER_WRITE])
        .chain(crate::lighting::SHAPE_REGISTERS)
}

/// a draw's fragment program as its constants and its lighting's shape,
/// from its registers, the units enabled, those with their alpha apart and
/// those reading a drawing in place, how its depth goes, and its lighting
/// as the uniform block packs it.
pub(super) fn fragment_constants(r: &[u32], enabled: u32, split: u32, sources: [bool; 3], depth_flags: u32, writes_depth: bool, lighting: Option<&[u32]>) -> ([u32; FRAGMENT_CONSTANTS], Vec<u32>) {
    let mut constants = [0u32; FRAGMENT_CONSTANTS];
    for (i, base) in STAGE_REGISTERS.into_iter().enumerate() {
        constants[i] = r[base] & 0x0FFF_0FFF;
        constants[6 + i] = r[base + 1] & 0x0077_7FFF;
        constants[12 + i] = r[base + 2] & 0x000F_000F;
        constants[18 + i] = r[base + 4] & 0x0003_0003;
    }
    // the buffer updates, and the fog's mode and which way its depth runs
    constants[24] = r[REG_UPDATE_BUFFER] & 0x1_FF07;
    constants[25] = ((r[REG_TEXTURE_CONFIG] & !7) | enabled) & 0x2707 | split << 24;
    for (unit, base) in TEXTURE_UNIT_BASES.into_iter().enumerate() {
        let config = r[base + 2];
        let unbordered = enabled & 1 << unit != 0 && (config >> 8) & 3 != 1 && (config >> 12) & 3 != 1;
        constants[25] |= (sources[unit] as u32) << (16 + unit) | (unbordered as u32) << (20 + unit);
    }
    // unit 0 projected, which divides its coordinates by set 0's w
    constants[25] |= (enabled & 1 != 0 && r[TEXTURE_UNIT_BASES[0] + 2] >> 28 & 7 == 3) as u32 * PROJECTED;
    constants[26] = lighting.is_some() as u32;
    constants[27] = r[REG_ALPHA_TEST] & 0x71 | unused_outputs(r);
    constants[28] = depth_flags | if writes_depth { WRITES_DEPTH } else { 0 };
    (constants, lighting.map_or_else(Vec::new, lighting_shape))
}

pub(super) fn generic_constants(constants: &[u32; FRAGMENT_CONSTANTS]) -> [u32; FRAGMENT_CONSTANTS] {
    let mut generic = [0u32; FRAGMENT_CONSTANTS];
    generic[25] = constants[25] & 0x400;
    generic[26] = constants[26];
    generic[28] = constants[28] & WRITES_DEPTH;
    generic[29] = 1;
    generic
}

pub(super) fn lighting_shape(w: &[u32]) -> Vec<u32> {
    let count = w[1] as usize;
    let mut shape = Vec::with_capacity(29 + count * 2);
    shape.extend_from_slice(&w[..8]);
    for i in 0..7 {
        shape.extend([w[8 + i * 4], w[9 + i * 4], w[10 + i * 4]]);
    }
    for slot in 0..count {
        shape.extend([w[40 + slot * 36 + 32], w[40 + slot * 36 + 33]]);
    }
    shape
}

pub(super) fn lighting_source(shape: &[u32]) -> String {
    let (config, count, bump, shadow) = (shape[0], shape[1] as usize, shape[2], shape[3]);
    let (fresnel_primary, fresnel_secondary, clamp_highlights, needs) = (shape[4] != 0, shape[5] != 0, shape[6] != 0, shape[7]);
    let lookup = |i: usize| (shape[8 + i * 3] != 0, shape[9 + i * 3], shape[10 + i * 3] != 0);
    let mut out = String::from("void shade(vec4 textures[4], out vec4 diffuse_out, out vec4 specular_out) {\n");
    out += "    float length_q = length(in_quaternion);\n";
    out += "    vec4 q = length_q > 0.0 ? in_quaternion / length_q : vec4(0.0, 0.0, 0.0, 1.0);\n";
    out += "    vec4 shadow = vec4(1.0);\n";
    if shadow & 1 != 0 {
        out += &format!("    shadow = textures[{}];\n", (shadow >> 4) & 3);
        if shadow & 0x100 != 0 {
            out += "    shadow = vec4(1.0) - shadow;\n";
        }
    }
    out += "    vec3 surface_normal = vec3(0.0, 0.0, 1.0);\n    vec3 surface_tangent = vec3(1.0, 0.0, 0.0);\n";
    match bump & 3 {
        1 => {
            out += &format!("    surface_normal = textures[{}].xyz * 2.0 - 1.0;\n", (bump >> 4) & 3);
            if bump & 0x100 != 0 {
                out += "    surface_normal.z = sqrt(max(1.0 - dot(surface_normal.xy, surface_normal.xy), 0.0));\n";
            }
        }
        2 => out += &format!("    surface_tangent = textures[{}].xyz * 2.0 - 1.0;\n", (bump >> 4) & 3),
        _ => {}
    }
    out += "    vec3 normal = rotate(q, surface_normal);\n";
    out += if config == 8 { "    vec3 tangent = rotate(q, surface_tangent);\n" } else { "    vec3 tangent = surface_tangent;\n" };
    out += "    vec3 view = in_view.xyz;\n";
    out += if needs & 2 != 0 { "    vec3 norm_view = normalize(view);\n" } else { "    vec3 norm_view = vec3(0.0);\n" };
    out += "    vec4 diffuse_sum = vec4(0.0, 0.0, 0.0, 1.0);\n    vec4 specular_sum = vec4(0.0, 0.0, 0.0, 1.0);\n";
    const TABLES: [&str; 6] = ["DISTRIBUTION0", "DISTRIBUTION1", "FRESNEL", "REFLECT_RED", "REFLECT_GREEN", "REFLECT_BLUE"];
    for slot in 0..count {
        let (flags, number) = (shape[29 + slot * 2], shape[30 + slot * 2]);
        let two_sided = flags & 2 != 0;
        let l = format!("lights[{slot}]");
        out += "    {\n";
        if flags & 1 != 0 {
            out += &format!("        vec3 light_vector = {l}.direction.xyz;\n");
        } else {
            out += &format!("        vec3 light_vector = normalize({l}.position.xyz + view);\n");
        }
        out += "        vec3 half_vector = vec3(0.0);\n        vec3 half_unit = vec3(0.0);\n";
        if needs & 1 != 0 {
            out += "        half_vector = norm_view + light_vector;\n        half_unit = normalize(half_vector);\n";
        }
        out += "        float distance = 1.0;\n";
        if flags & 16 != 0 {
            out += &format!("        {{\n            vec3 offset = -view - {l}.position.xyz;\n");
            out += &format!("            float place = clamp({l}.distance.y * length(offset) + {l}.distance.x, 0.0, 1.0);\n");
            out += "            float entry = clamp(floor(place * 256.0), 0.0, 255.0);\n";
            out += &format!("            distance = lookup(DISTANCE + {number}u, uint(entry), place * 256.0 - entry);\n        }}\n");
        }
        for i in 0..7 {
            let (enabled, input, absolute) = lookup(i);
            if !enabled || (i == 6 && flags & 32 == 0) {
                out += &format!("        float v{i} = 1.0;\n");
                continue;
            }
            let expression = match input {
                0 => "dot(normal, half_unit)".to_owned(),
                1 => "dot(norm_view, half_unit)".to_owned(),
                2 => "dot(normal, norm_view)".to_owned(),
                3 => "dot(light_vector, normal)".to_owned(),
                4 => format!("dot(light_vector, {l}.spot.xyz)"),
                5 if config == 8 => "dot(half_unit - normal * dot(normal, half_unit), tangent)".to_owned(),
                _ => "0.0".to_owned(),
            };
            let table = if i == 6 { format!("SPOTLIGHT + {number}u") } else { TABLES[i].to_owned() };
            out += &format!("        float v{i};\n        {{\n            float result = {expression};\n");
            if absolute {
                out += if two_sided { "            result = abs(result);\n" } else { "            result = max(result, 0.0);\n" };
                out += "            float e = clamp(floor(result * 256.0), 0.0, 255.0);\n";
                out += &format!("            v{i} = uintBitsToFloat(lookups[{i}].w) * lookup({table}, uint(e), result * 256.0 - e);\n");
            } else {
                out += "            float e = clamp(floor(result * 128.0), -128.0, 127.0);\n";
                out += &format!("            v{i} = uintBitsToFloat(lookups[{i}].w) * lookup({table}, uint(int(e)) & 0xFFu, result * 128.0 - e);\n");
            }
            out += "        }\n";
        }
        let green = if lookup(4).0 { "v4" } else { "v3" };
        let blue = if lookup(5).0 { "v5" } else { "v3" };
        out += &format!("        vec3 reflect_color = vec3(v3, {green}, {blue});\n");
        out += &format!("        vec3 specular0 = {l}.specular0.rgb * v0;\n");
        out += &format!("        vec3 specular1 = v1 * reflect_color * {l}.specular1.rgb;\n");
        if slot == count - 1 && lookup(2).0 {
            if fresnel_primary {
                out += "        diffuse_sum.a = v2;\n";
            }
            if fresnel_secondary {
                out += "        specular_sum.a = v2;\n";
            }
        }
        out += "        float facing = dot(light_vector, normal);\n";
        out += if two_sided { "        facing = abs(facing);\n" } else { "        facing = max(facing, 0.0);\n" };
        out += if clamp_highlights { "        float highlights = facing == 0.0 ? 0.0 : 1.0;\n" } else { "        float highlights = 1.0;\n" };
        if flags & 12 != 0 {
            out += "        float length2 = dot(half_vector, half_vector);\n";
            out += "        float factor = length2 == 0.0 ? 0.0 : min(facing / length2, 1.0);\n";
            if flags & 4 != 0 {
                out += "        specular0 *= factor;\n";
            }
            if flags & 8 != 0 {
                out += "        specular1 *= factor;\n";
            }
        }
        let shadowed = flags & 64 != 0 && shadow & 1 != 0;
        let primary = if shadowed && shadow & 0x200 != 0 { "shadow.rgb" } else { "vec3(1.0)" };
        let secondary = if shadowed && shadow & 0x400 != 0 { "shadow.rgb" } else { "vec3(1.0)" };
        out += &format!("        vec3 diffuse = ({l}.diffuse.rgb * facing + {l}.ambient.rgb) * distance * v6;\n");
        out += "        vec3 specular = (specular0 + specular1) * highlights * distance * v6;\n";
        out += &format!("        diffuse_sum.rgb += diffuse * {primary};\n        specular_sum.rgb += specular * {secondary};\n    }}\n");
    }
    if shadow & 0x800 != 0 {
        if fresnel_primary {
            out += "    diffuse_sum.a *= shadow.a;\n";
        }
        if fresnel_secondary {
            out += "    specular_sum.a *= shadow.a;\n";
        }
    }
    out += "    diffuse_sum.rgb += global_ambient.rgb;\n";
    out += "    diffuse_out = vec4(quantize(diffuse_sum.r), quantize(diffuse_sum.g), quantize(diffuse_sum.b), quantize(diffuse_sum.a));\n";
    out += "    specular_out = vec4(quantize(specular_sum.r), quantize(specular_sum.g), quantize(specular_sum.b), quantize(specular_sum.a));\n";
    out += "}\n\n";
    out
}

/// a vertex program multiplying as the host does rather than working out the
/// PICA's 0 times infinity = 0 for every product, a third of what the Mali
/// spent on a battle. what makes infinities, reciprocals, roots, logarithms
/// and powers, is held at the largest finite float instead, so 0 times one
/// of them still comes to 0.
pub(super) fn plain_multiplies(source: &str) -> String {
    const LARGEST: &str = "-3.4028235e38, 3.4028235e38";
    source
        .replace("    return mix(product, mix(mix(0.0, product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("    return mix(product, mix(mix(vec4(0.0), product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("vec4(1.0 / a.x);", &format!("vec4(clamp(1.0 / a.x, {LARGEST}));"))
        .replace("vec4(1.0 / sqrt(a.x));", &format!("vec4(clamp(1.0 / sqrt(a.x), {LARGEST}));"))
        .replace("vec4(log2(a.x));", &format!("vec4(clamp(log2(a.x), {LARGEST}));"))
        .replace("vec4(exp2(a.x));", &format!("vec4(clamp(exp2(a.x), {LARGEST}));"))
}

/// the six combiner stages of one combination as straight code, each
/// reading only what it uses: Mali's compiler on the R36S builds a program
/// in three quarters of the time it takes over the generic stages' switches.
pub(super) fn combiner_source(constants: &[u32; FRAGMENT_CONSTANTS]) -> String {
    let input = |source: u32, stage: usize| match source {
        0 => "primary".to_owned(),
        1 => "fragment_primary".to_owned(),
        2 => "fragment_secondary".to_owned(),
        3..=6 => format!("textures[{}]", source - 3),
        0xD => "held".to_owned(),
        0xE => format!("colors[{stage}]"),
        _ => "previous".to_owned(),
    };
    let color = |v: &str, operand: u32| match operand {
        0x1 => format!("vec3(1.0) - {v}.rgb"),
        0x2 => format!("vec3({v}.a)"),
        0x3 => format!("vec3(1.0 - {v}.a)"),
        0x4 => format!("vec3({v}.r)"),
        0x5 => format!("vec3(1.0 - {v}.r)"),
        0x8 => format!("vec3({v}.g)"),
        0x9 => format!("vec3(1.0 - {v}.g)"),
        0xC => format!("vec3({v}.b)"),
        0xD => format!("vec3(1.0 - {v}.b)"),
        _ => format!("{v}.rgb"),
    };
    let alpha = |v: &str, operand: u32| match operand {
        0 => format!("{v}.a"),
        1 => format!("1.0 - {v}.a"),
        2 => format!("{v}.r"),
        3 => format!("1.0 - {v}.r"),
        4 => format!("{v}.g"),
        5 => format!("1.0 - {v}.g"),
        6 => format!("{v}.b"),
        _ => format!("1.0 - {v}.b"),
    };
    // how many inputs an operation reads, and it on them
    let inputs = |op: u32| match op {
        0 => 1,
        4 | 8 | 9 => 3,
        _ => 2,
    };
    let combine = |op: u32, one: &str, zero: &str, p: &str| match op {
        0 => format!("{p}0"),
        1 => format!("{p}0 * {p}1"),
        2 => format!("min({p}0 + {p}1, {one})"),
        3 => format!("clamp({p}0 + {p}1 - 0.5, 0.0, 1.0)"),
        4 => format!("{p}0 * {p}2 + {p}1 * ({one} - {p}2)"),
        5 => format!("max({p}0 - {p}1, {zero})"),
        6 | 7 if one == "1.0" => format!("{p}0"),
        6 | 7 => format!("vec3(clamp(dot({p}0 * 2.0 - 1.0, {p}1 * 2.0 - 1.0), 0.0, 1.0))"),
        8 => format!("min({p}0 * {p}1 + {p}2, {one})"),
        _ => format!("min({p}0 + {p}1, {one}) * {p}2"),
    };
    let factor = |raw: u32| match raw & 3 {
        1 => " * 2.0",
        2 => " * 4.0",
        _ => "",
    };
    let update = constants[24] & 0xFF00;
    let mut out = String::new();
    for stage in 0..6 {
        let (source, operand, combiner, scale) = (constants[stage], constants[6 + stage], constants[12 + stage], constants[18 + stage]);
        let passes = source & 0x000F_000F == 0x000F_000F && operand & 0x700F == 0 && combiner & 0x000F_000F == 0 && scale == 0;
        if !passes {
            let (color_op, alpha_op) = ((combiner & 0xF).min(9), ((combiner >> 16) & 0xF).min(9));
            out += "    {\n";
            for i in 0..inputs(color_op) {
                out += &format!("        vec3 c{i} = {};\n", color(&input((source >> (i * 4)) & 0xF, stage), (operand >> (i * 4)) & 0xF));
            }
            out += &format!("        vec3 rgb = {};\n", combine(color_op, "vec3(1.0)", "vec3(0.0)", "c"));
            if color_op == 7 {
                out += "        float alpha = rgb.r;\n";
            } else {
                for i in 0..inputs(alpha_op) {
                    out += &format!("        float a{i} = {};\n", alpha(&input((source >> (16 + i * 4)) & 0xF, stage), (operand >> (12 + i * 4)) & 0x7));
                }
                out += &format!("        float alpha = {};\n", combine(alpha_op, "1.0", "0.0", "a"));
            }
            out += &format!("        previous = clamp(vec4(rgb{}, alpha{}), 0.0, 1.0);\n    }}\n", factor(scale), factor(scale >> 16));
        }
        out += "    held = next_buffer;\n";
        if stage < 4 && update & (0x100 << stage) != 0 {
            out += "    next_buffer.rgb = previous.rgb;\n";
        }
        if stage < 4 && update & (0x1000 << stage) != 0 {
            out += "    next_buffer.a = previous.a;\n";
        }
    }
    out
}

pub(super) fn fragment_source(constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32]) -> String {
    let mut out = String::from("#version 310 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n");
    if writes_depth {
        out.push_str("#define WRITES_DEPTH\n");
    }
    let mut skipping = false;
    for line in FRAGMENT_SOURCE.lines().skip(1) {
        if !lighting.is_empty() && line.starts_with("void shade(") {
            out += &lighting_source(lighting);
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with("precise vec3 color_operand") {
                skipping = false;
            } else {
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("layout(constant_id = ") {
            let (id, rest) = rest.split_once(')').unwrap_or_default();
            let name = rest.trim().trim_start_matches("const uint ").split(' ').next().unwrap_or_default();
            out += &format!("const uint {name} = {}u;\n", constants[id.parse::<usize>().unwrap_or(0)]);
        } else if line.contains("noperspective in float in_depth") {
            out += "layout(location = 3) in float in_depth_w;\nlayout(location = 6) in float in_w;\n#define in_depth (in_depth_w / in_w)\n";
        } else if line.trim_start().starts_with("combine_stage(") && constants[29] == 0 {
            if line.trim_start().starts_with("combine_stage(0u") {
                out += &combiner_source(constants);
            }
        } else {
            out += &line.replace("set = 0, ", "").replace("precise ", "");
            out.push('\n');
        }
    }
    if crate::fast_with(crate::speedup::HALF) {
        // in fast mode the colors at half precision, which the GPU works
        // twice as fast as full, what places texels, depth and lights kept
        // full
        for (from, to) in [
            ("precision highp float;\nprecision highp int;\nprecision highp sampler2D;", "precision mediump float;\nprecision highp int;\nprecision mediump sampler2D;"),
            ("in vec4 in_texcoords01", "in highp vec4 in_texcoords01"),
            ("in vec2 in_texcoord2", "in highp vec2 in_texcoord2"),
            ("in vec4 in_view", "in highp vec4 in_view"),
            ("in float in_depth_w", "in highp float in_depth_w"),
            ("in float in_w", "in highp float in_w"),
            ("sample_unit(uint unit, vec2 uv)", "sample_unit(uint unit, highp vec2 uv)"),
            ("    vec2 st = ", "    highp vec2 st = "),
            ("    float depth = in_depth", "    highp float depth = in_depth"),
            ("    vec3 view = in_view.xyz", "    highp vec3 view = in_view.xyz"),
            ("vec3 offset = -view", "highp vec3 offset = -view"),
        ] {
            out = out.replace(from, to);
        }
    }
    if ADRENO.load(std::sync::atomic::Ordering::Relaxed) {
        out = out.replace(SWITCH_COMPARE, ADRENO_COMPARE);
    }
    // ZAKURO_UNROLLED_STAND_INS: the generic program with the six stages
    // written out as before the loop, for telling whether the loop is what
    // a driver draws wrong
    static UNROLLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *UNROLLED.get_or_init(|| crate::switch("ZAKURO_UNROLLED_STAND_INS", "the generic program's stages written out")) {
        out = out.replace("    if (DYNAMIC != 0u) {\n        // the generic program, which reads every stage's registers", "    if (false) {\n        // the generic program, which reads every stage's registers");
    }
    out
}


pub(super) const PRESENT_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D source;
uniform ivec4 area;
uniform ivec4 screen;
uniform int upright;
uniform int smooth_scale;
layout(location = 0) out vec4 color;
void main() {
    if (smooth_scale != 0) {
        // each pixel of the area blends the screen's pixels it covers, so
        // they come out even when the area is not a whole multiple
        vec2 size = vec2(screen.xy);
        vec2 s = size / vec2(area.zw);
        vec2 pixel = vec2(gl_FragCoord.x - float(area.x), float(area.y + area.w) - gl_FragCoord.y) - 0.5;
        vec2 start = pixel * s;
        // more of the screen's pixels than the area's, as one drawn bigger
        // than shown: each of the area's averages a grid of reads spread
        // over all the screen's pixels it covers
        if (s.x > 1.0 || s.y > 1.0) {
            ivec2 n = ivec2(min(ceil(s), vec2(4.0)));
            vec3 sum = vec3(0.0);
            for (int j = 0; j < n.y; j++) {
                for (int i = 0; i < n.x; i++) {
                    vec2 at = clamp(start + (vec2(float(i), float(j)) + 0.5) * s / vec2(n), vec2(0.5), size - 0.5);
                    vec2 texel = upright != 0 ? at : vec2(float(screen.y) - at.y, float(screen.z - screen.w) - at.x);
                    sum += texture(source, texel / vec2(textureSize(source, 0))).rgb;
                }
            }
            color = vec4(sum / float(n.x * n.y), 1.0);
            return;
        }
        vec2 first = floor(start);
        vec2 past = clamp(((pixel + 1.0) * s - first - 1.0) / s, 0.0, 1.0);
        vec2 at = clamp(mix(first + 0.5 + past, start + 0.5 * s, step(1.0, s)), vec2(0.5), size - 0.5);
        vec2 texel = upright != 0 ? at : vec2(float(screen.y) - at.y, float(screen.z - screen.w) - at.x);
        color = vec4(texture(source, texel / vec2(textureSize(source, 0))).rgb, 1.0);
        return;
    }
    int sx = int(floor((gl_FragCoord.x - float(area.x)) * float(screen.x) / float(area.z)));
    int sy = int(floor((float(area.y + area.w) - gl_FragCoord.y) * float(screen.y) / float(area.w)));
    ivec2 at = upright != 0 ? ivec2(sx, sy) : ivec2(screen.y - 1 - sy, screen.z - 1 - screen.w - sx);
    color = vec4(texelFetch(source, at, 0).rgb, 1.0);
}
"#;

pub(super) const OVERLAY_VERTEX_SOURCE: &str = r#"#version 310 es
precision highp float;
layout(location = 0) in vec2 position;
layout(location = 1) in vec2 uv;
layout(location = 2) in vec4 color;
uniform vec2 size;
out vec2 at;
out vec4 tint;
void main() {
    at = uv;
    tint = color;
    gl_Position = vec4(position.x / size.x * 2.0 - 1.0, 1.0 - position.y / size.y * 2.0, 0.0, 1.0);
}
"#;

pub(super) const OVERLAY_FRAGMENT_SOURCE: &str = r#"#version 310 es
precision mediump float;
uniform sampler2D source;
in vec2 at;
in vec4 tint;
layout(location = 0) out vec4 color;
void main() {
    color = texture(source, at) * tint;
}
"#;

pub(super) const ROTATE_SOURCE: &str = r#"#version 310 es
precision highp float;
uniform highp sampler2D canvas;
uniform vec4 rect;
uniform int turn;
layout(location = 0) out vec4 color;
void main() {
    vec2 p = (gl_FragCoord.xy - rect.xy) / rect.zw;
    if (p.x < 0.0 || p.y < 0.0 || p.x > 1.0 || p.y > 1.0) {
        color = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }
    vec2 uv = turn == 0 ? vec2(1.0 - p.y, p.x) : vec2(p.y, 1.0 - p.x);
    color = vec4(texture(canvas, uv).rgb, 1.0);
}
"#;
