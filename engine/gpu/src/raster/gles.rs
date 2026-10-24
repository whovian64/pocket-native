//! filling triangles on the host's GPU through OpenGL ES 3.1, for GPUs
//! without Vulkan. the CPU shades, clips and places the triangles, the GPU
//! does the pixels with raster.frag. buffers the GPU draws into go back to
//! guest memory when the command list ends or something else reads them,
//! everything else goes through the software paths.

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::sync::Arc;
use std::thread::{self, ThreadId};

use glow::HasContext;
use glutin_egl_sys::egl;

use super::host::{Draw, Geometry, Input, Shading, Transfer};
use between::{Between, Step};
use super::{color_words, BoundTexture, DepthMap, DrawnTexture, Screen, Wrap, COLOR_WORDS, TEXTURE_UNIT_BASES};
use crate::blend::LogicOp;
use crate::format::{morton_offset, ColorFormat};
use crate::lighting::Tables;
use crate::proctex;

mod between;
use crate::registers::*;
use crate::shader::glsl::{self, SEMANTICS};
use crate::shader::INPUT_REGISTERS;
use crate::{GpuMemory, Picture, ScreenRef};

const FRAGMENT_SOURCE: &str = include_str!("../../shaders/raster.frag");

const VERTEX_SOURCE: &str = r#"#version 310 es
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
layout(location = 5) out vec3 out_view;
layout(location = 6) out float out_w;

void main() {
    float depth = clamp(texcoord2_depth.z, 0.0, 1.0);
    gl_Position = vec4(position.xy, (depth * 2.0 - 1.0) * position.w, position.w);
    out_color = color;
    out_texcoords01 = texcoords01;
    out_texcoord2 = texcoord2_depth.xy;
    out_depth_w = texcoord2_depth.z * position.w;
    out_quaternion = quaternion;
    out_view = view.xyz;
    out_w = position.w;
}
"#;

const TRANSFER_SOURCE: &str = r#"#version 310 es
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

const UPRIGHT_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D upright;
uniform ivec2 size;

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= size.x || at.y >= size.y) {
        return;
    }
    imageStore(upright, at, imageLoad(source, ivec2(size.y - 1 - at.y, size.x - 1 - at.x)));
}
"#;

const PACK_VERTEX_SOURCE: &str = r#"#version 310 es
void main() {
    vec2 corner = vec2(float((gl_VertexID & 1) * 4 - 1), float((gl_VertexID & 2) * 2 - 1));
    gl_Position = vec4(corner, 0.0, 1.0);
}
"#;

const PACK_DEPTH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D depth;
layout(location = 0) out vec4 color;
void main() {
    uint value = uint(round(texelFetch(depth, ivec2(gl_FragCoord.xy), 0).r * 16777215.0));
    color = vec4(float(value & 255u), float((value >> 8) & 255u), float(value >> 16), 0.0) / 255.0;
}
"#;

const PACK_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(0.0, 0.0, 0.0, float(texelFetch(stencil, ivec2(gl_FragCoord.xy), 0).r) / 255.0);
}
"#;

const COPY_COLOR_SOURCE: &str = r#"#version 310 es
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

const COPY_DEPTH_SOURCE: &str = r#"#version 310 es
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

const COPY_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
uniform int base;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(float(texelFetch(stencil, ivec2(int(gl_FragCoord.x), base - int(gl_FragCoord.y)), 0).r) / 255.0, 0.0, 0.0, 0.0);
}
"#;

const VERTEX_SIZE: usize = 24 * 4;
const PROCTEX_WORDS: usize = 8;
const UNIFORM_WORDS: usize = 12 * 4 + 4 + 3 * 4 + 4 + 328 + PROCTEX_WORDS + COLOR_WORDS;
const TABLES_SIZE: usize = (24 * 256 + 3 * proctex::MAP_ENTRIES + 4 * proctex::COLOR_ENTRIES) * 8;
const FRAGMENT_CONSTANTS: usize = 30;
const WRITES_DEPTH: u32 = 4;

const STAGE_REGISTERS: [usize; 6] = [0x0C0, 0x0C8, 0x0D0, 0x0D8, 0x0F0, 0x0F8];
const REG_UPDATE_BUFFER: usize = 0x0E0;
const REG_BUFFER_COLOR: usize = 0x0FD;
const REG_BLEND_COLOR: usize = 0x103;

const COMPARES: [u32; 8] = [glow::NEVER, glow::ALWAYS, glow::EQUAL, glow::NOTEQUAL, glow::LESS, glow::LEQUAL, glow::GREATER, glow::GEQUAL];
const STENCIL_OPS: [u32; 8] = [glow::KEEP, glow::ZERO, glow::REPLACE, glow::INCR, glow::DECR, glow::INVERT, glow::INCR_WRAP, glow::DECR_WRAP];
const FACTORS: [u32; 15] = [
    glow::ZERO,
    glow::ONE,
    glow::SRC_COLOR,
    glow::ONE_MINUS_SRC_COLOR,
    glow::DST_COLOR,
    glow::ONE_MINUS_DST_COLOR,
    glow::SRC_ALPHA,
    glow::ONE_MINUS_SRC_ALPHA,
    glow::DST_ALPHA,
    glow::ONE_MINUS_DST_ALPHA,
    glow::CONSTANT_COLOR,
    glow::ONE_MINUS_CONSTANT_COLOR,
    glow::CONSTANT_ALPHA,
    glow::ONE_MINUS_CONSTANT_ALPHA,
    glow::SRC_ALPHA_SATURATE,
];
const EQUATIONS: [u32; 5] = [glow::FUNC_ADD, glow::FUNC_SUBTRACT, glow::FUNC_REVERSE_SUBTRACT, glow::MIN, glow::MAX];

const TEXTURE_KEEP_FLUSHES: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Color(ColorFormat),
    Depth(u32),
}

impl Kind {
    fn bytes(self) -> u32 {
        match self {
            Kind::Color(format) => format.bytes_per_pixel() as u32,
            Kind::Depth(bytes) => bytes,
        }
    }
}

struct Surface {
    texture: glow::Texture,
    addr: u32,
    width: u32,
    height: u32,
    kind: Kind,
    tiled: bool,
    shadow: Vec<u8>,
    checked: bool,
    dirty: Option<(u32, u32)>,
    screen_batch: u64,
    picture: Option<(u64, Arc<Vec<u8>>)>,
    upright: Option<(glow::Texture, glow::Framebuffer)>,
    packed: Option<(glow::Texture, glow::Framebuffer)>,
    version: u64,
    stale: bool,
}

static VERSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// words as their bytes, which is how the GPU reads them on a little-endian
/// host.
fn word_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: the words' own bytes
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), words.len() * 4) }
}

fn next_version() -> u64 {
    VERSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Surface {
    fn size(&self) -> u32 {
        self.width * self.height * self.kind.bytes()
    }

    fn overlaps(&self, addr: u32, len: u32) -> bool {
        addr < self.addr + self.size() && self.addr < addr + len
    }

    fn row_bytes(&self) -> u32 {
        self.width * self.kind.bytes()
    }

    fn dirty_overlaps(&self, addr: u32, len: u32) -> bool {
        self.dirty.is_some_and(|(start, end)| {
            let (from, to) = (self.addr + start * self.row_bytes(), self.addr + end * self.row_bytes());
            addr < to && from < addr + len
        })
    }

    fn drew(&mut self, (start, end): (u32, u32)) {
        let rows = ((start / 8 * 8).min(self.height), end.div_ceil(8).saturating_mul(8).min(self.height));
        self.dirty = Some(match self.dirty {
            Some((from, to)) => (from.min(rows.0), to.max(rows.1)),
            None => rows,
        });
        self.picture = None;
        self.version = next_version();
    }
}

fn crop(image: Arc<Vec<u8>>, screen: ScreenRef) -> Arc<Vec<u8>> {
    let (row_pixels, rows) = screen.size;
    let (first, count) = screen.rows;
    if first == 0 && count == rows && screen.columns == row_pixels {
        return image;
    }
    let width = rows as usize;
    let (left, right) = (first as usize, (first + count) as usize);
    let (top, bottom) = ((row_pixels - screen.columns) as usize, row_pixels as usize);
    let mut out = Vec::with_capacity((right - left) * (bottom - top) * 4);
    for y in top..bottom {
        out.extend_from_slice(&image[(y * width + left) * 4..(y * width + right) * 4]);
    }
    Arc::new(out)
}

const TIMED: [&str; 21] = ["display_transfer", "picture", "upload", "clear", "read_depth", "write_back", "texture", "upload_tables", "draw", "draw_gl", "surface_rows", "filled", "depth_pass", "stencil_pass", "depth_read", "gpu_pending", "shading_bytes", "push", "gl_draw", "uniform_words", "gl_wait"];
static TIMES: [std::sync::atomic::AtomicU64; 42] = [const { std::sync::atomic::AtomicU64::new(0) }; 42];

struct Timed(usize, std::time::Instant);

impl Drop for Timed {
    fn drop(&mut self) {
        TIMES[self.0 * 2].fetch_add(self.1.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        TIMES[self.0 * 2 + 1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// whether memory still holds what a surface was made from, in fast mode
/// judged from a few spots, so a buffer the CPU rewrote whole is still seen.
fn same(bytes: &[u8], shadow: &[u8]) -> bool {
    if !crate::fast_with(crate::speedup::SPOTS) || bytes.len() != shadow.len() || bytes.len() < 64 * 64 {
        return bytes == shadow;
    }
    let step = (bytes.len() - 64) / 63;
    (0..64).all(|spot| bytes[spot * step..spot * step + 64] == shadow[spot * step..spot * step + 64])
}

/// counts draws by what they are, ZAKURO_DRAW_STATS=1 prints the busiest
/// every 600 calls of a few seconds, ZAKURO_DROP_DRAWS=prefix,prefix>n
/// leaves out the draws whose signature starts with one (of more than n
/// indices) and
/// ZAKURO_THIN_DRAWS=prefix thins them, whether dropped and whether thinned.
fn draw_filter(signature: impl FnOnce() -> String, count: usize) -> (bool, bool) {
    use std::sync::OnceLock;
    static STATS: OnceLock<bool> = OnceLock::new();
    static THIN: OnceLock<String> = OnceLock::new();
    static COUNTS: std::sync::Mutex<Option<(HashMap<String, (u64, u64)>, u64)>> = std::sync::Mutex::new(None);
    let drops = drops();
    let drop = &drops.prefixes;
    let stats = *STATS.get_or_init(|| std::env::var_os("ZAKURO_DRAW_STATS").is_some());
    let thin = THIN.get_or_init(|| std::env::var("ZAKURO_THIN_DRAWS").unwrap_or_default());
    if drop.is_empty() && !stats && thin.is_empty() {
        return (false, false);
    }
    let signature = signature();
    let dropped = drop.iter().any(|(prefix, least)| count > *least && signature.starts_with(prefix.as_str()));
    if stats {
        let mut counts = COUNTS.lock().unwrap();
        let (map, total) = counts.get_or_insert_with(Default::default);
        let entry = map.entry(signature.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += count as u64;
        *total += 1;
        if *total % 20000 == 0 {
            let mut busiest: Vec<_> = map.iter().collect();
            busiest.sort_by_key(|(_, (draws, _))| std::cmp::Reverse(*draws));
            for (signature, (draws, indices)) in busiest.into_iter().take(25) {
                log::info!("draws {draws:6} indices {indices:9} {signature}");
            }
            map.clear();
        }
    }
    (dropped, !thin.is_empty() && signature.starts_with(thin.as_str()))
}

/// the fragment stages' outputs nothing reads, bit 8 the color and bit 9
/// the alpha, for the alpha test constant: masked off, or blended in with a
/// zero factor and read by no factor, so the shader leaves out what only
/// they need.
fn unused_outputs(r: &[u32]) -> u32 {
    let mask = if r[REG_COLOR_BUFFER_WRITE] != 0 && LogicOp::read(r) != Some(LogicOp::Noop) { (r[REG_DEPTH_COLOR_MASK] >> 8) & 0xF } else { 0 };
    let (color, alpha) = (mask & 7 != 0, mask & 8 != 0);
    let (color_used, alpha_used) = if r[REG_COLOR_OPERATION] & 0x100 == 0 {
        (color, alpha)
    } else {
        let config = r[REG_BLEND_FUNC];
        let field = |shift: u32| (config >> shift) & 0xF;
        let min_max = |equation: u32| matches!(equation & 7, 3 | 4);
        let reads_alpha = |factor: u32| matches!(factor, 6 | 7 | 14);
        let color_blended = color && !min_max(config);
        (
            color && (min_max(config) || field(16) != 0 || matches!(field(20), 2 | 3)),
            alpha && (min_max(config >> 8) || field(24) != 0 || matches!(field(28), 2 | 3) || reads_alpha(field(28)))
                || color_blended && (reads_alpha(field(16)) || reads_alpha(field(20))),
        )
    };
    let alpha_used = alpha_used || r[REG_ALPHA_TEST] & 1 != 0;
    (!color_used as u32) << 8 | (!alpha_used as u32) << 9
}

/// the draws left out: signature prefixes, each with the indices a draw
/// needs more than (prefix>n), and, for @depth or @depth:prefix, the
/// prefixes of the screen-wide passes that read a depth buffer as a texture,
/// as the shading battles put over the Pokémon.
struct Drops {
    prefixes: Vec<(String, usize)>,
    depth: Vec<String>,
}

static DROPS: std::sync::RwLock<Option<Arc<Drops>>> = std::sync::RwLock::new(None);

/// leaves out the draws the rules name from now on, written as
/// ZAKURO_DROP_DRAWS takes them, which they replace.
pub fn set_drops(rules: &str) {
    let entries: Vec<&str> = rules.split(',').filter(|entry| !entry.is_empty()).collect();
    let prefixes = entries
        .iter()
        .filter(|entry| !entry.starts_with('@'))
        .map(|entry| match entry.split_once('>') {
            Some((prefix, least)) => (prefix.to_owned(), least.trim().parse().unwrap_or(0)),
            None => ((*entry).to_owned(), 0),
        })
        .collect();
    let depth = entries.iter().filter_map(|entry| entry.strip_prefix("@depth")).map(|rest| rest.strip_prefix(':').unwrap_or(rest).to_owned()).collect();
    *DROPS.write().unwrap() = Some(Arc::new(Drops { prefixes, depth }));
}

fn drops() -> Arc<Drops> {
    if let Some(drops) = DROPS.read().unwrap().as_ref() {
        return drops.clone();
    }
    set_drops(&std::env::var("ZAKURO_DROP_DRAWS").unwrap_or_default());
    drops()
}

/// times a stretch of GL work for ZAKURO_GLES_TIMES, nothing without it.
fn timed(index: usize) -> Option<Timed> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ON.get_or_init(|| std::env::var_os("ZAKURO_GLES_TIMES").is_some()).then(|| Timed(index, std::time::Instant::now()))
}

static WAITS: std::sync::Mutex<Vec<(&'static str, u32)>> = std::sync::Mutex::new(Vec::new());


/// a name from a pool the GL thread makes 64 at a time ahead of need,
/// sending them once it gets to it, so a new object waits on it only when
/// the pool runs dry.
fn pooled<T: Send + 'static>(worker: &Worker, gl: GlRef, (pool, coming): (&mut Vec<T>, &mut Option<std::sync::mpsc::Receiver<Vec<T>>>), make: fn(&glow::Context) -> Option<T>, waited: &'static str) -> Result<T, String> {
    if let Some(names) = coming.as_ref().and_then(|coming| coming.try_recv().ok()) {
        pool.extend(names);
        *coming = None;
    }
    if pool.len() < 32 && coming.is_none() {
        let (send, receiver) = std::sync::mpsc::channel();
        worker.post_keeping(move || drop(send.send((0..64).filter_map(|_| make(&gl)).collect::<Vec<_>>())));
        *coming = Some(receiver);
    }
    if pool.is_empty() {
        wait_from(waited);
        let names = coming.take().map(|coming| coming.recv()).ok_or("no names coming")?.map_err(|_| "the GL thread stopped")?;
        pool.extend(names);
    }
    Ok(pool.pop().expect("a name was made"))
}

thread_local! {
    /// what the next sync is for, so that its wait says so in the log.
    static SYNC_WHY: std::cell::Cell<&'static str> = const { std::cell::Cell::new("sync") };
}

/// names what the next sync on this thread is for.
pub(crate) fn sync_why(why: &'static str) {
    SYNC_WHY.with(|cell| cell.set(why));
}

/// a copy or transfer of a drawing left to the CPU, which reads it back,
/// in the log the first few times, with the drawing it reads, for telling
/// which a game needs done on the GPU.
fn left_to_cpu(what: &str, input: u32, len: u32, details: String, surfaces: &[Surface]) {
    static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let Some(s) = surfaces.iter().find(|s| s.dirty_overlaps(input, len)) else { return };
    if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 200 {
        return;
    }
    log::info!(
        "{what} left to the CPU, {len} bytes from {input:08X}, {details}; the drawing there is {:08X} {}x{} {:?} {} rows {:?}",
        s.addr,
        s.width,
        s.height,
        s.kind,
        if s.tiled { "tiled" } else { "linear" },
        s.dirty
    );
}

fn wait_from(name: &'static str) {
    let mut waits = WAITS.lock().unwrap();
    match waits.iter_mut().find(|(n, _)| *n == name) {
        Some(entry) => entry.1 += 1,
        None => waits.push((name, 1)),
    }
}

fn report_times() {
    log::info!("gl waits: {:?}", std::mem::take(&mut *WAITS.lock().unwrap()));
    let line: Vec<String> = TIMED
        .iter()
        .enumerate()
        .map(|(i, name)| format!("{name} {}ms/{}", TIMES[i * 2].swap(0, std::sync::atomic::Ordering::Relaxed) / 1000, TIMES[i * 2 + 1].swap(0, std::sync::atomic::Ordering::Relaxed)))
        .collect();
    log::info!("gl times: {}", line.join(", "));
    let culled = CULLED.each_ref().map(|count| count.swap(0, std::sync::atomic::Ordering::Relaxed));
    if culled[1] > 0 {
        log::info!("culled {} of {} heavy draws", culled[0], culled[1]);
    }
}

const MANIFEST_HEADER: &str = "zakuro-gles 4";

/// a manifest from before the texture configuration constant told which
/// units never clamp to the border, with every unit it turns on so.
fn upgrade_manifest(text: &str) -> String {
    let mut out = format!("{MANIFEST_HEADER}\n");
    for line in text.lines().skip(1) {
        let mut fields: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        if let Some(config) = fields.get_mut(2 + 25) {
            if let Ok(value) = u32::from_str_radix(config, 16) {
                *config = format!("{:x}", value | (value & 7) << 20);
            }
        }
        out += &fields.join(" ");
        out.push('\n');
    }
    out
}

fn vertex_path(manifest: &std::path::Path, hash: u64) -> std::path::PathBuf {
    let mut dir = manifest.as_os_str().to_owned();
    dir.push(".vertex");
    std::path::PathBuf::from(dir).join(format!("{hash:016x}.vert"))
}

fn manifest_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("ZAKURO_SHADER_MANIFEST").filter(|value| !value.is_empty()) {
        return Some(path.into());
    }
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let dir = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?;
    Some(dir.join("zakuro").join("gles").join("programs.txt"))
}

fn fnv(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.iter().flat_map(|part| part.bytes().chain([0])) {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

const SHADING_BLOCK: &str = "layout(std140, binding = 6) uniform Shading {
    vec4 floats[96];
    ivec4 integers[4];
    uvec4 shading_misc;
    vec4 depth_map;
    vec4 viewport;
};
#define bools shading_misc.x
";

fn vertex_es(source: &str) -> Result<String, String> {
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

fn program_cache_path(renderer: &str, vertex: &str, fragment: &str) -> Option<std::path::PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let dir = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?;
    Some(dir.join("zakuro").join("gles").join(format!("{:016x}.bin", fnv(&[renderer, vertex, fragment]))))
}

fn format_index(format: ColorFormat) -> i32 {
    match format {
        ColorFormat::Rgba8 => 0,
        ColorFormat::Rgb8 => 1,
        ColorFormat::Rgb565 => 2,
        ColorFormat::Rgb5A1 => 3,
        ColorFormat::Rgba4 => 4,
    }
}

fn pixel_offset(tiled: bool, x: u32, y: u32, width: u32, bpp: u32) -> usize {
    if tiled {
        morton_offset(x, y, width, bpp) as usize
    } else {
        ((y * width + x) * bpp) as usize
    }
}

/// the rows first..last of a picture read back from GL, bottom up in RGBA8,
/// stored in a surface's own layout as encode makes each pixel. a tiled row
/// goes eight pixels at a time, one row of a tile, their places in it from a
/// table.
fn store_rows<const BPP: usize>(data: &[u8], out: &mut [u8], width: u32, height: u32, tiled: bool, rows: (u32, u32), encode: impl Fn([u8; 4]) -> [u8; BPP]) {
    // x's bits spread to the even places of a tile's Morton order
    const SPREAD: [usize; 8] = [0, 1, 4, 5, 16, 17, 20, 21];
    let width = width as usize;
    for y in rows.0..rows.1 {
        let source = &data[(height - 1 - y) as usize * width * 4..][..width * 4];
        if tiled && width.is_multiple_of(8) {
            let row_base = y as usize / 8 * width * 8 + (SPREAD[y as usize & 7] << 1);
            for (tile, pixels) in source.as_chunks::<32>().0.iter().enumerate() {
                let tile_base = row_base + tile * 64;
                for (i, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
                    let at = (tile_base + SPREAD[i]) * BPP;
                    out[at..at + BPP].copy_from_slice(&encode(*pixel));
                }
            }
        } else {
            for (x, pixel) in source.as_chunks::<4>().0.iter().enumerate() {
                let at = pixel_offset(tiled, x as u32, y, width as u32, BPP as u32);
                out[at..at + BPP].copy_from_slice(&encode(*pixel));
            }
        }
    }
}

const LIGHTING_BASE: usize = 68;

/// what a draw gives back while the program it needs is still compiling, the
/// vertices then shaded on the CPU.
pub(crate) const COMPILING: &str = "its program is compiling";

/// the generic fragment program's constants, which draws whatever the
/// combination through uniforms.
fn generic_constants(constants: &[u32; FRAGMENT_CONSTANTS]) -> [u32; FRAGMENT_CONSTANTS] {
    let mut generic = [0u32; FRAGMENT_CONSTANTS];
    generic[25] = constants[25] & 0x400;
    generic[26] = constants[26];
    generic[28] = constants[28] & WRITES_DEPTH;
    generic[29] = 1;
    generic
}

fn lighting_shape(words: &[u32]) -> Vec<u32> {
    let w = &words[LIGHTING_BASE..];
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

fn lighting_source(shape: &[u32]) -> String {
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
    out += "    vec3 view = in_view;\n";
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
fn plain_multiplies(source: &str) -> String {
    const LARGEST: &str = "-3.4028235e38, 3.4028235e38";
    source
        .replace("    return mix(product, mix(mix(0.0, product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("    return mix(product, mix(mix(vec4(0.0), product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("vec4(1.0 / a.x);", &format!("vec4(clamp(1.0 / a.x, {LARGEST}));"))
        .replace("vec4(1.0 / sqrt(a.x));", &format!("vec4(clamp(1.0 / sqrt(a.x), {LARGEST}));"))
        .replace("vec4(log2(a.x));", &format!("vec4(clamp(log2(a.x), {LARGEST}));"))
        .replace("vec4(exp2(a.x));", &format!("vec4(clamp(exp2(a.x), {LARGEST}));"))
}

fn fragment_source(constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32]) -> String {
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
            ("in vec3 in_view", "in highp vec3 in_view"),
            ("in float in_depth_w", "in highp float in_depth_w"),
            ("in float in_w", "in highp float in_w"),
            ("sample_unit(uint unit, vec2 uv)", "sample_unit(uint unit, highp vec2 uv)"),
            ("    vec2 st = ", "    highp vec2 st = "),
            ("    float depth = in_depth", "    highp float depth = in_depth"),
            ("    vec3 view = in_view", "    highp vec3 view = in_view"),
            ("vec3 offset = -view", "highp vec3 offset = -view"),
        ] {
            out = out.replace(from, to);
        }
    }
    out
}


const PRESENT_SOURCE: &str = r#"#version 310 es
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

#[repr(C)]
struct DrmModeRes {
    count_fbs: i32,
    fbs: *mut u32,
    count_crtcs: i32,
    crtcs: *mut u32,
    count_connectors: i32,
    connectors: *mut u32,
    count_encoders: i32,
    encoders: *mut u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    kind: u32,
    name: [u8; 32],
}

#[repr(C)]
struct DrmModeConnector {
    connector_id: u32,
    encoder_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    count_modes: i32,
    modes: *mut DrmModeInfo,
    count_props: i32,
    props: *mut u32,
    prop_values: *mut u64,
    count_encoders: i32,
    encoders: *mut u32,
}

#[repr(C)]
struct DrmModeEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

type FlipHandler = extern "C" fn(i32, u32, u32, u32, *mut c_void);

#[repr(C)]
struct DrmEventContext {
    version: i32,
    vblank_handler: Option<FlipHandler>,
    page_flip_handler: Option<FlipHandler>,
}

/// the panel's refresh count and the time of it, in microseconds of the
/// monotonic clock, at the last flip.
static FLIPPED_AT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static FLIPPED_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

extern "C" fn flipped(_fd: i32, sequence: u32, seconds: u32, microseconds: u32, _data: *mut c_void) {
    FLIPPED_AT.store(sequence, std::sync::atomic::Ordering::Relaxed);
    FLIPPED_TIME.store(seconds as u64 * 1_000_000 + microseconds as u64, std::sync::atomic::Ordering::Relaxed);
}

/// a native window made by the program around the engine, an Android
/// activity's, that the screens go to in place of the panel, null for none.
static WINDOW: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// whether the window is both of a dual-screen handheld's panels stacked,
/// as GammaOS's DualStack gives an app them, its top half on the top panel
/// and its bottom half on the bottom one.
static STACKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// says the window set_window gives is two panels stacked, before the
/// renderer starts: it is never turned as a portrait panel would be, and the
/// menu goes on the top half.
pub fn set_stacked(stacked: bool) {
    STACKED.store(stacked, std::sync::atomic::Ordering::Relaxed);
}

/// whether the window is two panels stacked.
pub fn stacked() -> bool {
    STACKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// a second native window, on a second display, that one of the screens
/// goes to by itself, null for none, and the one the GL thread has a
/// surface on, which set_second_window waits to see go before its window
/// may go away.
static SECOND_WINDOW: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static SECOND_IN_USE: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
/// whether the second window shows the top screen rather than the bottom.
static SECOND_SHOWS_TOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// whether the second window's screen is stretched over all of it.
static SECOND_STRETCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// whether the second window's screen fills all of it, out of shape, rather
/// than keeping its shape with bars.
pub fn set_second_stretched(stretched: bool) {
    SECOND_STRETCHED.store(stretched, std::sync::atomic::Ordering::Relaxed);
}

/// hands the renderer a second window, on a second display, for one of the
/// screens by itself, or takes it back with null. Taking it back waits, a
/// half second at most, for the GL thread to stop drawing into it, after
/// which the caller may let the window go.
pub fn set_second_window(window: *mut c_void) {
    SECOND_WINDOW.store(window, std::sync::atomic::Ordering::Release);
    if window.is_null() {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !SECOND_IN_USE.load(std::sync::atomic::Ordering::Acquire).is_null() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// whether there is a second window for a screen to go to.
pub fn second_window() -> bool {
    !SECOND_WINDOW.load(std::sync::atomic::Ordering::Acquire).is_null()
}

/// which screen the second window shows, the top one or the bottom one.
pub fn set_second_shows_top(top: bool) {
    SECOND_SHOWS_TOP.store(top, std::sync::atomic::Ordering::Relaxed);
}

/// hands the renderer a native window to show the screens in, an
/// ANativeWindow on Android, before the title is loaded. The window has to
/// stay valid as long as the renderer runs.
pub fn set_window(window: *mut c_void) {
    WINDOW.store(window, std::sync::atomic::Ordering::Release);
}

/// the panel's refresh period, and how long before its refresh the last
/// picture was handed over, in nanoseconds, 0 when not known.
static PANEL_PERIOD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PANEL_SLACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// the panel's refresh period, and how long before its refresh the last
/// picture was handed over, when one was since the last call.
pub fn panel_timing() -> Option<(std::time::Duration, Option<std::time::Duration>)> {
    let period = PANEL_PERIOD.load(std::sync::atomic::Ordering::Relaxed);
    let slack = PANEL_SLACK.swap(0, std::sync::atomic::Ordering::Relaxed);
    (period > 0).then(|| (std::time::Duration::from_nanos(period), (slack > 0).then(|| std::time::Duration::from_nanos(slack))))
}

fn monotonic_micros() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes the timespec it is given
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1000
}

const GBM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// the panel, driven through KMS from a GBM surface the context draws into.
struct Kms {
    drm: libloading::Library,
    gbm: libloading::Library,
    fd: i32,
    device: *mut c_void,
    surface: *mut c_void,
    crtc: u32,
    connector: u32,
    mode: DrmModeInfo,
    shown: Option<(*mut c_void, u32)>,
    pending: Option<(*mut c_void, u32)>,
    set: bool,
    /// how many pictures stayed up for 1, 2, 3 and 4 or more panel
    /// refreshes, logged every few seconds: at an even frame rate they all
    /// stay up as long.
    held: [u32; 4],
    held_since: std::time::Instant,
    last_flip: u32,
    /// when the flip still queued was asked for, in microseconds of the
    /// monotonic clock, and the time from asking to flipping summed and
    /// counted since the last log.
    asked: u64,
    slack: (u64, u32),
}

macro_rules! call {
    ($library:expr, $name:literal, fn($($arg:ty),*) -> $ret:ty, $($value:expr),*) => {{
        let address = symbol(&$library, $name);
        if address.is_null() {
            return Err(format!("no {}", $name));
        }
        let function: extern "C" fn($($arg),*) -> $ret = std::mem::transmute(address);
        function($($value),*)
    }};
}

impl Kms {
    /// SAFETY: plain libdrm and GBM calls on the descriptor and device this
    /// opens, with the structs they return read before they are freed
    unsafe fn open() -> Result<Kms, String> {
        let drm = open(&["libdrm.so.2", "libdrm.so"]).map_err(|why| format!("could not load libdrm, {why}"))?;
        let gbm = open(&["libgbm.so.1", "libgbm.so"]).map_err(|why| format!("could not load libgbm, {why}"))?;
        let path = CString::new("/dev/dri/card0").map_err(|error| error.to_string())?;
        let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            return Err("could not open /dev/dri/card0".into());
        }
        let device = call!(gbm, "gbm_create_device", fn(i32) -> *mut c_void, fd);
        if device.is_null() {
            return Err("no GBM device".into());
        }
        let resources = call!(drm, "drmModeGetResources", fn(i32) -> *mut DrmModeRes, fd);
        if resources.is_null() {
            return Err("no KMS resources".into());
        }
        let res = &*resources;
        let mut found = None;
        for i in 0..res.count_connectors as usize {
            let connector = call!(drm, "drmModeGetConnector", fn(i32, u32) -> *mut DrmModeConnector, fd, *res.connectors.add(i));
            if connector.is_null() {
                continue;
            }
            let c = &*connector;
            if c.connection == 1 && c.count_modes > 0 {
                let modes = std::slice::from_raw_parts(c.modes, c.count_modes as usize);
                let mode = *modes.iter().find(|mode| mode.kind & (1 << 3) != 0).unwrap_or(&modes[0]);
                let mut crtc = 0;
                let encoder = call!(drm, "drmModeGetEncoder", fn(i32, u32) -> *mut DrmModeEncoder, fd, c.encoder_id);
                if !encoder.is_null() {
                    crtc = (*encoder).crtc_id;
                    call!(drm, "drmModeFreeEncoder", fn(*mut DrmModeEncoder) -> (), encoder);
                }
                if crtc == 0 && res.count_crtcs > 0 {
                    crtc = *res.crtcs;
                }
                found = Some((c.connector_id, mode, crtc));
            }
            call!(drm, "drmModeFreeConnector", fn(*mut DrmModeConnector) -> (), connector);
            if found.is_some() {
                break;
            }
        }
        call!(drm, "drmModeFreeResources", fn(*mut DrmModeRes) -> (), resources);
        let (connector, mode, crtc) = found.ok_or("no connected display")?;
        let refresh = mode.clock as f64 * 1000.0 / (mode.htotal as f64 * mode.vtotal as f64);
        log::info!("panel {}x{} at {refresh:.3} Hz", mode.hdisplay, mode.vdisplay);
        if (50.0..70.0).contains(&refresh) {
            PANEL_PERIOD.store((1e9 / refresh) as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let surface = call!(
            gbm,
            "gbm_surface_create",
            fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void,
            device,
            mode.hdisplay as u32,
            mode.vdisplay as u32,
            GBM_FORMAT_XRGB8888,
            1 | 4
        );
        if surface.is_null() {
            return Err("no GBM surface".into());
        }
        Ok(Kms { drm, gbm, fd, device, surface, crtc, connector, mode, shown: None, pending: None, set: false, held: [0; 4], held_since: std::time::Instant::now(), last_flip: 0, asked: 0, slack: (0, 0) })
    }

    fn size(&self) -> (i32, i32) {
        (self.mode.hdisplay as i32, self.mode.vdisplay as i32)
    }

    /// waits for the flip queued last to happen, then lets go of the
    /// buffer it replaced.
    fn settle(&mut self) -> Result<(), String> {
        let Some(pending) = self.pending.take() else { return Ok(()) };
        // SAFETY: as in open, on the descriptor and buffers this holds
        unsafe {
            let mut context = DrmEventContext { version: 2, vblank_handler: None, page_flip_handler: Some(flipped) };
            let mut poll = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
            if libc::poll(&mut poll, 1, 100) > 0 {
                call!(self.drm, "drmHandleEvent", fn(i32, *mut DrmEventContext) -> i32, self.fd, &mut context);
                let at = FLIPPED_AT.load(std::sync::atomic::Ordering::Relaxed);
                let slack = FLIPPED_TIME.load(std::sync::atomic::Ordering::Relaxed).wrapping_sub(self.asked);
                if slack < 100_000 {
                    PANEL_SLACK.store(slack * 1000, std::sync::atomic::Ordering::Relaxed);
                    self.slack = (self.slack.0 + slack, self.slack.1 + 1);
                }
                let refreshes = at.wrapping_sub(self.last_flip) as usize;
                if self.last_flip != 0 && refreshes > 0 {
                    self.held[refreshes.min(4) - 1] += 1;
                }
                self.last_flip = at;
                if self.held_since.elapsed() >= std::time::Duration::from_secs(5) {
                    let [one, two, three, more] = std::mem::take(&mut self.held);
                    let (sum, count) = std::mem::take(&mut self.slack);
                    log::info!("pictures up for 1/2/3/4+ panel refreshes: {one}/{two}/{three}/{more}, handed over {:.1} ms before the refresh", sum as f64 / count.max(1) as f64 / 1000.0);
                    self.held_since = std::time::Instant::now();
                }
            }
            if let Some((bo, fb)) = self.shown.replace(pending) {
                call!(self.drm, "drmModeRmFB", fn(i32, u32) -> i32, self.fd, fb);
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
            }
        }
        Ok(())
    }

    /// queues what the context just swapped to for the panel's next
    /// vertical blank, waiting only for a flip still queued from before.
    fn flip(&mut self) -> Result<(), String> {
        self.settle()?;
        // SAFETY: as in open, the buffer object is the surface's front one
        unsafe {
            let bo = call!(self.gbm, "gbm_surface_lock_front_buffer", fn(*mut c_void) -> *mut c_void, self.surface);
            if bo.is_null() {
                return Err("no front buffer".into());
            }
            let handle = call!(self.gbm, "gbm_bo_get_handle", fn(*mut c_void) -> u64, bo) as u32;
            let stride = call!(self.gbm, "gbm_bo_get_stride", fn(*mut c_void) -> u32, bo);
            let (width, height) = self.size();
            let mut fb = 0u32;
            let added = call!(
                self.drm,
                "drmModeAddFB",
                fn(i32, u32, u32, u8, u8, u32, u32, *mut u32) -> i32,
                self.fd,
                width as u32,
                height as u32,
                24,
                32,
                stride,
                handle,
                &mut fb
            );
            if added != 0 {
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
                return Err("could not make a framebuffer".into());
            }
            if !self.set {
                let mut connector = self.connector;
                let mut mode = self.mode;
                let result = call!(
                    self.drm,
                    "drmModeSetCrtc",
                    fn(i32, u32, u32, u32, u32, *mut u32, i32, *mut DrmModeInfo) -> i32,
                    self.fd,
                    self.crtc,
                    fb,
                    0,
                    0,
                    &mut connector,
                    1,
                    &mut mode
                );
                if result != 0 {
                    return Err(format!("could not set the display mode, {result}"));
                }
                self.set = true;
                self.shown = Some((bo, fb));
            } else if call!(self.drm, "drmModePageFlip", fn(i32, u32, u32, u32, *mut c_void) -> i32, self.fd, self.crtc, fb, 1, std::ptr::null_mut()) == 0 {
                self.asked = monotonic_micros();
                self.pending = Some((bo, fb));
            } else {
                call!(self.drm, "drmModeRmFB", fn(i32, u32) -> i32, self.fd, fb);
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
            }
        }
        Ok(())
    }
}

/// the size the frontend lays the screens out in, the panel's shape with
/// its long side across, 480 tall or 640 wide: 640x480 on a 4:3 panel,
/// 853x480 on a 16:9 one, 640x640 on a square one.
static LAYOUT: std::sync::OnceLock<(i32, i32)> = std::sync::OnceLock::new();

pub fn layout_size() -> (u32, u32) {
    let (width, height) = *LAYOUT.get().unwrap_or(&(640, 480));
    (width as u32, height as u32)
}

fn layout_for((width, height): (i32, i32)) -> (i32, i32) {
    if width * 3 >= height * 4 {
        ((480 * width + height / 2) / height, 480)
    } else {
        (640, (640 * height + width / 2) / width)
    }
}

/// the panel's size and the way its picture is turned, none on a landscape
/// panel.
fn panel_shape(output: &Output) -> ((i32, i32), Option<i32>) {
    let size = match output {
        Output::Kms(kms) => kms.size(),
        Output::Fbdev(window) => (window[0] as i32, window[1] as i32),
        Output::Window(size) => *size,
        Output::Offscreen(_) => (640, 480),
    };
    let turn = match output {
        Output::Kms(_) | Output::Fbdev(_) | Output::Window(_) => panel_turn(size.0, size.1),
        Output::Offscreen(_) => None,
    };
    (size, turn)
}

/// the layout scaled to fit what it is drawn into and centred, the menu
/// made for 640x480 in the middle of it.
#[derive(Clone, Copy)]
struct Fit {
    scale: f32,
    left: f32,
    top: f32,
}

impl Fit {
    fn new((width, height): (i32, i32), (into_width, into_height): (i32, i32)) -> Fit {
        let scale = (into_width as f32 / width as f32).min(into_height as f32 / height as f32);
        Fit { scale, left: (into_width as f32 - width as f32 * scale) / 2.0, top: (into_height as f32 - height as f32 * scale) / 2.0 }
    }

    /// the same, from a point of the layout on.
    fn inset(self, x: i32, y: i32) -> Fit {
        Fit { left: self.left + x as f32 * self.scale, top: self.top + y as f32 * self.scale, ..self }
    }

    fn x(self, x: i32) -> i32 {
        (self.left + x as f32 * self.scale).round() as i32
    }

    fn y(self, y: i32) -> i32 {
        (self.top + y as f32 * self.scale).round() as i32
    }

    /// left, top, width and height.
    fn area(self, [left, top, width, height]: [i32; 4]) -> [i32; 4] {
        let (x, y) = (self.x(left), self.y(top));
        [x, y, self.x(left + width) - x, self.y(top + height) - y]
    }
}

enum Part {
    Cursor { x: i32, y: i32 },
    /// a screen's buffer, the top one's version when it is interpolated.
    Surface { texture: glow::Texture, screen: [i32; 4], area: [i32; 4], smooth: bool, top: bool, picture: Option<u64> },
    Pixels { index: usize, pixels: Vec<u8>, width: u32, height: u32, area: [i32; 4], smooth: bool },
    Overlay(Arc<crate::Overlay>),
}

const OVERLAY_VERTEX_SOURCE: &str = r#"#version 310 es
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

const OVERLAY_FRAGMENT_SOURCE: &str = r#"#version 310 es
precision mediump float;
uniform sampler2D source;
in vec2 at;
in vec4 tint;
layout(location = 0) out vec4 color;
void main() {
    color = texture(source, at) * tint;
}
"#;

/// draws a user interface over the screens, its textures made the first time
/// they come and kept by their id.
/// SAFETY: on the GL thread, with the panel's framebuffer bound
unsafe fn draw_overlay(gl: &glow::Context, panel: &mut Panel, overlay: &crate::Overlay, height: i32, fit: Fit) -> Result<(), String> {
    let (program, array, vertices, indices) = match panel.overlay {
        Some(made) => made,
        None => {
            let program = link(gl, OVERLAY_VERTEX_SOURCE, OVERLAY_FRAGMENT_SOURCE)?;
            let (array, vertices, indices) = (gl.create_vertex_array()?, gl.create_buffer()?, gl.create_buffer()?);
            gl.bind_vertex_array(Some(array));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(indices));
            let stride = std::mem::size_of::<crate::OverlayVertex>() as i32;
            for (slot, count, kind, normalized, offset) in [(0, 2, glow::FLOAT, false, 0), (1, 2, glow::FLOAT, false, 8), (2, 4, glow::UNSIGNED_BYTE, true, 16)] {
                gl.enable_vertex_attrib_array(slot);
                gl.vertex_attrib_pointer_f32(slot, count, kind, normalized, stride, offset);
            }
            panel.overlay = Some((program, array, vertices, indices));
            (program, array, vertices, indices)
        }
    };
    for texture in &overlay.textures {
        let made = match panel.overlay_textures.get(&texture.id) {
            Some(&(made, size)) if texture.offset.is_some() || size == texture.size => {
                if texture.offset.is_none() {
                    continue;
                }
                made
            }
            old => {
                if let Some(&(old, _)) = old {
                    gl.delete_texture(old);
                }
                let made = gl.create_texture()?;
                gl.bind_texture(glow::TEXTURE_2D, Some(made));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, texture.size[0] as i32, texture.size[1] as i32);
                let filter = if texture.linear { glow::LINEAR } else { glow::NEAREST } as i32;
                for (parameter, value) in [(glow::TEXTURE_MIN_FILTER, filter), (glow::TEXTURE_MAG_FILTER, filter), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32)] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value);
                }
                panel.overlay_textures.insert(texture.id, (made, texture.size));
                made
            }
        };
        let [x, y] = texture.offset.unwrap_or_default();
        gl.bind_texture(glow::TEXTURE_2D, Some(made));
        let rows = texture.pixels.len() as u32 / 4 / texture.size[0].max(1);
        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, x as i32, y as i32, texture.size[0] as i32, rows as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&texture.pixels)));
    }
    for id in &overlay.free {
        if let Some((texture, _)) = panel.overlay_textures.remove(id) {
            gl.delete_texture(texture);
        }
    }
    let [x, y, width, tall] = fit.area([0, 0, 640, 480]);
    gl.viewport(x, height - y - tall, width, tall);
    gl.use_program(Some(program));
    gl.uniform_2_f32(gl.get_uniform_location(program, "size").as_ref(), 640.0, 480.0);
    gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
    gl.bind_vertex_array(Some(array));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
    gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(indices));
    gl.active_texture(glow::TEXTURE0);
    gl.bind_sampler(0, None);
    gl.enable(glow::BLEND);
    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
    gl.enable(glow::SCISSOR_TEST);
    for mesh in &overlay.meshes {
        let Some(&(texture, _)) = panel.overlay_textures.get(&mesh.texture) else { continue };
        let [left, top, right, bottom] = mesh.clip.map(|edge| edge as i32);
        let [left, top, right, bottom] = [fit.x(left), fit.y(top), fit.x(right), fit.y(bottom)];
        gl.scissor(left, height - bottom, right - left, bottom - top);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let bytes = std::slice::from_raw_parts(mesh.vertices.as_ptr().cast::<u8>(), std::mem::size_of_val(mesh.vertices.as_slice()));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
        let index_bytes = std::slice::from_raw_parts(mesh.indices.as_ptr().cast::<u8>(), std::mem::size_of_val(mesh.indices.as_slice()));
        gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, index_bytes, glow::STREAM_DRAW);
        gl.draw_elements(glow::TRIANGLES, mesh.indices.len() as i32, glow::UNSIGNED_INT, 0);
    }
    gl.disable(glow::SCISSOR_TEST);
    gl.disable(glow::BLEND);
    Ok(())
}

/// held while the GL thread or the compiler thread compiles: Mali's driver
/// on the RX6H crashed in the compiler thread with both at it.
static COMPILER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn link(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
    let _compiling = COMPILER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: on the GL thread, the shaders are deleted once linked
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (stage, source) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let shader = gl.create_shader(stage)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(format!("could not compile a shader, {}", gl.get_shader_info_log(shader)));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.link_program(program);
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !gl.get_program_link_status(program) {
            return Err(format!("could not link a program, {}", gl.get_program_info_log(program)));
        }
        Ok(program)
    }
}

const ROTATE_SOURCE: &str = r#"#version 310 es
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

/// a landscape canvas the screens are laid out in and drawn into, which is
/// turned a quarter onto a portrait panel and fitted with black bars.
#[derive(Clone, Copy)]
struct Canvas {
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    program: glow::Program,
    width: i32,
    height: i32,
}

/// which way to turn the canvas onto the panel, 0 clockwise, 1 counter
/// clockwise, none when the panel is landscape. ZAKURO_PANEL_ROTATE=0 turns
/// nothing, 90 turns clockwise, 270 counter clockwise, and unset turns
/// clockwise on a portrait panel.
fn panel_turn(width: i32, height: i32) -> Option<i32> {
    // two landscape panels stacked are taller than wide, and are not turned
    if stacked() {
        return None;
    }
    static CHOICE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let choice = CHOICE.get_or_init(|| std::env::var("ZAKURO_PANEL_ROTATE").ok());
    match choice.as_deref() {
        Some("0") | Some("off") => None,
        Some("90") => Some(0),
        Some("270") => Some(1),
        _ => (height > width).then_some(0),
    }
}

/// the surface on the second window set_second_window gave, made, remade
/// or let go as the window came, changed or went, and its size.
fn second_surface(panel: &mut Panel) -> Option<(egl::types::EGLSurface, (i32, i32))> {
    let wanted = SECOND_WINDOW.load(std::sync::atomic::Ordering::Acquire);
    let egl = panel.egl;
    if panel.second.is_some_and(|(_, window, _)| window != wanted) {
        let (surface, ..) = panel.second.take().unwrap();
        // SAFETY: a surface this made on the display, not current
        unsafe { egl.egl.DestroySurface(egl.display, surface) };
        SECOND_IN_USE.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        log::info!("the second window went");
    }
    if panel.second.is_none() && !wanted.is_null() {
        // SAFETY: a window set_second_window keeps valid until it takes it
        // back, on the display and config the panel's context has
        unsafe {
            let surface = egl.egl.CreateWindowSurface(egl.display, egl.config, wanted as glutin_egl_sys::EGLNativeWindowType, std::ptr::null());
            if surface.is_null() {
                log::warn!("could not make a surface on the second window, 0x{:X}", egl.egl.GetError());
                return None;
            }
            let (mut width, mut height) = (0, 0);
            egl.egl.QuerySurface(egl.display, surface, egl::WIDTH as i32, &mut width);
            egl.egl.QuerySurface(egl.display, surface, egl::HEIGHT as i32, &mut height);
            // the panel's swap waits for its refresh already, this one need not
            egl.egl.MakeCurrent(egl.display, surface, surface, egl.context);
            egl.egl.SwapInterval(egl.display, 0);
            egl.egl.MakeCurrent(egl.display, egl.surface, egl.surface, egl.context);
            SECOND_IN_USE.store(wanted, std::sync::atomic::Ordering::Release);
            log::info!("a screen goes to a {width}x{height} second window");
            panel.second = Some((surface, wanted, (width, height)));
        }
    }
    panel.second.map(|(surface, _, size)| (surface, size))
}

/// draws the second window's screen filling it, with the cursor over it,
/// swaps it, and makes the panel's surface current again.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_second(
    gl: &glow::Context,
    panel: &mut Panel,
    program: glow::Program,
    linear: glow::Sampler,
    empty: glow::VertexArray,
    surface: egl::types::EGLSurface,
    (width, height): (i32, i32),
    parts: Vec<Part>,
) -> Result<(), String> {
    let egl = panel.egl;
    if egl.egl.MakeCurrent(egl.display, surface, surface, egl.context) == 0 {
        return Err(format!("could not make the second window current, 0x{:X}", egl.egl.GetError()));
    }
    // the screen's own size is the layout here, its area all of it
    let mut layout = parts
        .iter()
        .find_map(|part| match part {
            Part::Surface { area, .. } | Part::Pixels { area, .. } => Some((area[2].max(1), area[3].max(1))),
            _ => None,
        })
        .unwrap_or((320, 240));
    let mut parts = parts;
    if SECOND_STRETCHED.load(std::sync::atomic::Ordering::Relaxed) {
        // stretched, the window's own size is the layout, the screen all of
        // it and the cursor moved along
        let (across, down) = (width as f32 / layout.0 as f32, height as f32 / layout.1 as f32);
        for part in &mut parts {
            match part {
                Part::Surface { area, .. } | Part::Pixels { area, .. } => *area = [0, 0, width, height],
                Part::Cursor { x, y } => (*x, *y) = ((*x as f32 * across) as i32, (*y as f32 * down) as i32),
                Part::Overlay(_) => {}
            }
        }
        layout = (width, height);
    }
    let fit = Fit::new(layout, (width, height));
    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    gl.viewport(0, 0, width, height);
    gl.disable(glow::SCISSOR_TEST);
    gl.disable(glow::BLEND);
    gl.color_mask(true, true, true, true);
    gl.clear_color(0.0, 0.0, 0.0, 1.0);
    gl.clear(glow::COLOR_BUFFER_BIT);
    gl.use_program(Some(program));
    gl.bind_vertex_array(Some(empty));
    gl.active_texture(glow::TEXTURE0);
    gl.bind_sampler(0, None);
    gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
    let drawn = draw_parts(gl, panel, program, linear, None, height, fit, layout, parts);
    let swapped = egl.egl.SwapBuffers(egl.display, surface) != 0;
    egl.egl.MakeCurrent(egl.display, egl.surface, egl.surface, egl.context);
    drawn?;
    if !swapped {
        log::warn!("could not swap the second window's buffers, 0x{:X}", egl.egl.GetError());
    }
    Ok(())
}

/// draws each part into the framebuffer bound, height pixels high, the
/// layout fitted into it as fit says.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_parts(
    gl: &glow::Context,
    panel: &mut Panel,
    program: glow::Program,
    linear: glow::Sampler,
    framebuffer: Option<glow::Framebuffer>,
    height: i32,
    fit: Fit,
    layout: (i32, i32),
    parts: Vec<Part>,
) -> Result<(), String> {
            for part in parts {
                let (texture, screen, upright, area, smooth) = match part {
                    Part::Cursor { x, y } => {
                        let (x, y) = (fit.x(x), fit.y(y));
                        gl.enable(glow::SCISSOR_TEST);
                        for (size, shade) in [(7, 0.0), (5, 1.0)].map(|(size, shade)| ((size as f32 * fit.scale) as i32 | 1, shade)) {
                            gl.scissor(x - size / 2, height - y - size / 2, size, size);
                            gl.clear_color(shade, shade, shade, 1.0);
                            gl.clear(glow::COLOR_BUFFER_BIT);
                        }
                        gl.disable(glow::SCISSOR_TEST);
                        continue;
                    }
                    Part::Surface { texture, screen, area, smooth, top, picture } => {
                        let area = fit.area(area);
                        let step = match picture {
                            Some(version) => panel.between.step((texture, version)),
                            None => {
                                if top {
                                    panel.between.reset();
                                }
                                Step::Plain { keep: false }
                            }
                        };
                        let keep = match step {
                            Step::Plain { keep } => keep.then_some(0),
                            Step::Halfway => Some(1),
                        };
                        if let Some(slot) = keep {
                            panel.between.keep(gl, program, texture, screen, slot)?;
                            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
                        }
                        if let Step::Halfway = step {
                            let gl_area = [area[0], height - area[1] - area[3], area[2], area[3]];
                            panel.between.draw(gl, framebuffer, gl_area, smooth.then_some(linear))?;
                            gl.use_program(Some(program));
                            continue;
                        }
                        gl.use_program(Some(program));
                        (texture, screen, 0, area, smooth)
                    }
                    Part::Overlay(overlay) => {
                        // the menu in the middle, or on the top panel of two stacked
                        let down = if stacked() { 0 } else { (layout.1 - 480) / 2 };
                        draw_overlay(gl, panel, &overlay, height, fit.inset((layout.0 - 640) / 2, down))?;
                        continue;
                    }
                    Part::Pixels { index, pixels, width, height, area, smooth } => {
                        if index == 0 {
                            panel.between.reset();
                        }
                        let texture = match panel.textures[index] {
                            Some((texture, w, h)) if (w, h) == (width, height) => texture,
                            made => {
                                if let Some((old, ..)) = made {
                                    gl.delete_texture(old);
                                }
                                let texture = gl.create_texture()?;
                                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width as i32, height as i32);
                                panel.textures[index] = Some((texture, width, height));
                                texture
                            }
                        };
                        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width as i32, height as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&pixels)));
                        (texture, [width as i32, height as i32, 0, 0], 1, fit.area(area), smooth)
                    }
                };
                let y = height - area[1] - area[3];
                gl.viewport(area[0], y, area[2], area[3]);
                gl.uniform_4_i32(gl.get_uniform_location(program, "area").as_ref(), area[0], y, area[2], area[3]);
                gl.uniform_4_i32(gl.get_uniform_location(program, "screen").as_ref(), screen[0], screen[1], screen[2], screen[3]);
                gl.uniform_1_i32(gl.get_uniform_location(program, "upright").as_ref(), upright);
                gl.uniform_1_i32(gl.get_uniform_location(program, "smooth_scale").as_ref(), smooth as i32);
                gl.bind_sampler(0, smooth.then_some(linear));
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
    Ok(())
}

fn show(gl: GlRef, parts: Vec<Part>) -> Result<(), String> {
    disturb();
    PANEL.with(|panel| {
        let mut panel = panel.borrow_mut();
        let Some(panel) = panel.as_mut() else { return Ok(()) };
        let program = match panel.program {
            Some(program) => program,
            None => {
                let program = link(&gl, PACK_VERTEX_SOURCE, PRESENT_SOURCE)?;
                panel.program = Some(program);
                program
            }
        };
        // SAFETY: on the GL thread, the window surface is current and every
        // texture read is the size the uniforms say
        unsafe {
            let empty = match panel.empty {
                Some(empty) => empty,
                None => {
                    let empty = gl.create_vertex_array()?;
                    panel.empty = Some(empty);
                    empty
                }
            };
            let ((panel_width, panel_height), turn) = panel_shape(&panel.output);
            let panel_framebuffer = match &panel.output {
                Output::Offscreen(framebuffer) => Some(*framebuffer),
                _ => None,
            };
            // a portrait panel under a landscape layout: everything is drawn
            // into a canvas the panel's size on its side, which is turned
            // onto the panel at the end
            let (width, height, framebuffer) = match turn {
                Some(_) => {
                    let (canvas_width, canvas_height) = (panel_height, panel_width);
                    let canvas = match panel.canvas {
                        Some(canvas) if (canvas.width, canvas.height) == (canvas_width, canvas_height) => canvas,
                        _ => {
                            let texture = gl.create_texture()?;
                            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, canvas_width, canvas_height);
                            let framebuffer = gl.create_framebuffer()?;
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
                            let program = link(&gl, PACK_VERTEX_SOURCE, ROTATE_SOURCE)?;
                            let made = Canvas { texture, framebuffer, program, width: canvas_width, height: canvas_height };
                            panel.canvas = Some(made);
                            log::info!("drawing a {canvas_width}x{canvas_height} picture turned onto the {panel_width}x{panel_height} panel");
                            made
                        }
                    };
                    (canvas_width, canvas_height, Some(canvas.framebuffer))
                }
                None => (panel_width, panel_height, panel_framebuffer),
            };
            let layout = *LAYOUT.get().unwrap_or(&(640, 480));
            let fit = Fit::new(layout, (width, height));
            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
            gl.viewport(0, 0, width, height);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::CULL_FACE);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(program));
            let linear = match panel.linear {
                Some(linear) => linear,
                None => {
                    let linear = gl.create_sampler()?;
                    for (parameter, value) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
                        gl.sampler_parameter_i32(linear, parameter, value as i32);
                    }
                    panel.linear = Some(linear);
                    linear
                }
            };
            gl.bind_vertex_array(Some(empty));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_sampler(0, None);
            gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
            // the screen the second window shows, and the cursor when that is
            // the bottom one, go there; the rest to the panel
            let second = second_surface(panel);
            let (parts, second_parts): (Vec<Part>, Vec<Part>) = match second {
                Some(_) => {
                    let top = SECOND_SHOWS_TOP.load(std::sync::atomic::Ordering::Relaxed);
                    parts.into_iter().partition(|part| match part {
                        Part::Surface { top: is_top, .. } => *is_top != top,
                        Part::Pixels { index, .. } => (*index == 0) != top,
                        Part::Cursor { .. } => top,
                        Part::Overlay(_) => true,
                    })
                }
                None => (parts, Vec::new()),
            };
            draw_parts(&gl, panel, program, linear, framebuffer, height, fit, layout, parts)?;
            if let Output::Offscreen(_) = panel.output {
                if let Some(path) = std::env::var_os("ZAKURO_PANEL_SHOT") {
                    let mut data = vec![0u8; (width * height * 4) as usize];
                    gl.read_pixels(0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
                    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
                    for row in data.chunks(width as usize * 4).rev() {
                        ppm.extend(row.chunks(4).flat_map(|pixel| &pixel[..3]));
                    }
                    // a % in the name numbers each picture rather than keeping the last
                    static SHOTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let path = path.to_string_lossy().replace('%', &format!("{:05}", SHOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
                    let _ = std::fs::write(path, ppm);
                }
                return Ok(());
            }
            if let (Some(turn), Some(canvas)) = (turn, panel.canvas) {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, panel_width, panel_height);
                gl.disable(glow::SCISSOR_TEST);
                gl.disable(glow::BLEND);
                gl.color_mask(true, true, true, true);
                gl.clear_color(0.0, 0.0, 0.0, 1.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                // the canvas on its side, fitted to the panel and centred
                let (turned_width, turned_height) = (canvas.height as f32, canvas.width as f32);
                let scale = (panel_width as f32 / turned_width).min(panel_height as f32 / turned_height);
                let (rect_width, rect_height) = (turned_width * scale, turned_height * scale);
                let (left, bottom) = ((panel_width as f32 - rect_width) / 2.0, (panel_height as f32 - rect_height) / 2.0);
                gl.use_program(Some(canvas.program));
                gl.uniform_1_i32(gl.get_uniform_location(canvas.program, "canvas").as_ref(), 0);
                gl.uniform_1_i32(gl.get_uniform_location(canvas.program, "turn").as_ref(), turn);
                gl.uniform_4_f32(gl.get_uniform_location(canvas.program, "rect").as_ref(), left, bottom, rect_width, rect_height);
                gl.bind_vertex_array(Some(empty));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_sampler(0, Some(linear));
                gl.bind_texture(glow::TEXTURE_2D, Some(canvas.texture));
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            if panel.egl.egl.SwapBuffers(panel.egl.display, panel.egl.surface) == 0 {
                return Err("could not swap the panel's buffers".into());
            }
            if let Some((surface, size)) = second {
                draw_second(&gl, panel, program, linear, empty, surface, size, second_parts)?;
            }
        }
        match &mut panel.output {
            Output::Kms(kms) => kms.flip(),
            Output::Fbdev(_) | Output::Window(_) | Output::Offscreen(_) => Ok(()),
        }
    })
}

/// where the panel's picture goes: the handheld's screen, or for working on
/// a computer without one an image of its size, which ZAKURO_PANEL_SHOT=file
/// saves each time, numbered in place of a % in the name.
enum Output {
    Kms(Kms),
    /// the framebuffer device's size, the window a Mali driver built for
    /// it takes, kept where EGL was given it.
    Fbdev(Box<[u16; 2]>),
    /// a window set_window was given, its size.
    Window((i32, i32)),
    Offscreen(glow::Framebuffer),
}

struct Panel {
    egl: &'static Egl,
    output: Output,
    program: Option<glow::Program>,
    textures: [Option<(glow::Texture, u32, u32)>; 2],
    empty: Option<glow::VertexArray>,
    /// linear filtering for the screen drawn smooth.
    linear: Option<glow::Sampler>,
    /// the overlay's program, vertex array and buffers, and its textures.
    overlay: Option<(glow::Program, glow::VertexArray, glow::Buffer, glow::Buffer)>,
    overlay_textures: HashMap<u64, (glow::Texture, [u32; 2])>,
    between: Between,
    /// the landscape canvas a portrait panel's picture is drawn into first.
    canvas: Option<Canvas>,
    /// the surface on the second window, the window and its size.
    second: Option<(egl::types::EGLSurface, *mut c_void, (i32, i32))>,
}

thread_local! {
    static PANEL: std::cell::RefCell<Option<Panel>> = const { std::cell::RefCell::new(None) };
}

struct Egl {
    libraries: (libloading::Library, Option<libloading::Library>),
    egl: egl::Egl,
    display: egl::types::EGLDisplay,
    config: egl::types::EGLConfig,
    context: egl::types::EGLContext,
    surface: egl::types::EGLSurface,
}

// SAFETY: EGL's entry points can be called from any thread, and the handles
// here are only read once made
unsafe impl Send for Egl {}
unsafe impl Sync for Egl {}

/// glDrawRangeElementsBaseVertex, which glow does not wrap, as the loader
/// found it.
static DRAW_RANGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn remember(name: &str, address: *const c_void) -> *const c_void {
    if name == "glDrawRangeElementsBaseVertex" {
        DRAW_RANGE.store(address as usize, std::sync::atomic::Ordering::Relaxed);
    }
    address
}

fn symbol(library: &libloading::Library, name: &str) -> *const c_void {
    let Ok(name) = CString::new(name) else { return std::ptr::null() };
    // SAFETY: the symbol is only read as an address
    unsafe { library.get::<*const c_void>(name.as_bytes_with_nul()).map_or(std::ptr::null(), |symbol| *symbol) }
}

/// the first of the names that loads, else why the first did not.
fn open(names: &[&str]) -> Result<libloading::Library, String> {
    let mut first = None;
    for name in names {
        // SAFETY: loading the system's EGL and GLES libraries runs their
        // initializers, which is what using them takes
        match unsafe { libloading::Library::new(name) } {
            Ok(library) => return Ok(library),
            Err(why) => _ = first.get_or_insert(why.to_string()),
        }
    }
    Err(first.unwrap_or_default())
}

/// the EGL, GBM and Mali libraries this process loaded, and whether it can
/// use the Mali device, for telling why EGL would not start.
fn drawing_libraries() -> String {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut found: Vec<&str> = maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter(|path| {
            let file = path.rsplit('/').next().unwrap_or(path).to_lowercase();
            ["egl", "gbm", "gles", "mali"].iter().any(|name| file.contains(name))
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    // SAFETY: access only looks the path up
    let mali = unsafe { libc::access(c"/dev/mali0".as_ptr(), libc::R_OK | libc::W_OK) } == 0;
    format!("loaded {}, /dev/mali0 {}", found.join(" "), if mali { "usable" } else { "not usable" })
}

fn gbm_display(loaded: &egl::Egl) -> Option<egl::types::EGLDisplay> {
    let library = open(&["libgbm.so.1", "libgbm.so"]).ok()?;
    let create = symbol(&library, "gbm_create_device");
    if create.is_null() {
        return None;
    }
    // SAFETY: gbm_create_device takes a DRM file descriptor and returns a
    // device the process keeps for as long as it draws, so neither is closed
    unsafe {
        let create: extern "C" fn(i32) -> *mut c_void = std::mem::transmute(create);
        let found = ["/dev/dri/card0", "/dev/dri/renderD128"].iter().find_map(|path| {
            let path = CString::new(*path).ok()?;
            let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let device = create(fd);
            let display = if device.is_null() { std::ptr::null() } else { loaded.GetDisplay(device as glutin_egl_sys::EGLNativeDisplayType) };
            if display.is_null() {
                libc::close(fd);
                return None;
            }
            Some(display)
        });
        if found.is_some() {
            std::mem::forget(library);
        }
        found
    }
}

impl Egl {
    fn new() -> Result<(Egl, glow::Context), String> {
        let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
        let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
        let loaded = egl::Egl::load_with(|name| symbol(&library, name));
        // SAFETY: plain EGL calls with attribute lists that end in NONE and
        // live as long as each call
        unsafe {
            let mut display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                display = gbm_display(&loaded).ok_or("no EGL display")?;
            }
            if loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL, 0x{:X}", loaded.GetError()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let mut config = std::ptr::null();
            let mut pbuffer = false;
            for surface_type in [egl::PBUFFER_BIT, 0] {
                let attributes = [
                    egl::RENDERABLE_TYPE as i32,
                    egl::OPENGL_ES3_BIT as i32,
                    egl::SURFACE_TYPE as i32,
                    surface_type as i32,
                    egl::NONE as i32,
                ];
                let mut count = 0;
                if loaded.ChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) != 0 && count > 0 {
                    pbuffer = surface_type != 0;
                    break;
                }
                config = std::ptr::null();
            }
            if config.is_null() {
                return Err("no EGL config for OpenGL ES 3".into());
            }
            let context_attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
            let context = loaded.CreateContext(display, config, egl::NO_CONTEXT, context_attributes.as_ptr());
            if context.is_null() {
                return Err(format!("could not create an OpenGL ES 3.1 context, 0x{:X}", loaded.GetError()));
            }
            let surface_attributes = [egl::WIDTH as i32, 16, egl::HEIGHT as i32, 16, egl::NONE as i32];
            let surface = match pbuffer {
                true => loaded.CreatePbufferSurface(display, config, surface_attributes.as_ptr()),
                false => egl::NO_SURFACE,
            };
            if loaded.MakeCurrent(display, surface, surface, context) == 0 {
                return Err(format!("could not make the context current, 0x{:X}", loaded.GetError()));
            }
            let gl = glow::Context::from_loader_function(|name| {
                let direct = gles.as_ref().map_or(std::ptr::null(), |gles| symbol(gles, name));
                if !direct.is_null() {
                    return remember(name, direct);
                }
                let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
                remember(name, loaded.GetProcAddress(c_name.as_ptr()) as *const c_void)
            });
            Ok((Egl { libraries: (library, gles), egl: loaded, display, config, context, surface }, gl))
        }
    }

    /// a context drawing into a GBM surface on the panel, for showing the
    /// screens through KMS.
    fn new_kms() -> Result<(Egl, glow::Context, Kms), String> {
        // SAFETY: as in new, and the window is the GBM surface Kms made
        unsafe {
            let kms = Kms::open()?;
            // the Mali driver is GBM, EGL and GLES in one library, and its GBM
            // device only works with its own EGL; dArkOS points libEGL.so.1
            // at glvnd and Mesa instead
            let together = !symbol(&kms.gbm, "eglGetDisplay").is_null();
            let (egl_names, gles_names): (&[&str], &[&str]) = match together {
                true => (&["libgbm.so.1", "libgbm.so"], &["libgbm.so.1", "libgbm.so"]),
                false => (&["libEGL.so.1", "libEGL.so"], &["libGLESv2.so.2", "libGLESv2.so"]),
            };
            let library = open(egl_names).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(gles_names).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let (mut major, mut minor) = (0, 0);
            let mut display = loaded.GetDisplay(kms.device as glutin_egl_sys::EGLNativeDisplayType);
            let mut ready = !display.is_null() && loaded.Initialize(display, &mut major, &mut minor) != 0;
            if !ready {
                // the Mali blob's legacy eglGetDisplay returns no display for
                // a GBM device (g29p1 on the RG52 Mini); the platform call works
                const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
                let platform = ["eglGetPlatformDisplay", "eglGetPlatformDisplayEXT"]
                    .iter()
                    .map(|name| symbol(&library, name))
                    .find(|address| !address.is_null());
                if let Some(address) = platform {
                    let get: extern "C" fn(u32, *mut c_void, *const i32) -> egl::types::EGLDisplay = std::mem::transmute(address);
                    display = get(EGL_PLATFORM_GBM_KHR, kms.device, std::ptr::null());
                    ready = !display.is_null() && loaded.Initialize(display, &mut major, &mut minor) != 0;
                }
            }
            if !ready {
                return Err(format!("could not initialize EGL on the GBM device, 0x{:X}, {}", loaded.GetError(), drawing_libraries()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [egl::RENDERABLE_TYPE as i32, egl::OPENGL_ES3_BIT as i32, egl::SURFACE_TYPE as i32, egl::WINDOW_BIT as i32, egl::NONE as i32];
            let mut configs = [std::ptr::null(); 64];
            let mut count = 0;
            loaded.ChooseConfig(display, attributes.as_ptr(), configs.as_mut_ptr(), 64, &mut count);
            let config = configs[..count.max(0) as usize]
                .iter()
                .copied()
                .find(|&config| {
                    let mut visual = 0;
                    loaded.GetConfigAttrib(display, config, egl::NATIVE_VISUAL_ID as i32, &mut visual);
                    visual as u32 == GBM_FORMAT_XRGB8888
                })
                .ok_or("no EGL config for the panel")?;
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, kms.surface as glutin_egl_sys::EGLNativeWindowType, 0)?;
            Ok((egl, gl, kms))
        }
    }

    /// screens through the framebuffer device, for a Mali driver built for
    /// it rather than for GBM, as on Allwinner's H700 under Knulli.
    fn new_fbdev() -> Result<(Egl, glow::Context, Box<[u16; 2]>), String> {
        // SAFETY: FBIOGET_VSCREENINFO fills the 160 bytes of the screen's
        // fb_var_screeninfo, its size first, and the EGL calls are as in new
        unsafe {
            let fd = libc::open(c"/dev/fb0".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return Err("could not open /dev/fb0".into());
            }
            let mut info = [0u32; 40];
            let read = libc::ioctl(fd, 0x4600, info.as_mut_ptr());
            libc::close(fd);
            if read < 0 {
                return Err("could not read the size of /dev/fb0".into());
            }
            let window = Box::new([info[0] as u16, info[1] as u16]);
            let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL for /dev/fb0, 0x{:X}, {}", loaded.GetError(), drawing_libraries()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [
                egl::RENDERABLE_TYPE as i32,
                egl::OPENGL_ES3_BIT as i32,
                egl::SURFACE_TYPE as i32,
                egl::WINDOW_BIT as i32,
                egl::RED_SIZE as i32,
                8,
                egl::GREEN_SIZE as i32,
                8,
                egl::BLUE_SIZE as i32,
                8,
                egl::NONE as i32,
            ];
            let (mut config, mut count) = (std::ptr::null(), 0);
            if loaded.ChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0 || count == 0 {
                return Err("no EGL config for /dev/fb0".into());
            }
            // the driver flips on the framebuffer, at the next refresh
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, window.as_ptr() as glutin_egl_sys::EGLNativeWindowType, 1)?;
            Ok((egl, gl, window))
        }
    }

    /// a context drawing into a native window set_window was given, and the
    /// window's size.
    fn new_window(window: *mut c_void) -> Result<(Egl, glow::Context, (i32, i32)), String> {
        // SAFETY: as in new, and the window stays valid as set_window asks
        unsafe {
            let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL for the window, 0x{:X}", loaded.GetError()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [
                egl::RENDERABLE_TYPE as i32,
                egl::OPENGL_ES3_BIT as i32,
                egl::SURFACE_TYPE as i32,
                egl::WINDOW_BIT as i32,
                egl::RED_SIZE as i32,
                8,
                egl::GREEN_SIZE as i32,
                8,
                egl::BLUE_SIZE as i32,
                8,
                egl::ALPHA_SIZE as i32,
                0,
                egl::DEPTH_SIZE as i32,
                0,
                egl::NONE as i32,
            ];
            let mut configs = [std::ptr::null(); 64];
            let mut count = 0;
            if loaded.ChooseConfig(display, attributes.as_ptr(), configs.as_mut_ptr(), 64, &mut count) == 0 || count <= 0 {
                return Err("no EGL config for the window".into());
            }
            // the first config with exactly eight bits a channel and no alpha,
            // which the window's buffers take without a conversion
            let exact = configs[..count as usize].iter().copied().find(|&config| {
                let size = |attribute: u32| {
                    let mut value = 0;
                    loaded.GetConfigAttrib(display, config, attribute as i32, &mut value);
                    value
                };
                [size(egl::RED_SIZE), size(egl::GREEN_SIZE), size(egl::BLUE_SIZE), size(egl::ALPHA_SIZE)] == [8, 8, 8, 0]
            });
            let config = exact.unwrap_or(configs[0]);
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, window as glutin_egl_sys::EGLNativeWindowType, 1)?;
            let (mut width, mut height) = (0, 0);
            egl.egl.QuerySurface(egl.display, egl.surface, egl::WIDTH as i32, &mut width);
            egl.egl.QuerySurface(egl.display, egl.surface, egl::HEIGHT as i32, &mut height);
            if width <= 0 || height <= 0 {
                return Err(format!("the window has no size, {width}x{height}"));
            }
            Ok((egl, gl, (width, height)))
        }
    }

    /// an OpenGL ES 3.1 context drawing to a window, swapping its buffers
    /// interval refreshes apart.
    unsafe fn on_window(
        (library, gles): (libloading::Library, Option<libloading::Library>),
        loaded: egl::Egl,
        display: egl::types::EGLDisplay,
        config: egl::types::EGLConfig,
        window: glutin_egl_sys::EGLNativeWindowType,
        interval: i32,
    ) -> Result<(Egl, glow::Context), String> {
        let context_attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
        let context = loaded.CreateContext(display, config, egl::NO_CONTEXT, context_attributes.as_ptr());
        if context.is_null() {
            return Err(format!("could not create an OpenGL ES 3.1 context, 0x{:X}", loaded.GetError()));
        }
        let surface = loaded.CreateWindowSurface(display, config, window, std::ptr::null());
        if surface.is_null() {
            return Err(format!("could not make a window surface, 0x{:X}", loaded.GetError()));
        }
        if loaded.MakeCurrent(display, surface, surface, context) == 0 {
            return Err(format!("could not make the context current, 0x{:X}", loaded.GetError()));
        }
        loaded.SwapInterval(display, interval);
        let gl = glow::Context::from_loader_function(|name| {
            let direct = gles.as_ref().map_or(std::ptr::null(), |gles| symbol(gles, name));
            if !direct.is_null() {
                return remember(name, direct);
            }
            let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
            remember(name, loaded.GetProcAddress(c_name.as_ptr()) as *const c_void)
        });
        Ok((Egl { libraries: (library, gles), egl: loaded, display, config, context, surface }, gl))
    }

    /// a context sharing this one's objects, made current on the calling
    /// thread with no surface, or a small pbuffer where EGL wants one.
    fn shared_context(&self) -> Option<glow::Context> {
        // SAFETY: plain EGL calls with attribute lists that end in NONE and
        // live as long as each call
        unsafe {
            let attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
            let context = self.egl.CreateContext(self.display, self.config, self.context, attributes.as_ptr());
            if context.is_null() {
                log::warn!("no context to compile shaders in, 0x{:X}", self.egl.GetError());
                return None;
            }
            let mut current = self.egl.MakeCurrent(self.display, egl::NO_SURFACE, egl::NO_SURFACE, context) != 0;
            if !current {
                let size = [egl::WIDTH as i32, 16, egl::HEIGHT as i32, 16, egl::NONE as i32];
                let surface = self.egl.CreatePbufferSurface(self.display, self.config, size.as_ptr());
                current = !surface.is_null() && self.egl.MakeCurrent(self.display, surface, surface, context) != 0;
            }
            if !current {
                log::warn!("could not make the context to compile shaders in current, 0x{:X}", self.egl.GetError());
                self.egl.DestroyContext(self.display, context);
                return None;
            }
            let gles = self.libraries.1.as_ref();
            Some(glow::Context::from_loader_function(|name| {
                let direct = gles.map_or(std::ptr::null(), |gles| symbol(gles, name));
                if !direct.is_null() {
                    return direct;
                }
                let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
                self.egl.GetProcAddress(c_name.as_ptr()) as *const c_void
            }))
        }
    }

    fn new_with_panel() -> Result<(Egl, glow::Context, Option<Output>), String> {
        let window = WINDOW.load(std::sync::atomic::Ordering::Acquire);
        if !window.is_null() {
            let (egl, gl, size) = Egl::new_window(window)?;
            log::info!("the screens go to a {}x{} window", size.0, size.1);
            return Ok((egl, gl, Some(Output::Window(size))));
        }
        if std::env::var_os("ZAKURO_KMS").is_some() {
            return match Egl::new_kms() {
                Ok((egl, gl, kms)) => Ok((egl, gl, Some(Output::Kms(kms)))),
                Err(error) if std::path::Path::new("/dev/fb0").exists() => {
                    let (egl, gl, window) = Egl::new_fbdev().map_err(|fbdev| format!("{error}, and {fbdev}"))?;
                    log::info!("{error}, the screens go through /dev/fb0, {}x{}", window[0], window[1]);
                    Ok((egl, gl, Some(Output::Fbdev(window))))
                }
                Err(error) => Err(error),
            };
        }
        let (egl, gl) = Egl::new()?;
        Ok((egl, gl, None))
    }
}

#[derive(Default)]
struct Cached {
    framebuffer: Option<glow::Framebuffer>,
    program: Option<glow::Program>,
    depth_range: Option<(u32, u32)>,
    scissor: Option<[i32; 4]>,
    color_mask: Option<u32>,
    blend: Option<Option<(u32, u32)>>,
    depth: Option<Option<u32>>,
    depth_mask: Option<bool>,
    stencil: Option<Option<(u32, u32, u32)>>,
    units: [Option<(glow::Texture, Option<glow::Sampler>)>; 3],
    tables_bound: Option<usize>,
    viewport: Option<[i32; 4]>,
    cull: Option<u32>,
    vertex_array: Option<glow::VertexArray>,
    stride: Option<usize>,
    uniforms: Vec<u32>,
    shading: Vec<u32>,
}

struct Ring {
    buffer: glow::Buffer,
    size: usize,
    offset: usize,
    align: usize,
    laps: u64,
}

impl Ring {
    fn new(gl: &glow::Context, target: u32, size: usize, align: usize) -> Result<Ring, String> {
        // SAFETY: the context is current
        let buffer = unsafe {
            let buffer = gl.create_buffer()?;
            gl.bind_buffer(target, Some(buffer));
            gl.buffer_data_size(target, size as i32, glow::STREAM_DRAW);
            buffer
        };
        Ok(Ring { buffer, size, offset: 0, align, laps: 0 })
    }

    fn push(&mut self, gl: &glow::Context, target: u32, bytes: &[u8]) -> Result<usize, String> {
        self.push_aligned(gl, target, bytes, self.align)
    }

    fn push_aligned(&mut self, gl: &glow::Context, target: u32, bytes: &[u8], align: usize) -> Result<usize, String> {
        let _timed = timed(17);
        let mut at = self.offset.div_ceil(align) * align;
        // SAFETY: the context is current, the range mapped lies inside the
        // buffer and past everything a draw before it reads, or in storage
        // orphaned for it
        unsafe {
            gl.bind_buffer(target, Some(self.buffer));
            if at + bytes.len() > self.size {
                if bytes.len() > self.size {
                    self.size = bytes.len().next_power_of_two();
                }
                gl.buffer_data_size(target, self.size as i32, glow::STREAM_DRAW);
                at = 0;
                self.laps += 1;
            }
            let access = glow::MAP_WRITE_BIT | glow::MAP_UNSYNCHRONIZED_BIT | glow::MAP_INVALIDATE_RANGE_BIT;
            let mapped = gl.map_buffer_range(target, at as i32, bytes.len() as i32, access);
            if mapped.is_null() {
                return Err("could not map a buffer".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped, bytes.len());
            gl.unmap_buffer(target);
        }
        self.offset = at + bytes.len();
        Ok(at)
    }
}

struct Texture {
    texture: glow::Texture,
    _texels: (Arc<[[u8; 4]]>, Option<Arc<[u8]>>),
    used: u64,
}

enum Attribute {
    Constant([f32; 4]),
    Array { offset: usize, ty: u32, count: u32, stride: u32 },
}

enum Attributes {
    Packed { bytes: Vec<u8>, stride: usize },
    Fetched { bytes: Vec<u8>, slots: Vec<Attribute> },
    /// in fast mode, arrays kept in buffers of their own on the GPU while
    /// the mesh cache keeps them, an offset of a slot is into its array.
    Resident { arrays: Vec<Arc<[u8]>>, slots: Vec<(usize, Attribute)> },
}

/// the vertex arrays of meshes the scenery draws every frame, each in a
/// buffer on the GPU, so it is uploaded once rather than every draw.
#[derive(Default)]
struct Resident {
    buffers: rustc_hash::FxHashMap<(usize, usize), (glow::Buffer, Arc<[u8]>, u64)>,
    bytes: usize,
    draws: u64,
}

/// how many bytes of arrays stay on the GPU before the oldest go.
const RESIDENT_BYTES: usize = 64 << 20;

impl Resident {
    fn buffer(&mut self, gl: &glow::Context, array: &Arc<[u8]>) -> Result<glow::Buffer, String> {
        let key = (Arc::as_ptr(array) as *const u8 as usize, array.len());
        let draws = self.draws;
        if let Some((buffer, _, used)) = self.buffers.get_mut(&key) {
            *used = draws;
            return Ok(*buffer);
        }
        if self.bytes + array.len() > RESIDENT_BYTES {
            // the arrays no draw used lately go, all of them when that is not
            // enough
            let recent = draws.saturating_sub(20_000);
            let mut gone = Vec::new();
            self.buffers.retain(|_, (buffer, array, used)| {
                let keep = *used >= recent;
                if !keep {
                    gone.push((*buffer, array.len()));
                }
                keep
            });
            if self.bytes + array.len() > RESIDENT_BYTES + gone.iter().map(|(_, len)| len).sum::<usize>() {
                gone.extend(self.buffers.drain().map(|(_, (buffer, array, _))| (buffer, array.len())));
            }
            for (buffer, len) in gone {
                self.bytes -= len;
                // SAFETY: on the GL thread, the draws that read it were issued
                unsafe { gl.delete_buffer(buffer) };
            }
        }
        // SAFETY: on the GL thread with the context current
        let buffer = unsafe {
            let buffer = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, array, glow::STATIC_DRAW);
            buffer
        };
        self.bytes += array.len();
        self.buffers.insert(key, (buffer, array.clone(), draws));
        Ok(buffer)
    }
}

enum Submit {
    Triangles { viewport: [i32; 4], vertices: Vec<u8>, count: i32 },
    Shaded { viewport: [i32; 4], cull: u32, uniforms: Vec<u32>, attributes: Attributes, indices: Vec<u8>, count: i32, range: (u32, u32), resident_indices: Option<Arc<[u8]>> },
}

struct DrawJob {
    /// a heavy mesh's key and the picture, for occlusion culling.
    occlusion: Option<(u64, u64)>,
    framebuffer: glow::Framebuffer,
    program: glow::Program,
    depth_range: (f32, f32),
    scissor: [i32; 4],
    color_mask: u32,
    blend: Option<(u32, u32)>,
    depth: Option<u32>,
    depth_mask: bool,
    stencil: Option<(u32, u32, bool)>,
    bound: [(glow::Texture, Option<glow::Sampler>); 3],
    uniforms: Vec<u32>,
    vertex_array: glow::VertexArray,
    shaded_array: glow::VertexArray,
    geometry: Submit,
    targets: [Option<glow::Texture>; 2],
    sources: [Option<glow::Texture>; 3],
}

impl DrawJob {
    fn submit(self, gl: GlRef) -> Result<(), String> {
        let _timed_gl = timed(9);
        for source in self.sources.into_iter().flatten() {
            flush_clears(Some(source));
        }
        local(|local| {
            // SAFETY: on the GL thread
            let query = match self.occlusion {
                Some((key, picture)) => match unsafe { local.occlusion.check(&gl, key, picture) } {
                    (false, _) => return Ok(()),
                    (true, query) => query,
                },
                None => None,
            };
            let mut cleared = false;
            let mut i = 0;
            while i < local.clears.len() {
                if !self.targets.contains(&Some(local.clears[i].texture)) {
                    i += 1;
                    continue;
                }
                let clear = local.clears.swap_remove(i);
                // SAFETY: on the GL thread, the framebuffer holds the surface,
                // cleared first thing in its pass
                unsafe {
                    if !cleared {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
                    }
                    clear_bound(&gl, clear.kind, &clear.pixel);
                }
                cleared = true;
            }
            if std::mem::take(&mut local.disturbed) || cleared {
                local.cache = Cached::default();
                if cleared {
                    local.cache.framebuffer = Some(self.framebuffer);
                }
            }
            let cache = &mut local.cache;
            let factor = |raw: u32| FACTORS.get((raw & 0xF) as usize).copied().unwrap_or(glow::ONE);
            let equation = |raw: u32| EQUATIONS.get((raw & 7) as usize).copied().unwrap_or(glow::FUNC_ADD);
            // SAFETY: on the GL thread, everything bound was made from its
            // context and the buffers are sized for what is written into them
            unsafe {
                if cache.framebuffer != Some(self.framebuffer) {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
                    cache.framebuffer = Some(self.framebuffer);
                }
                if cache.program != Some(self.program) {
                    gl.use_program(Some(self.program));
                    cache.program = Some(self.program);
                }
                let range = (self.depth_range.0.to_bits(), self.depth_range.1.to_bits());
                if cache.depth_range != Some(range) {
                    gl.depth_range_f32(self.depth_range.0, self.depth_range.1);
                    cache.depth_range = Some(range);
                }
                let scissor = self.scissor;
                if cache.scissor != Some(scissor) {
                    if cache.scissor.is_none() {
                        gl.enable(glow::SCISSOR_TEST);
                    }
                    gl.scissor(scissor[0], scissor[1], scissor[2], scissor[3]);
                    cache.scissor = Some(scissor);
                }
                let color_mask = self.color_mask;
                if cache.color_mask != Some(color_mask) {
                    gl.color_mask(color_mask & 1 != 0, color_mask & 2 != 0, color_mask & 4 != 0, color_mask & 8 != 0);
                    cache.color_mask = Some(color_mask);
                }
                if cache.blend != Some(self.blend) {
                    match self.blend {
                        Some((config, color)) => {
                            let constant = color.to_le_bytes().map(|c| c as f32 / 255.0);
                            gl.enable(glow::BLEND);
                            gl.blend_equation_separate(equation(config), equation(config >> 8));
                            gl.blend_func_separate(factor(config >> 16), factor(config >> 20), factor(config >> 24), factor(config >> 28));
                            gl.blend_color(constant[0], constant[1], constant[2], constant[3]);
                        }
                        None => gl.disable(glow::BLEND),
                    }
                    cache.blend = Some(self.blend);
                }
                if cache.depth != Some(self.depth) {
                    match self.depth {
                        Some(func) => {
                            gl.enable(glow::DEPTH_TEST);
                            gl.depth_func(func);
                        }
                        None => gl.disable(glow::DEPTH_TEST),
                    }
                    cache.depth = Some(self.depth);
                }
                if cache.depth_mask != Some(self.depth_mask) {
                    gl.depth_mask(self.depth_mask);
                    cache.depth_mask = Some(self.depth_mask);
                }
                let stencil = self.stencil.map(|(test, op, writable)| (test & 0xFFFF_FFF0, op, if writable { (test >> 8) & 0xFF } else { 0 }));
                if cache.stencil != Some(stencil) {
                    match self.stencil {
                        Some((test, op, writable)) => {
                            gl.enable(glow::STENCIL_TEST);
                            gl.stencil_func(COMPARES[((test >> 4) & 7) as usize], ((test >> 16) & 0xFF) as i32, (test >> 24) & 0xFF);
                            gl.stencil_op(STENCIL_OPS[(op & 7) as usize], STENCIL_OPS[((op >> 4) & 7) as usize], STENCIL_OPS[((op >> 8) & 7) as usize]);
                            gl.stencil_mask(if writable { (test >> 8) & 0xFF } else { 0 });
                        }
                        None => gl.disable(glow::STENCIL_TEST),
                    }
                    cache.stencil = Some(stencil);
                }
                for (unit, (texture, sampler)) in self.bound.into_iter().enumerate() {
                    if cache.units[unit] != Some((texture, sampler)) {
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                        gl.bind_sampler(unit as u32, sampler);
                        cache.units[unit] = Some((texture, sampler));
                    }
                }
                if cache.uniforms != self.uniforms {
                    let offset = local.uniforms.push(&gl, glow::UNIFORM_BUFFER, word_bytes(&self.uniforms))?;
                    gl.bind_buffer_range(glow::UNIFORM_BUFFER, 3, Some(local.uniforms.buffer), offset as i32, self.uniforms.len() as i32 * 4);
                    local.cache.uniforms = self.uniforms;
                }
                if let Some(at) = local.tables_at.filter(|&at| local.cache.tables_bound != Some(at)) {
                    gl.bind_buffer_range(glow::SHADER_STORAGE_BUFFER, 4, Some(local.tables.buffer), at as i32, TABLES_SIZE as i32);
                    local.cache.tables_bound = Some(at);
                }
                let cache = &mut local.cache;
                match self.geometry {
                    Submit::Triangles { viewport, vertices, count } => {
                        if cache.viewport != Some(viewport) {
                            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
                            cache.viewport = Some(viewport);
                        }
                        if cache.cull != Some(0) {
                            gl.disable(glow::CULL_FACE);
                            cache.cull = Some(0);
                        }
                        if cache.vertex_array != Some(self.vertex_array) {
                            gl.bind_vertex_array(Some(self.vertex_array));
                            cache.vertex_array = Some(self.vertex_array);
                        }
                        let offset = local.vertices.push(&gl, glow::ARRAY_BUFFER, &vertices)?;
                        gl.draw_arrays(glow::TRIANGLES, (offset / VERTEX_SIZE) as i32, count);
                    }
                    Submit::Shaded { viewport, cull, uniforms, attributes, indices, count, range, resident_indices } => {
                        if cache.viewport != Some(viewport) {
                            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
                            cache.viewport = Some(viewport);
                        }
                        if cache.cull != Some(cull) {
                            match cull {
                                0 => gl.disable(glow::CULL_FACE),
                                cull => {
                                    gl.enable(glow::CULL_FACE);
                                    gl.front_face(glow::CCW);
                                    gl.cull_face(if cull == 1 { glow::FRONT } else { glow::BACK });
                                }
                            }
                            cache.cull = Some(cull);
                        }
                        if cache.shading != uniforms {
                            let offset = local.shading.push(&gl, glow::UNIFORM_BUFFER, word_bytes(&uniforms))?;
                            gl.bind_buffer_range(glow::UNIFORM_BUFFER, 6, Some(local.shading.buffer), offset as i32, uniforms.len() as i32 * 4);
                            local.cache.shading = uniforms;
                        }
                        let cache = &mut local.cache;
                        if cache.vertex_array != Some(self.shaded_array) {
                            gl.bind_vertex_array(Some(self.shaded_array));
                            cache.vertex_array = Some(self.shaded_array);
                        }
                        let base_vertex = match attributes {
                            Attributes::Fetched { bytes, slots } => {
                                let at = local.inputs.push_aligned(&gl, glow::ARRAY_BUFFER, &bytes, 16)?;
                                for (slot, attribute) in slots.iter().enumerate() {
                                    let slot = slot as u32;
                                    match *attribute {
                                        Attribute::Array { offset, ty, count, stride } => {
                                            let kind = [glow::BYTE, glow::UNSIGNED_BYTE, glow::SHORT, glow::FLOAT][(ty & 3) as usize];
                                            gl.enable_vertex_attrib_array(slot);
                                            gl.vertex_attrib_pointer_f32(slot, count as i32, kind, false, stride as i32, (at + offset) as i32);
                                        }
                                        Attribute::Constant(value) => {
                                            gl.disable_vertex_attrib_array(slot);
                                            gl.vertex_attrib_4_f32(slot, value[0], value[1], value[2], value[3]);
                                        }
                                    }
                                }
                                for slot in slots.len() as u32..16 {
                                    gl.disable_vertex_attrib_array(slot);
                                }
                                local.cache.stride = None;
                                0
                            }
                            Attributes::Resident { arrays, slots } => {
                                local.resident.draws += 1;
                                let buffers = arrays.iter().map(|array| local.resident.buffer(&gl, array)).collect::<Result<Vec<_>, String>>()?;
                                for (slot, (array, attribute)) in slots.iter().enumerate() {
                                    let slot = slot as u32;
                                    match *attribute {
                                        Attribute::Array { offset, ty, count, stride } => {
                                            let kind = [glow::BYTE, glow::UNSIGNED_BYTE, glow::SHORT, glow::FLOAT][(ty & 3) as usize];
                                            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffers[*array]));
                                            gl.enable_vertex_attrib_array(slot);
                                            gl.vertex_attrib_pointer_f32(slot, count as i32, kind, false, stride as i32, offset as i32);
                                        }
                                        Attribute::Constant(value) => {
                                            gl.disable_vertex_attrib_array(slot);
                                            gl.vertex_attrib_4_f32(slot, value[0], value[1], value[2], value[3]);
                                        }
                                    }
                                }
                                for slot in slots.len() as u32..16 {
                                    gl.disable_vertex_attrib_array(slot);
                                }
                                local.cache.stride = None;
                                0
                            }
                            Attributes::Packed { bytes, stride } => {
                                let vertex_bytes = stride.max(1) * 16;
                                let offset = local.inputs.push_aligned(&gl, glow::ARRAY_BUFFER, &bytes, vertex_bytes)?;
                                if local.cache.stride != Some(stride) {
                                    for slot in 0..16u32 {
                                        if (slot as usize) < stride {
                                            gl.enable_vertex_attrib_array(slot);
                                            gl.vertex_attrib_pointer_f32(slot, 4, glow::FLOAT, false, (stride * 16) as i32, (slot as usize * 16) as i32);
                                        } else {
                                            gl.disable_vertex_attrib_array(slot);
                                        }
                                    }
                                    local.cache.stride = Some(stride);
                                }
                                (offset / vertex_bytes) as i32
                            }
                        };
                        // a mesh's indices stay on the GPU, others go through the ring
                        let (index_offset, kind) = match &resident_indices {
                            Some(bytes) => {
                                let buffer = local.resident.buffer(&gl, bytes)?;
                                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(buffer));
                                (0, if bytes.len() == count as usize * 2 { glow::UNSIGNED_SHORT } else { glow::UNSIGNED_INT })
                            }
                            None => {
                                let kind = if indices.len() == count as usize * 2 { glow::UNSIGNED_SHORT } else { glow::UNSIGNED_INT };
                                (local.indices.push(&gl, glow::ELEMENT_ARRAY_BUFFER, &indices)?, kind)
                            }
                        };
                        let called = timed(18);
                        // the range the indices span, which the driver would
                        // otherwise work out reading them back from the
                        // buffer, memory the CPU does not cache
                        let draw_range = DRAW_RANGE.load(std::sync::atomic::Ordering::Relaxed);
                        static NO_RANGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                        if let Some(query) = query {
                            gl.begin_query(glow::ANY_SAMPLES_PASSED_CONSERVATIVE, query);
                        }
                        if draw_range != 0 && range.0 <= range.1 && !*NO_RANGE.get_or_init(|| crate::switch("ZAKURO_NO_RANGE", "vertex ranges off")) {
                            let draw: extern "C" fn(u32, u32, u32, i32, u32, *const c_void, i32) = std::mem::transmute(draw_range);
                            draw(glow::TRIANGLES, range.0, range.1, count, kind, index_offset as *const c_void, base_vertex);
                        } else {
                            gl.draw_elements_base_vertex(glow::TRIANGLES, count, kind, index_offset as i32, base_vertex);
                        }
                        if query.is_some() {
                            gl.end_query(glow::ANY_SAMPLES_PASSED_CONSERVATIVE);
                        }
                        drop(called);
                    }
                }
            }
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
struct GlRef(*const glow::Context);

// SAFETY: the context is only ever used on the GL thread, which every
// holder hands its work to
unsafe impl Send for GlRef {}

impl std::ops::Deref for GlRef {
    type Target = glow::Context;

    fn deref(&self) -> &glow::Context {
        // SAFETY: the context is leaked when the GL thread makes it
        unsafe { &*self.0 }
    }
}

/// GL_OES_compressed_ETC1_RGB8_texture's format.
const ETC1_RGB8_OES: u32 = 0x8D64;

/// in fast mode, ZAKURO_NO_CULL turning it off, heavy meshes drawn inside an
/// occlusion query, and left
/// out for a few pictures after one found nothing of them on screen, which
/// spares the GPU shading the vertices of scenery out of view.
#[derive(Default)]
struct Occlusion {
    meshes: rustc_hash::FxHashMap<u64, Hidden>,
    free: Vec<glow::Query>,
}

#[derive(Default)]
struct Hidden {
    query: Option<(glow::Query, u64)>,
    until: u64,
}

/// heavy draws left out and heavy draws looked at, for ZAKURO_GLES_TIMES.
static CULLED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// pictures a mesh found off screen is left out for, two or three frames.
const HIDDEN_PICTURES: u64 = 8;

impl Occlusion {
    /// whether to draw the mesh, and the query to draw it inside when it is
    /// tried again.
    /// SAFETY: on the GL thread
    unsafe fn check(&mut self, gl: &glow::Context, key: u64, picture: u64) -> (bool, Option<glow::Query>) {
        if self.meshes.len() > 8192 {
            self.meshes.retain(|_, mesh| mesh.query.is_some());
        }
        let Occlusion { meshes, free } = self;
        let mesh = meshes.entry(key).or_default();
        unsafe {
            if let Some((query, tried)) = mesh.query {
                if gl.get_query_parameter_u32(query, glow::QUERY_RESULT_AVAILABLE) != 0 {
                    if gl.get_query_parameter_u32(query, glow::QUERY_RESULT) == 0 {
                        mesh.until = tried + HIDDEN_PICTURES;
                    }
                    free.push(query);
                    mesh.query = None;
                }
            }
            CULLED[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if picture < mesh.until {
                CULLED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (false, None);
            }
            if mesh.query.is_some() {
                return (true, None);
            }
            let Some(query) = free.pop().or_else(|| gl.create_query().ok()) else { return (true, None) };
            mesh.query = Some((query, picture));
            (true, Some(query))
        }
    }
}

struct Local {
    cache: Cached,
    disturbed: bool,
    uniforms: Ring,
    shading: Ring,
    inputs: Ring,
    indices: Ring,
    vertices: Ring,
    resident: Resident,
    occlusion: Occlusion,
    /// the lighting and procedural texture tables, each set where the last
    /// upload left it.
    tables: Ring,
    tables_at: Option<usize>,
    table_sets: rustc_hash::FxHashMap<u64, (u64, usize)>,
    clears: Vec<Clear>,
}

struct Clear {
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    kind: Kind,
    pixel: Vec<u8>,
}

thread_local! {
    static GL_CONTEXT: std::cell::Cell<Option<&'static glow::Context>> = const { std::cell::Cell::new(None) };
}

/// clears the bound framebuffer's attachment of a kind to a pixel.
///
/// # Safety
/// on the GL thread, with a framebuffer holding that kind bound
unsafe fn clear_bound(gl: &glow::Context, kind: Kind, pixel: &[u8]) {
    gl.disable(glow::SCISSOR_TEST);
    match kind {
        Kind::Color(format) => {
            gl.color_mask(true, true, true, true);
            let rgba = format.decode(pixel).map(|c| c as f32 / 255.0);
            gl.clear_buffer_f32_slice(glow::COLOR, 0, &rgba);
        }
        Kind::Depth(sample) => {
            let (depth, stencil) = match sample {
                2 => (((u16::from_le_bytes([pixel[0], pixel[1]]) as u64 * 0xFF_FFFF + 0x7FFF) / 0xFFFF) as u32, 0),
                3 => (u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]), 0),
                _ => (u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]), pixel[3] as i32),
            };
            gl.depth_mask(true);
            gl.stencil_mask(0xFF);
            gl.clear_buffer_depth_stencil(glow::DEPTH_STENCIL, 0, depth as f32 / 16_777_215.0, stencil);
        }
    }
}

/// ZAKURO_SIM_GPU=factor, for a PC standing in for a handheld: the GPU time
/// the kernel counts for this process, times the factor, goes on a modelled
/// GPU's timeline, a present waits until that GPU finished the picture before
/// and a write back until it finished everything.
struct SimGpu {
    factor: f64,
    counted: u64,
    checked: std::time::Instant,
    free_at: std::time::Instant,
    previous: Option<std::time::Instant>,
    busy: f64,
    since: std::time::Instant,
}

thread_local! {
    static SIM_GPU: std::cell::RefCell<Option<SimGpu>> = const { std::cell::RefCell::new(None) };
}

fn sim_factor() -> Option<f64> {
    static FACTOR: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *FACTOR.get_or_init(|| std::env::var("ZAKURO_SIM_GPU").ok().and_then(|factor| factor.parse().ok()))
}

/// nanoseconds the GPU spent on this process's work, from its DRM files.
fn sim_gpu_time() -> u64 {
    let Ok(entries) = std::fs::read_dir("/proc/self/fdinfo") else { return 0 };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .map(|text| text.lines().filter(|line| line.starts_with("drm-engine-")).filter_map(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok()).sum::<u64>())
        .max()
        .unwrap_or(0)
}

/// the modelled GPU's work so far, and when it is done with it.
fn sim_finish(gl: &glow::Context) -> Option<std::time::Instant> {
    let factor = sim_factor()?;
    SIM_GPU.with(|sim| {
        let mut sim = sim.borrow_mut();
        let now = std::time::Instant::now();
        let sim = sim.get_or_insert_with(|| SimGpu { factor, counted: sim_gpu_time(), checked: now, free_at: now, previous: None, busy: 0.0, since: now });
        // SAFETY: on the GL thread
        unsafe { gl.finish() };
        let counted = sim_gpu_time();
        let modelled = counted.saturating_sub(sim.counted) as f64 * 1e-9 * sim.factor;
        sim.free_at = sim.free_at.max(sim.checked) + std::time::Duration::from_secs_f64(modelled);
        (sim.counted, sim.checked) = (counted, now);
        sim.busy += modelled;
        Some(sim.free_at)
    })
}

fn sim_present(gl: &glow::Context) {
    let Some(done) = sim_finish(gl) else { return };
    SIM_GPU.with(|sim| {
        let mut sim = sim.borrow_mut();
        let Some(sim) = sim.as_mut() else { return };
        if let Some(previous) = sim.previous.replace(done) {
            std::thread::sleep(previous.saturating_duration_since(std::time::Instant::now()));
        }
        let elapsed = sim.since.elapsed().as_secs_f64();
        if elapsed >= 5.0 {
            log::info!("sim gpu load {:.0}%", sim.busy / elapsed * 100.0);
            (sim.busy, sim.since) = (0.0, std::time::Instant::now());
        }
    });
}

fn sim_wait(gl: &glow::Context) {
    if let Some(done) = sim_finish(gl) {
        std::thread::sleep(done.saturating_duration_since(std::time::Instant::now()));
    }
}

/// does the clears left for a draw into their surfaces, all of them or the
/// one surface's.
fn flush_clears(only: Option<glow::Texture>) {
    let Some(gl) = GL_CONTEXT.get() else { return };
    LOCAL.with(|local| {
        let Ok(mut local) = local.try_borrow_mut() else { return };
        let Some(local) = local.as_mut() else { return };
        let mut i = 0;
        while i < local.clears.len() {
            if only.is_some_and(|texture| local.clears[i].texture != texture) {
                i += 1;
                continue;
            }
            let clear = local.clears.swap_remove(i);
            // SAFETY: on the GL thread, the framebuffer holds the surface
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(clear.framebuffer));
                clear_bound(gl, clear.kind, &clear.pixel);
            }
            local.disturbed = true;
        }
    });
}

thread_local! {
    static LOCAL: std::cell::RefCell<Option<Local>> = const { std::cell::RefCell::new(None) };
}

fn local<R>(f: impl FnOnce(&mut Local) -> R) -> R {
    LOCAL.with(|local| f(local.borrow_mut().as_mut().expect("the GL thread's state is set up")))
}

fn disturb() {
    LOCAL.with(|local| {
        if let Some(local) = local.borrow_mut().as_mut() {
            local.disturbed = true;
        }
    });
}

type Job = Box<dyn FnOnce() + Send>;

type ProgramKey = ([u32; FRAGMENT_CONSTANTS], bool, Vec<u32>, u64);

/// compiles and links a program, or loads it from the program binary cache
/// when it was made before, its samplers on the texture units.
fn build_program(gl: &glow::Context, vertex: &str, fragment: &str, cached: Option<&std::path::Path>) -> Result<glow::Program, String> {
    let started = std::time::Instant::now();
    let _compiling = COMPILER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: the context is current on this thread, the shaders are deleted
    // once linked into the program
    unsafe {
        let program = gl.create_program()?;
        let loaded = cached.and_then(|path| std::fs::read(path).ok()).filter(|bytes| bytes.len() > 4).is_some_and(|bytes| {
            let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            gl.program_binary(program, &glow::ProgramBinary { buffer: bytes[4..].to_vec(), format });
            gl.get_program_link_status(program)
        });
        if !loaded {
            let mut shaders = Vec::new();
            for (stage, source) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
                let shader = gl.create_shader(stage)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(format!("could not compile a shader, {}", gl.get_shader_info_log(shader)));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.program_binary_retrievable_hint(program, true);
            gl.link_program(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            if !gl.get_program_link_status(program) {
                return Err(format!("could not link a program, {}", gl.get_program_info_log(program)));
            }
            if let (Some(path), Some(binary)) = (cached, gl.get_program_binary(program)) {
                let mut bytes = binary.format.to_le_bytes().to_vec();
                bytes.extend_from_slice(&binary.buffer);
                let _ = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(path, bytes));
            }
        }
        gl.use_program(Some(program));
        for unit in 0..3 {
            if let Some(location) = gl.get_uniform_location(program, &format!("texture{unit}")) {
                gl.uniform_1_i32(Some(&location), unit);
            }
        }
        log::debug!("program {} in {:.1} ms", if loaded { "loaded" } else { "compiled" }, started.elapsed().as_secs_f64() * 1000.0);
        Ok(program)
    }
}

/// a program for the compiler thread to make.
struct Compile {
    key: ProgramKey,
    vertex: String,
    fragment: String,
    cached: Option<std::path::PathBuf>,
}

/// a thread compiling new programs in a context that shares the GL thread's
/// objects, so that a draw goes on with a stand-in rather than wait for the
/// driver's compiler.
struct Compiler {
    jobs: std::sync::mpsc::Sender<Compile>,
    done: std::sync::mpsc::Receiver<(ProgramKey, Result<glow::Program, String>)>,
    pending: rustc_hash::FxHashSet<ProgramKey>,
    failed: rustc_hash::FxHashMap<ProgramKey, String>,
}

impl Compiler {
    fn start(egl: &'static Egl) -> Option<Compiler> {
        if crate::switch("ZAKURO_SYNC_SHADERS", "shaders compiled when a draw needs them") {
            return None;
        }
        let (jobs, received) = std::sync::mpsc::channel::<Compile>();
        let (finished, done) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("gles-compiler".into())
            .spawn(move || {
                let Some(gl) = egl.shared_context() else {
                    let _ = ready.send(false);
                    return;
                };
                // SAFETY: lowers the priority of this thread alone, which is
                // what Linux takes 0 for, the game's threads going first
                unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
                let _ = ready.send(true);
                for job in received {
                    let result = build_program(&gl, &job.vertex, &job.fragment, job.cached.as_deref());
                    // SAFETY: the context is current on this thread
                    unsafe { gl.finish() };
                    if finished.send((job.key, result)).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        started.recv().unwrap_or(false).then(|| Compiler { jobs, done, pending: Default::default(), failed: Default::default() })
    }
}

struct AssertSend<T>(T);

// SAFETY: only used for work the sending thread waits on, so nothing it
// holds is touched from two threads at once
unsafe impl<T> Send for AssertSend<T> {}

struct Worker {
    jobs: std::sync::mpsc::Sender<(bool, Job)>,
    thread: ThreadId,
    /// draws waiting to go over together, one wakeup for many
    held: std::sync::Mutex<Vec<Job>>,
}

impl Worker {
    fn start() -> Result<(Worker, GlRef, Option<Compiler>), String> {
        let (jobs, receiver) = std::sync::mpsc::channel::<(bool, Job)>();
        let (ready, made) = std::sync::mpsc::channel();
        let handle = thread::Builder::new()
            .name("gles".into())
            .spawn(move || match Egl::new_with_panel() {
                Ok((egl, gl, output)) => {
                    let egl: &'static Egl = Box::leak(Box::new(egl));
                    let output = match output {
                        Some(output) => Some(output),
                        // SAFETY: on the GL thread, its context current
                        None if std::env::var_os("ZAKURO_PANEL_SHOT").is_some() => unsafe {
                            let image = gl.create_renderbuffer().expect("a renderbuffer for the panel");
                            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(image));
                            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, 640, 480);
                            let framebuffer = gl.create_framebuffer().expect("a framebuffer for the panel");
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(image));
                            Some(Output::Offscreen(framebuffer))
                        },
                        None => None,
                    };
                    if let Some(output) = output {
                        let ((width, height), turn) = panel_shape(&output);
                        let layout = layout_for(if turn.is_some() { (height, width) } else { (width, height) });
                        let _ = LAYOUT.set(layout);
                        log::info!("screens laid out in {}x{}", layout.0, layout.1);
                        PANEL.with(|screen| *screen.borrow_mut() = Some(Panel { egl, output, program: None, textures: [None, None], empty: None, linear: None, overlay: None, overlay_textures: HashMap::new(), between: Between::default(), canvas: None, second: None }));
                    }
                    let gl: &'static glow::Context = Box::leak(Box::new(gl));
                    GL_CONTEXT.set(Some(gl));
                    let _ = ready.send(Ok((GlRef(gl), Compiler::start(egl))));
                    for (keeps_clears, job) in receiver {
                        if !keeps_clears {
                            flush_clears(None);
                        }
                        job();
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| error.to_string())?;
        let (gl, compiler) = made.recv().map_err(|_| "the GL thread stopped".to_owned())??;
        Ok((Worker { jobs, thread: handle.thread().id(), held: Default::default() }, gl, compiler))
    }

    fn here(&self) -> bool {
        thread::current().id() == self.thread
    }

    fn post(&self, job: impl FnOnce() + Send + 'static) {
        if self.here() {
            flush_clears(None);
            job();
        } else {
            self.flush();
            self.jobs.send((false, Box::new(job))).expect("the GL thread stopped");
        }
    }

    /// posts work that touches no surface, the clears left for draws stay.
    fn post_keeping(&self, job: impl FnOnce() + Send + 'static) {
        if self.here() {
            job();
        } else {
            self.flush();
            self.jobs.send((true, Box::new(job))).expect("the GL thread stopped");
        }
    }

    /// a draw, held to go with the next few.
    fn post_draw(&self, job: impl FnOnce() + Send + 'static) {
        if self.here() {
            job();
            return;
        }
        let mut held = self.held.lock().unwrap();
        held.push(Box::new(job));
        if held.len() >= 16 {
            let draws = std::mem::take(&mut *held);
            drop(held);
            self.send_draws(draws);
        }
    }

    fn flush(&self) {
        let draws = std::mem::take(&mut *self.held.lock().unwrap());
        if !draws.is_empty() {
            self.send_draws(draws);
        }
    }

    fn send_draws(&self, draws: Vec<Job>) {
        self.jobs.send((true, Box::new(move || draws.into_iter().for_each(|draw| draw())))).expect("the GL thread stopped");
    }

    fn run<R>(&self, job: impl FnOnce() -> R) -> R {
        if self.here() {
            flush_clears(None);
            return job();
        }
        let (done, result) = std::sync::mpsc::sync_channel(1);
        let job = AssertSend(job);
        let wrapped: Box<dyn FnOnce() + '_> = Box::new(move || {
            let job = job;
            let _ = done.send(AssertSend((job.0)()));
        });
        // SAFETY: this thread waits below until the job has run, so what it
        // borrows outlives it, and nothing else touches that meanwhile
        let wrapped: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + '_>, Box<dyn FnOnce() + Send + 'static>>(wrapped) };
        let _timed = timed(20);
        self.flush();
        self.jobs.send((false, wrapped)).expect("the GL thread stopped");
        result.recv().expect("the GL thread stopped").0
    }
}

pub struct Gles {
    gl: GlRef,
    worker: Arc<Worker>,
    presenting: Arc<std::sync::atomic::AtomicUsize>,
    name: String,
    programs: rustc_hash::FxHashMap<ProgramKey, glow::Program>,
    compiler: Option<Compiler>,
    shades: bool,
    translated: rustc_hash::FxHashMap<(u64, u32, [u32; SEMANTICS]), Option<u64>>,
    vertex_sources: HashMap<u64, String>,
    shaded_array: glow::VertexArray,
    generic: bool,
    vertex_array: glow::VertexArray,
    tables: Option<(u64, u64)>,
    table_hashes: [(u64, u64); 24],
    table_sets: rustc_hash::FxHashMap<[u64; 25], (u64, Arc<[u8]>)>,
    table_set_ids: u64,
    texture_names: Vec<glow::Texture>,
    names_coming: Option<std::sync::mpsc::Receiver<Vec<glow::Texture>>>,
    framebuffer_names: Vec<glow::Framebuffer>,
    framebuffers_coming: Option<std::sync::mpsc::Receiver<Vec<glow::Framebuffer>>>,
    sampler_names: Vec<glow::Sampler>,
    samplers_coming: Option<std::sync::mpsc::Receiver<Vec<glow::Sampler>>>,
    surfaces: Vec<Surface>,
    framebuffers: rustc_hash::FxHashMap<(Option<usize>, Option<usize>), glow::Framebuffer>,
    textures: rustc_hash::FxHashMap<usize, Texture>,
    samplers: rustc_hash::FxHashMap<(bool, Wrap, Wrap), glow::Sampler>,
    blank: glow::Texture,
    transfer: Option<glow::Program>,
    upright: Option<glow::Program>,
    packers: Option<(glow::Program, glow::Program, glow::VertexArray)>,
    copiers: Option<([glow::Program; 3], glow::VertexArray)>,
    copies: rustc_hash::FxHashMap<DrawnTexture, (glow::Texture, glow::Framebuffer)>,
    copied: rustc_hash::FxHashMap<DrawnTexture, (glow::Texture, u64, u32)>,
    /// the memory rows a texture over a drawing last had, to upload them
    /// again only when they change.
    over_drawing: rustc_hash::FxHashMap<DrawnTexture, Vec<u8>>,
    flushes: u64,
    /// display transfers so far, the clock occlusion culling keeps time by.
    pictures: u64,
    warned_logic_op: bool,
    preloading: bool,
}

impl Gles {
    pub fn new() -> Result<Gles, String> {
        let (worker, gl, compiler) = Worker::start()?;
        let worker = Arc::new(worker);
        let on = worker.clone();
        on.run(move || Self::made(worker, gl, compiler))
    }

    fn made(worker: Arc<Worker>, gl: GlRef, compiler: Option<Compiler>) -> Result<Gles, String> {
        // SAFETY: on the GL thread, every object is made from its context
        unsafe {
            let name = format!("{} ({})", gl.get_parameter_string(glow::RENDERER), gl.get_parameter_string(glow::VERSION));
            let vertex_array = gl.create_vertex_array()?;
            let vertices = Ring::new(&gl, glow::ARRAY_BUFFER, 8 << 20, VERTEX_SIZE)?;
            gl.bind_vertex_array(Some(vertex_array));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices.buffer));
            for location in 0..6 {
                gl.enable_vertex_attrib_array(location);
                gl.vertex_attrib_pointer_f32(location, 4, glow::FLOAT, false, VERTEX_SIZE as i32, (location * 16) as i32);
            }
            let align = gl.get_parameter_i32(glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT).max(16) as usize;
            let uniforms = Ring::new(&gl, glow::UNIFORM_BUFFER, 2 << 20, align)?;
            let shading = Ring::new(&gl, glow::UNIFORM_BUFFER, 2 << 20, align)?;
            let inputs = Ring::new(&gl, glow::ARRAY_BUFFER, 8 << 20, 16)?;
            let shaded_array = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(shaded_array));
            let indices = Ring::new(&gl, glow::ELEMENT_ARRAY_BUFFER, 2 << 20, 4)?;
            gl.bind_vertex_array(None);
            let shades = !crate::switch("ZAKURO_CPU_SHADERS", "vertices shaded on the CPU") && gl.get_parameter_i32(glow::MAX_VERTEX_ATTRIBS) >= 16;
            let storage_align = gl.get_parameter_i32(glow::SHADER_STORAGE_BUFFER_OFFSET_ALIGNMENT).max(16) as usize;
            let tables = Ring::new(&gl, glow::SHADER_STORAGE_BUFFER, 8 << 20, storage_align)?;
            let blank = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(blank));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, 1, 1);
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 1, 1, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 255])));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::DITHER);
            log::info!("drawing with OpenGL ES on {name}");
            if gl.supported_extensions().contains("GL_OES_compressed_ETC1_RGB8_texture") && !crate::switch("ZAKURO_NO_ETC1", "ETC1 textures decoded") {
                crate::raster::SAMPLES_ETC1.store(true, std::sync::atomic::Ordering::Relaxed);
                log::info!("ETC1 textures stay compressed");
            }
            LOCAL.with(|local| {
                *local.borrow_mut() = Some(Local { cache: Cached::default(), disturbed: true, uniforms, shading, inputs, indices, vertices, resident: Resident::default(), tables, tables_at: None, table_sets: Default::default(), clears: Vec::new(), occlusion: Occlusion::default() });
            });
            let mut gles = Gles {
                gl,
                worker,
                presenting: Arc::default(),
                name,
                programs: Default::default(),
                compiler,
                shades,
                translated: Default::default(),
                vertex_sources: HashMap::new(),
                shaded_array,
                generic: crate::switch("ZAKURO_GENERIC_SHADERS", "generic shaders"),
                vertex_array,
                tables: None,
                table_hashes: [(u64::MAX, 0); 24],
                table_sets: Default::default(),
                table_set_ids: 0,
                texture_names: Vec::new(),
                names_coming: None,
                framebuffer_names: Vec::new(),
                framebuffers_coming: None,
                sampler_names: Vec::new(),
                samplers_coming: None,
                surfaces: Vec::new(),
                framebuffers: Default::default(),
                textures: Default::default(),
                samplers: Default::default(),
                blank,
                transfer: None,
                upright: None,
                packers: None,
                copiers: None,
                copies: Default::default(),
                copied: Default::default(),
                over_drawing: Default::default(),
                flushes: 0,
                pictures: 0,
                warned_logic_op: false,
                preloading: false,
            };
            gles.preload();
            gles.prepare_stand_ins();
            Ok(gles)
        }
    }

    /// has the compiler thread make the generic programs a draw stands in
    /// with while its own compiles, before any draw needs one.
    fn prepare_stand_ins(&mut self) {
        for writes_depth in [false, true] {
            for unbordered in [0, 0x400] {
                for lighting in [0, 1] {
                    let mut constants = [0u32; FRAGMENT_CONSTANTS];
                    constants[25] = unbordered;
                    constants[26] = lighting;
                    constants[28] = if writes_depth { WRITES_DEPTH } else { 0 };
                    let _ = self.program(&generic_constants(&constants), writes_depth, &[], 0, false);
                }
            }
        }
    }

    fn preload(&mut self) {
        let Some(path) = manifest_path() else { return };
        let Ok(mut text) = std::fs::read_to_string(&path) else { return };
        if text.lines().next() == Some("zakuro-gles 3") {
            text = upgrade_manifest(&text);
            let _ = std::fs::write(&path, &text);
        }
        if text.lines().next() != Some(MANIFEST_HEADER) {
            log::info!("{} is from another version, starting it over", path.display());
            let _ = std::fs::remove_file(&path);
            return;
        }
        let started = std::time::Instant::now();
        self.preloading = true;
        let mut count = 0;
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(Ok(vertex)) = fields.next().map(|field| u64::from_str_radix(field, 16)) else { continue };
            if vertex != 0 && !self.vertex_sources.contains_key(&vertex) {
                match std::fs::read_to_string(vertex_path(&path, vertex)) {
                    Ok(source) => {
                        self.vertex_sources.insert(vertex, source);
                    }
                    Err(_) => continue,
                }
            }
            let mut words = fields.map(|word| u32::from_str_radix(word, 16));
            let Some(Ok(writes_depth)) = words.next() else { continue };
            let words: Vec<u32> = words.filter_map(Result::ok).collect();
            if words.len() < FRAGMENT_CONSTANTS {
                continue;
            }
            let (constants, lighting) = words.split_at(FRAGMENT_CONSTANTS);
            if !lighting.is_empty() && (lighting.len() < 29 || lighting.len() != 29 + 2 * lighting[1] as usize) {
                continue;
            }
            let Ok(constants) = <[u32; FRAGMENT_CONSTANTS]>::try_from(constants) else { continue };
            match self.program(&constants, writes_depth != 0, lighting, vertex, true) {
                Ok(_) => count += 1,
                Err(error) => log::warn!("could not prepare a shader from {}, {error}", path.display()),
            }
        }
        self.preloading = false;
        log::info!("prepared {count} shaders from {} in {:.1} s", path.display(), started.elapsed().as_secs_f64());
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn shades(&self) -> bool {
        self.shades
    }

    pub(crate) fn fetches(&self) -> bool {
        self.shades
    }

    pub fn scale(&self) -> u32 {
        1
    }

    pub fn set_scale(&mut self, _scale: u32) -> u32 {
        1
    }

    fn texture_source(&self, texture: &DrawnTexture) -> Option<(usize, u32)> {
        let depth = match texture.format {
            ColorFormat::Rgba8 => Kind::Depth(4),
            ColorFormat::Rgb8 => Kind::Depth(3),
            _ => Kind::Color(texture.format),
        };
        let kinds = [Kind::Color(texture.format), depth];
        let tile_rows = 8 * texture.width * Kind::Color(texture.format).bytes();
        let end = texture.addr as u64 + (texture.height / 8 * tile_rows) as u64;
        static FROM_MEMORY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let past = !*FROM_MEMORY.get_or_init(|| crate::switch("ZAKURO_NO_PAST_DRAWING", "textures past a drawing read from memory"));
        (0..self.surfaces.len())
            .filter(|&i| {
                let s = &self.surfaces[i];
                kinds.contains(&s.kind) && s.tiled && !s.stale && s.width == texture.width && {
                    let offset = texture.addr.checked_sub(s.addr).filter(|offset| offset % tile_rows == 0);
                    offset.is_some_and(|offset| {
                        let row = offset / tile_rows * 8;
                        // a power-of-two texture over a smaller drawing, as a
                        // 512 row texture over 400 drawn rows, repeats the
                        // last row past it unless another drawing is there
                        let below = s.addr as u64 + s.size() as u64;
                        row + texture.height <= s.height
                            || (past && matches!(s.kind, Kind::Color(_)) && row < s.height && !self.surfaces.iter().any(|o| o.dirty.is_some() && below < o.addr as u64 + o.size() as u64 && (o.addr as u64) < end))
                    })
                }
            })
            .max_by_key(|&i| (self.surfaces[i].dirty.is_some(), self.surfaces[i].height))
            .map(|index| (index, (texture.addr - self.surfaces[index].addr) / tile_rows * 8))
    }

    /// a texture that starts with rows nothing drew and goes on into a
    /// drawing, as Fire Emblem Awakening samples its picture for an effect:
    /// the drawing and how many rows of memory come before it, which the
    /// copy takes from memory as they are.
    fn texture_over_drawing(&self, texture: &DrawnTexture) -> Option<(usize, u32)> {
        static FROM_MEMORY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FROM_MEMORY.get_or_init(|| std::env::var_os("ZAKURO_NO_PAST_DRAWING").is_some()) {
            return None;
        }
        let kind = Kind::Color(texture.format);
        let tile_rows = 8 * texture.width * kind.bytes();
        let (index, lead) = self.surfaces.iter().enumerate().find_map(|(index, s)| {
            let offset = s.addr.checked_sub(texture.addr).filter(|&offset| offset > 0 && offset % tile_rows == 0)?;
            let lead = offset / tile_rows * 8;
            let fits = s.kind == kind && s.tiled && !s.stale && s.dirty.is_some() && s.width == texture.width && lead < texture.height && texture.height <= lead + s.height;
            fits.then_some((index, lead))
        })?;
        // the rows before it as memory has them, which another drawing there
        // would not be
        let lead_bytes = lead / 8 * tile_rows;
        (!self.surfaces.iter().any(|s| s.dirty_overlaps(texture.addr, lead_bytes))).then_some((index, lead))
    }

    /// the surface a drawn texture is the whole of, as it is, which a draw
    /// into another surface samples in place, upside down, rather than a copy.
    fn whole_surface(&self, texture: &DrawnTexture) -> Option<usize> {
        static COPIED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *COPIED.get_or_init(|| crate::switch("ZAKURO_NO_IN_PLACE", "drawn textures copied, not sampled in place")) {
            return None;
        }
        let (index, row) = self.texture_source(texture)?;
        let s = &self.surfaces[index];
        (texture.format == ColorFormat::Rgba8 && s.kind == Kind::Color(ColorFormat::Rgba8) && row == 0 && s.height == texture.height).then_some(index)
    }

    pub(crate) fn holds(&self, texture: &DrawnTexture) -> bool {
        static FROM_MEMORY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FROM_MEMORY.get_or_init(|| crate::switch("ZAKURO_NO_GPU_TEXTURES", "drawn textures read from memory")) {
            return false;
        }
        self.texture_source(texture).is_some_and(|(index, _)| self.surfaces[index].dirty.is_some()) || self.texture_over_drawing(texture).is_some()
    }

    fn copy_objects(&mut self, texture: &DrawnTexture) -> Result<(([glow::Program; 3], glow::VertexArray), (glow::Texture, glow::Framebuffer)), String> {
        let copiers = match self.copiers {
            Some(copiers) => copiers,
            None if !self.worker.here() => {
                let worker = self.worker.clone();
                wait_from("copy_objects");
                return worker.run(|| self.copy_objects(texture));
            }
            None => {
                disturb();
                let programs = [
                    self.graphics(PACK_VERTEX_SOURCE, COPY_COLOR_SOURCE)?,
                    self.graphics(PACK_VERTEX_SOURCE, COPY_DEPTH_SOURCE)?,
                    self.graphics(PACK_VERTEX_SOURCE, COPY_STENCIL_SOURCE)?,
                ];
                // SAFETY: on the GL thread
                let empty = unsafe { self.gl.create_vertex_array()? };
                self.copiers = Some((programs, empty));
                (programs, empty)
            }
        };
        if let Some(&copy) = self.copies.get(texture) {
            return Ok((copiers, copy));
        }
        let (copy, framebuffer) = (self.texture_name()?, self.framebuffer_name()?);
        let (gl, width, height) = (self.gl, texture.width as i32, texture.height as i32);
        self.worker.post_keeping(move || {
            disturb();
            // SAFETY: on the GL thread, the names are new and the copy is the
            // texture's size
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(copy));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width, height);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(copy), 0);
            }
        });
        self.copies.insert(*texture, (copy, framebuffer));
        Ok((copiers, (copy, framebuffer)))
    }

    fn copy_texture<M: GpuMemory>(&mut self, memory: &mut M, texture: &DrawnTexture) -> Result<glow::Texture, String> {
        if self.texture_source(texture).is_none() {
            if let Some((source, lead)) = self.texture_over_drawing(texture) {
                return self.copy_over_drawing(memory, texture, source, lead);
            }
        }
        let (source, row) = self.texture_source(texture).ok_or("the buffer a texture was drawn into is gone")?;
        let (([color, depth, stencil], empty), (copy, framebuffer)) = self.copy_objects(texture)?;
        let surface = &self.surfaces[source];
        let stamp = (surface.texture, surface.version, row);
        if self.copied.insert(*texture, stamp) == Some(stamp) {
            return Ok(copy);
        }
        let (kind, image) = (surface.kind, surface.texture);
        let base = surface.height as i32 - 1 - row as i32;
        let (width, height) = (texture.width as i32, texture.height as i32);
        let opaque = matches!(texture.format, ColorFormat::Rgb8 | ColorFormat::Rgb565) as i32;
        let levels = match texture.format {
            ColorFormat::Rgba8 | ColorFormat::Rgb8 => [255.0; 4],
            ColorFormat::Rgb565 => [31.0, 63.0, 31.0, 255.0],
            ColorFormat::Rgb5A1 => [31.0, 31.0, 31.0, 1.0],
            ColorFormat::Rgba4 => [15.0; 4],
        };
        let gl = self.gl;
        self.worker.post_keeping(move || {
            flush_clears(Some(image));
            disturb();
            // SAFETY: on the GL thread, the copy is the texture's size and
            // only rows of the surface are read
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                // the copy is drawn whole, what it held needn't be loaded
                gl.invalidate_framebuffer(glow::FRAMEBUFFER, &[glow::COLOR_ATTACHMENT0]);
                gl.viewport(0, 0, width, height);
                gl.disable(glow::SCISSOR_TEST);
                gl.disable(glow::BLEND);
                gl.disable(glow::DEPTH_TEST);
                gl.disable(glow::STENCIL_TEST);
                gl.disable(glow::CULL_FACE);
                gl.bind_vertex_array(Some(empty));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_sampler(0, None);
                gl.bind_texture(glow::TEXTURE_2D, Some(image));
                gl.color_mask(true, true, true, true);
                match kind {
                    Kind::Color(_) => {
                        gl.use_program(Some(color));
                        gl.uniform_1_i32(gl.get_uniform_location(color, "source").as_ref(), 0);
                        gl.uniform_1_i32(gl.get_uniform_location(color, "base").as_ref(), base);
                        gl.uniform_1_i32(gl.get_uniform_location(color, "opaque").as_ref(), opaque);
                        gl.uniform_4_f32_slice(gl.get_uniform_location(color, "levels").as_ref(), &levels);
                        gl.draw_arrays(glow::TRIANGLES, 0, 3);
                    }
                    Kind::Depth(bytes) => {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::DEPTH_COMPONENT as i32);
                        gl.use_program(Some(depth));
                        gl.uniform_1_i32(gl.get_uniform_location(depth, "depth").as_ref(), 0);
                        gl.uniform_1_i32(gl.get_uniform_location(depth, "base").as_ref(), base);
                        gl.uniform_1_i32(gl.get_uniform_location(depth, "stencil").as_ref(), (bytes == 4) as i32);
                        gl.draw_arrays(glow::TRIANGLES, 0, 3);
                        if bytes == 4 {
                            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::STENCIL_INDEX as i32);
                            gl.use_program(Some(stencil));
                            gl.uniform_1_i32(gl.get_uniform_location(stencil, "stencil").as_ref(), 0);
                            gl.uniform_1_i32(gl.get_uniform_location(stencil, "base").as_ref(), base);
                            gl.color_mask(true, false, false, false);
                            gl.draw_arrays(glow::TRIANGLES, 0, 3);
                            gl.color_mask(true, true, true, true);
                            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::DEPTH_COMPONENT as i32);
                        }
                    }
                }
            }
        });
        Ok(copy)
    }

    /// the copy of a texture whose first rows are memory and the rest a
    /// drawing: the drawing's rows drawn in on the GPU as copy_texture does,
    /// the rows before them uploaded from memory.
    fn copy_over_drawing<M: GpuMemory>(&mut self, memory: &mut M, texture: &DrawnTexture, source: usize, lead: u32) -> Result<glow::Texture, String> {
        let (([color, _, _], empty), (copy, framebuffer)) = self.copy_objects(texture)?;
        let (width, height) = (texture.width, texture.height);
        let bpp = texture.format.bytes_per_pixel();
        let length = (lead * width) as usize * bpp;
        // the memory rows, compared with what they were rather than hashed,
        // which a byte at a time took a fifth of the GPU thread's time
        let unchanged = match (memory.slice(texture.addr, length), self.over_drawing.get(texture)) {
            (Some(now), Some(before)) => now == &before[..],
            (None, Some(before)) => {
                let mut now = vec![0u8; length];
                memory.read(texture.addr, &mut now);
                now == *before
            }
            (_, None) => false,
        };
        let surface = &self.surfaces[source];
        let stamp = (surface.texture, surface.version, u32::MAX - lead);
        let drawn_again = self.copied.insert(*texture, stamp) != Some(stamp);
        if unchanged && !drawn_again {
            return Ok(copy);
        }
        let mut bytes = vec![0u8; length];
        memory.read(texture.addr, &mut bytes);
        let image = surface.texture;
        // the copy's row y is the texture's row y, the drawing's first row
        // the texture's row lead
        let base = surface.height as i32 - 1 + lead as i32;
        let opaque = matches!(texture.format, ColorFormat::Rgb8 | ColorFormat::Rgb565) as i32;
        let levels = match texture.format {
            ColorFormat::Rgba8 | ColorFormat::Rgb8 => [255.0; 4],
            ColorFormat::Rgb565 => [31.0, 63.0, 31.0, 255.0],
            ColorFormat::Rgb5A1 => [31.0, 31.0, 31.0, 1.0],
            ColorFormat::Rgba4 => [15.0; 4],
        };
        let rows = (!unchanged).then(|| {
            let mut rows = vec![0u8; (lead * width * 4) as usize];
            for y in 0..lead {
                for x in 0..width {
                    let at = pixel_offset(true, x, y, width, bpp as u32);
                    let out = ((y * width + x) * 4) as usize;
                    rows[out..out + 4].copy_from_slice(&texture.format.decode(&bytes[at..at + bpp]));
                }
            }
            rows
        });
        self.over_drawing.insert(*texture, bytes);
        let gl = self.gl;
        self.worker.post_keeping(move || {
            flush_clears(Some(image));
            disturb();
            // SAFETY: on the GL thread, the copy is the texture's size, the
            // drawing's rows are read inside it and the upload covers the
            // rows before them
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                gl.viewport(0, 0, width as i32, height as i32);
                gl.enable(glow::SCISSOR_TEST);
                gl.scissor(0, lead as i32, width as i32, (height - lead) as i32);
                gl.disable(glow::BLEND);
                gl.disable(glow::DEPTH_TEST);
                gl.disable(glow::STENCIL_TEST);
                gl.disable(glow::CULL_FACE);
                gl.bind_vertex_array(Some(empty));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_sampler(0, None);
                gl.bind_texture(glow::TEXTURE_2D, Some(image));
                gl.color_mask(true, true, true, true);
                gl.use_program(Some(color));
                gl.uniform_1_i32(gl.get_uniform_location(color, "source").as_ref(), 0);
                gl.uniform_1_i32(gl.get_uniform_location(color, "base").as_ref(), base);
                gl.uniform_1_i32(gl.get_uniform_location(color, "opaque").as_ref(), opaque);
                gl.uniform_4_f32_slice(gl.get_uniform_location(color, "levels").as_ref(), &levels);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                gl.disable(glow::SCISSOR_TEST);
                if let Some(rows) = &rows {
                    gl.bind_texture(glow::TEXTURE_2D, Some(copy));
                    gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width as i32, lead as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(rows)));
                }
            }
        });
        Ok(copy)
    }

    /// a texture copy of whole rows of tiles of a picture the GPU drew and
    /// has not written to memory, done on the GPU as a display transfer that
    /// keeps the layout and format: done on the CPU, the picture would have
    /// to be read back first, which stalls the GPU every time. false leaves
    /// it to the CPU. widths and skips are in bytes, as the copy gives them.
    pub(crate) fn texture_copy<M: GpuMemory>(&mut self, memory: &mut M, input: u32, output: u32, size: u32, input_lines: (u32, u32), output_lines: (u32, u32)) -> Result<bool, String> {
        let done = self.texture_copy_on_gpu(memory, input, output, size, input_lines, output_lines)?;
        if !done {
            let details = format!("to {output:08X}, lines of (bytes, skip) {input_lines:?} into {output_lines:?}");
            left_to_cpu("a texture copy", input, size, details, &self.surfaces);
        }
        Ok(done)
    }

    fn texture_copy_on_gpu<M: GpuMemory>(&mut self, memory: &mut M, input: u32, output: u32, size: u32, input_lines: (u32, u32), output_lines: (u32, u32)) -> Result<bool, String> {
        static CPU: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // Drawn texture copies set to Memory in the menu leaves these to the
        // CPU too, the way out if one goes wrong
        let cpu = || std::env::var_os("ZAKURO_NO_GPU_TEXTURES").is_some() || crate::switch("ZAKURO_CPU_TEXTURE_COPIES", "texture copies on the CPU");
        if *CPU.get_or_init(cpu) {
            return Ok(false);
        }
        if size == 0 {
            return Ok(false);
        }
        let Some((addr, width, column, copy, output_width, format)) = self.surfaces.iter().find_map(|s| {
            let Kind::Color(format) = s.kind else { return None };
            if !s.tiled || s.dirty.is_none() {
                return None;
            }
            let bpp = s.kind.bytes();
            let (tile, tile_row) = (64 * bpp, 8 * s.row_bytes());
            let offset = input.checked_sub(s.addr)?;
            if offset + size > s.size() || !offset.is_multiple_of(tile) {
                return None;
            }
            // a line of the input is whole rows of tiles, or the same few
            // tiles of each row of tiles, part of the picture's width
            let line = match input_lines {
                (0, _) | (_, 0) => tile_row,
                (bytes, skip) if bytes + skip == tile_row => bytes,
                _ => return None,
            };
            let start = offset % tile_row;
            if !line.is_multiple_of(tile) || !size.is_multiple_of(line) || start + line > tile_row || (line == tile_row && start != 0) {
                return None;
            }
            let (row, column) = (offset / tile_row * 8, start / tile * 8);
            // the output one run of bytes, a texture just as wide, or lines
            // as long as the input's with whole tiles between them, the
            // left of a wider texture's rows of tiles
            let output_width = match output_lines {
                (0, _) | (_, 0) => line / bpp / 8,
                (bytes, skip) if bytes == line && skip.is_multiple_of(tile) => (bytes + skip) / bpp / 8,
                _ => return None,
            };
            Some((s.addr + row / 8 * tile_row, s.width, column, (line / bpp / 8, size / line * 8), output_width, format))
        }) else {
            return Ok(false);
        };
        let transfer = Transfer {
            input: addr,
            output,
            input_width: width,
            input_height: copy.1,
            output_width,
            output_height: copy.1,
            copy,
            scale: (1, 1),
            flip: false,
            input_linear: false,
            output_tiled: true,
            input_format: format,
            output_format: format,
            column,
        };
        self.display_transfer_on_gpu(memory, &transfer)
    }

    pub(crate) fn display_transfer<M: GpuMemory>(&mut self, memory: &mut M, transfer: &Transfer) -> Result<bool, String> {
        let done = self.display_transfer_on_gpu(memory, transfer)?;
        if !done {
            let t = transfer;
            let len = t.input_width * t.input_height * Kind::Color(t.input_format).bytes();
            let details = format!(
                "{}x{} {:?} {} to {:08X} {}x{} {:?} {}, copying {:?} scaled {:?}{}",
                t.input_width,
                t.input_height,
                t.input_format,
                if t.input_linear { "linear" } else { "tiled" },
                t.output,
                t.output_width,
                t.output_height,
                t.output_format,
                if t.output_tiled { "tiled" } else { "linear" },
                t.copy,
                t.scale,
                if t.flip { " flipped" } else { "" }
            );
            left_to_cpu("a display transfer", t.input, len, details, &self.surfaces);
        }
        Ok(done)
    }

    fn display_transfer_on_gpu<M: GpuMemory>(&mut self, memory: &mut M, transfer: &Transfer) -> Result<bool, String> {
        let _timed = timed(0);
        self.pictures += 1;
        let t = transfer;
        let input_kind = Kind::Color(t.input_format);
        let output_kind = Kind::Color(t.output_format);
        static CPU: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if t.input_linear || t.copy.0 == 0 || t.copy.1 == 0 || *CPU.get_or_init(|| crate::switch("ZAKURO_CPU_TRANSFERS", "display transfers on the CPU")) {
            return Ok(false);
        }
        let rows = t.copy.1 * t.scale.1;
        let first = if t.flip { t.input_height - rows } else { 0 };
        let tile_rows = 8 * t.input_width * input_kind.bytes();
        let row_in = |s: &Surface| {
            let offset = t.input.checked_sub(s.addr).filter(|offset| offset % tile_rows == 0)? / tile_rows * 8;
            (offset + first + rows <= s.height).then_some(offset)
        };
        let Some((addr, input_size, row)) = self
            .surfaces
            .iter()
            .filter(|s| s.kind == input_kind && s.tiled && s.width == t.input_width)
            .filter_map(|s| Some((s, row_in(s)?)))
            .max_by_key(|(s, _)| (s.dirty.is_some(), s.height))
            .map(|(s, row)| (s.addr, (s.width, s.height), row))
        else {
            return self.transfer_below_memory(memory, t);
        };
        let input_end = addr as u64 + (input_size.0 * input_size.1 * input_kind.bytes()) as u64;
        let output_end = t.output as u64 + (t.output_width * t.output_height * output_kind.bytes()) as u64;
        if (addr as u64) < output_end && (t.output as u64) < input_end {
            return Ok(false);
        }
        self.current()?;
        let (source, target) = loop {
            let source = self.surface_rows(memory, addr, input_size, input_kind, true, None)?;
            let target = self.surface_rows(memory, t.output, (t.output_width, t.output_height), output_kind, t.output_tiled, None)?;
            if self.surfaces[source].checked && self.surfaces[target].checked {
                break (source, target);
            }
        };
        let program = match self.transfer {
            Some(program) => program,
            None => {
                let worker = self.worker.clone();
                let program = worker.run(|| self.compute(TRANSFER_SOURCE))?;
                self.transfer = Some(program);
                program
            }
        };
        let gl = self.gl;
        let t = t.clone();
        let (source_texture, target_texture) = (self.surfaces[source].texture, self.surfaces[target].texture);
        self.worker.post(move || unsafe {
            disturb();
            // SAFETY: on the GL thread, both textures have immutable rgba8
            // storage and the dispatch stays inside them
            gl.use_program(Some(program));
            let int = |name: &str, value: i32| gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), value);
            let pair = |name: &str, x: u32, y: u32| gl.uniform_2_i32(gl.get_uniform_location(program, name).as_ref(), x as i32, y as i32);
            pair("size", t.copy.0, t.copy.1);
            pair("scale", t.scale.0, t.scale.1);
            int("input_height", t.input_height as i32);
            int("output_height", t.output_height as i32);
            int("flip", t.flip as i32);
            int("input_format", format_index(t.input_format));
            int("output_format", format_index(t.output_format));
            int("source_row", row as i32);
            int("source_column", t.column as i32);
            int("target_row", 0);
            int("source_height", input_size.1 as i32);
            gl.bind_image_texture(0, Some(source_texture), 0, false, 0, glow::READ_ONLY, glow::RGBA8);
            gl.bind_image_texture(1, Some(target_texture), 0, false, 0, glow::WRITE_ONLY, glow::RGBA8);
            gl.dispatch_compute(t.copy.0.div_ceil(8), t.copy.1.div_ceil(8), 1);
            gl.memory_barrier(glow::ALL_BARRIER_BITS);
        });
        let target = &mut self.surfaces[target];
        target.drew((0, target.height));
        target.screen_batch += 1;
        self.end_batch();
        Ok(true)
    }

    /// a display transfer whose input starts with rows nothing drew and
    /// goes on into a drawing to its end, as Fire Emblem Awakening shrinks
    /// its picture for an effect: the rows from memory and the drawing each
    /// go through the transfer on the GPU, into their parts of the output,
    /// rather than the drawing being read back for the CPU.
    fn transfer_below_memory<M: GpuMemory>(&mut self, memory: &mut M, t: &Transfer) -> Result<bool, String> {
        if t.flip || t.input_linear || t.column != 0 {
            return Ok(false);
        }
        let (input_kind, output_kind) = (Kind::Color(t.input_format), Kind::Color(t.output_format));
        let tile_rows = 8 * t.input_width * input_kind.bytes();
        let rows = t.copy.1 * t.scale.1;
        let Some((addr, size, lead)) = self.surfaces.iter().find_map(|s| {
            let offset = s.addr.checked_sub(t.input).filter(|&offset| offset > 0 && offset % tile_rows == 0)?;
            let lead = offset / tile_rows * 8;
            let fits = s.kind == input_kind && s.tiled && s.width == t.input_width && s.dirty.is_some() && lead < rows && lead.is_multiple_of(t.scale.1) && rows <= lead + s.height;
            fits.then_some((s.addr, (s.width, s.height), lead))
        }) else {
            return Ok(false);
        };
        // the rows before the drawing come from memory as they are, which
        // another drawing there would make a read back again
        let lead_bytes = lead / 8 * tile_rows;
        if self.surfaces.iter().any(|s| s.dirty_overlaps(t.input, lead_bytes)) {
            return Ok(false);
        }
        let input_end = addr as u64 + (size.0 * size.1 * input_kind.bytes()) as u64;
        let output_end = t.output as u64 + (t.output_width * t.output_height * output_kind.bytes()) as u64;
        if (t.input as u64) < output_end && (t.output as u64) < input_end {
            return Ok(false);
        }
        self.current()?;
        let (before, drawing, target) = loop {
            let before = self.surface_rows(memory, t.input, (t.input_width, lead), input_kind, true, None)?;
            let drawing = self.surface_rows(memory, addr, size, input_kind, true, None)?;
            let target = self.surface_rows(memory, t.output, (t.output_width, t.output_height), output_kind, t.output_tiled, None)?;
            if [before, drawing, target].iter().all(|&i| self.surfaces[i].checked) {
                break (before, drawing, target);
            }
        };
        let program = match self.transfer {
            Some(program) => program,
            None => {
                let worker = self.worker.clone();
                let program = worker.run(|| self.compute(TRANSFER_SOURCE))?;
                self.transfer = Some(program);
                program
            }
        };
        let gl = self.gl;
        let t = t.clone();
        let split = lead / t.scale.1;
        // each part: its source, the source's height, and the output rows
        // it fills, from and how many
        let parts = [
            (self.surfaces[before].texture, lead, 0, split),
            (self.surfaces[drawing].texture, size.1, split, t.copy.1 - split),
        ];
        let target_texture = self.surfaces[target].texture;
        self.worker.post(move || unsafe {
            disturb();
            // SAFETY: on the GL thread, every texture has immutable rgba8
            // storage and each dispatch stays inside them
            gl.use_program(Some(program));
            let int = |name: &str, value: i32| gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), value);
            let pair = |name: &str, x: u32, y: u32| gl.uniform_2_i32(gl.get_uniform_location(program, name).as_ref(), x as i32, y as i32);
            pair("scale", t.scale.0, t.scale.1);
            int("input_height", t.input_height as i32);
            int("output_height", t.output_height as i32);
            int("flip", 0);
            int("input_format", format_index(t.input_format));
            int("output_format", format_index(t.output_format));
            int("source_row", 0);
            int("source_column", 0);
            gl.bind_image_texture(1, Some(target_texture), 0, false, 0, glow::WRITE_ONLY, glow::RGBA8);
            for (source, height, first, count) in parts {
                if count == 0 {
                    continue;
                }
                pair("size", t.copy.0, count);
                int("source_height", height as i32);
                int("target_row", first as i32);
                gl.bind_image_texture(0, Some(source), 0, false, 0, glow::READ_ONLY, glow::RGBA8);
                gl.dispatch_compute(t.copy.0.div_ceil(8), count.div_ceil(8), 1);
                gl.memory_barrier(glow::ALL_BARRIER_BITS);
            }
        });
        let target = &mut self.surfaces[target];
        target.drew((0, target.height));
        target.screen_batch += 1;
        self.end_batch();
        Ok(true)
    }

    fn compute(&self, source: &str) -> Result<glow::Program, String> {
        let gl = &self.gl;
        let _compiling = COMPILER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: the context is current, the shader is deleted once linked
        unsafe {
            let program = gl.create_program()?;
            let shader = gl.create_shader(glow::COMPUTE_SHADER)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(format!("could not compile a compute shader, {}", gl.get_shader_info_log(shader)));
            }
            gl.attach_shader(program, shader);
            gl.link_program(program);
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
            if !gl.get_program_link_status(program) {
                return Err(format!("could not link a compute program, {}", gl.get_program_info_log(program)));
            }
            Ok(program)
        }
    }

    pub(crate) fn screen(&mut self, addr: u32, (width, height): (u32, u32), stride: u32, format: ColorFormat, guest: &[u8]) -> Option<ScreenRef> {
        let row_bytes = stride * Kind::Color(format).bytes();
        let found = self.surfaces.iter().position(|s| {
            s.kind == Kind::Color(format)
                && !s.tiled
                && s.width == stride
                && stride >= width
                && addr >= s.addr
                && (addr - s.addr).is_multiple_of(row_bytes)
                && (addr - s.addr) / row_bytes + height <= s.height
                && s.screen_batch > 0
        })?;
        let s = &self.surfaces[found];
        let first = (addr - s.addr) / row_bytes;
        let shown = s.shadow.get((first * row_bytes) as usize..((first + height) * row_bytes) as usize);
        if s.dirty.is_none() && !shown.is_some_and(|shown| same(guest, shown)) {
            return None;
        }
        Some(ScreenRef { addr: s.addr, size: (s.width, s.height), format, batch: s.screen_batch, rows: (first, height), columns: width })
    }

    pub(crate) fn picture(&mut self, screen: ScreenRef) -> Result<Option<Picture>, String> {
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from("picture");
            return worker.run(|| self.picture(screen));
        }
        let _timed = timed(1);
        let (width, height) = screen.size;
        let Some(index) = self.surfaces.iter().position(|s| {
            s.addr == screen.addr && s.width == width && s.height == height && s.kind == Kind::Color(screen.format) && !s.tiled
        }) else {
            return Ok(None);
        };
        if let Some((batch, image)) = &self.surfaces[index].picture {
            if *batch == screen.batch {
                return Ok(Some((crop(image.clone(), screen), 1)));
            }
        }
        if self.surfaces[index].screen_batch != screen.batch {
            return Ok(None);
        }
        self.current()?;
        let program = match self.upright {
            Some(program) => program,
            None => {
                let program = self.compute(UPRIGHT_SOURCE)?;
                self.upright = Some(program);
                program
            }
        };
        let (up_width, up_height) = (height, width);
        disturb();
        let gl = &self.gl;
        let surface = &mut self.surfaces[index];
        let mut pixels = vec![0u8; (up_width * up_height * 4) as usize];
        // SAFETY: the context is current, the upright texture is made the
        // size the dispatch and the read cover
        unsafe {
            let (texture, framebuffer) = match surface.upright {
                Some(made) => made,
                None => {
                    let texture = gl.create_texture()?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, up_width as i32, up_height as i32);
                    let framebuffer = gl.create_framebuffer()?;
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                    gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
                    surface.upright = Some((texture, framebuffer));
                    (texture, framebuffer)
                }
            };
            gl.use_program(Some(program));
            gl.uniform_2_i32(gl.get_uniform_location(program, "size").as_ref(), up_width as i32, up_height as i32);
            gl.bind_image_texture(0, Some(surface.texture), 0, false, 0, glow::READ_ONLY, glow::RGBA8);
            gl.bind_image_texture(1, Some(texture), 0, false, 0, glow::WRITE_ONLY, glow::RGBA8);
            gl.dispatch_compute(up_width.div_ceil(8), up_height.div_ceil(8), 1);
            gl.memory_barrier(glow::ALL_BARRIER_BITS);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.read_pixels(0, 0, up_width as i32, up_height as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut pixels)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        }
        let image = Arc::new(pixels);
        surface.picture = Some((screen.batch, image.clone()));
        Ok(Some((crop(image, screen), 1)))
    }

    fn current(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// the program for a draw, none while the compiler thread is still
    /// making it, unless wait.
    fn program(&mut self, constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32], vertex: u64, wait: bool) -> Result<Option<glow::Program>, String> {
        let key = (*constants, writes_depth, lighting.to_vec(), vertex);
        if let Some(&program) = self.programs.get(&key) {
            return Ok(Some(program));
        }
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from("program");
            return worker.run(|| self.program(constants, writes_depth, lighting, vertex, wait));
        }
        self.take_compiled(false);
        if let Some(&program) = self.programs.get(&key) {
            return Ok(Some(program));
        }
        if let Some(compiler) = &self.compiler {
            if let Some(error) = compiler.failed.get(&key) {
                return Err(error.clone());
            }
            if compiler.pending.contains(&key) {
                if !wait {
                    return Ok(None);
                }
                while self.compiler.as_ref().is_some_and(|compiler| compiler.pending.contains(&key)) {
                    self.take_compiled(true);
                }
                return self.programs.get(&key).map(|&program| Some(program)).ok_or_else(|| "the compiler thread stopped".to_owned());
            }
        }
        let mut debug_constants = *constants;
        let debug = std::env::var("ZAKURO_FS_DEBUG").unwrap_or_default();
        if debug == "nolight" {
            debug_constants[26] = 0;
        }
        let fragment = if debug == "trivial" {
            "#version 310 es\nprecision highp float;\nlayout(location = 0) in vec4 in_color;\nlayout(location = 0) out vec4 out_color;\nvoid main() { out_color = clamp(in_color, 0.0, 1.0); }\n".to_owned()
        } else {
            fragment_source(&debug_constants, writes_depth, lighting)
        };
        let vertex_source = match vertex {
            0 => VERTEX_SOURCE.to_owned(),
            hash => self.vertex_sources.get(&hash).cloned().ok_or("a vertex program that was never translated")?,
        };
        let compiled = if crate::fast_with(crate::speedup::PLAIN_MATH) { plain_multiplies(&vertex_source) } else { vertex_source };
        let cached = program_cache_path(&self.name, &compiled, &fragment);
        if !wait && !self.preloading {
            if let Some(compiler) = self.compiler.as_mut() {
                compiler.pending.insert(key.clone());
                if compiler.jobs.send(Compile { key, vertex: compiled, fragment, cached }).is_err() {
                    self.compiler = None;
                }
                return Ok(None);
            }
        }
        disturb();
        let program = build_program(&self.gl, &compiled, &fragment, cached.as_deref())?;
        self.programs.insert(key.clone(), program);
        self.note_program(&key);
        Ok(Some(program))
    }

    /// takes up what the compiler thread finished, waiting for one when
    /// block.
    fn take_compiled(&mut self, block: bool) {
        let Some(compiler) = self.compiler.as_mut() else { return };
        let first = if block { compiler.done.recv().ok() } else { compiler.done.try_recv().ok() };
        let Some(first) = first else {
            if block {
                self.compiler = None;
            }
            return;
        };
        let mut finished = vec![first];
        finished.extend(compiler.done.try_iter());
        for (key, result) in finished {
            let compiler = self.compiler.as_mut().expect("the compiler is still there");
            compiler.pending.remove(&key);
            match result {
                Ok(program) => {
                    self.programs.insert(key.clone(), program);
                    self.note_program(&key);
                }
                Err(error) => {
                    log::warn!("{error}");
                    if key.3 != 0 {
                        for translation in self.translated.values_mut() {
                            if *translation == Some(key.3) {
                                *translation = None;
                            }
                        }
                    }
                    compiler.failed.insert(key, error);
                }
            }
        }
    }

    /// writes a new program the title used into the manifest, for the next
    /// run to prepare at startup.
    fn note_program(&self, key: &ProgramKey) {
        let (constants, writes_depth, lighting, vertex) = (&key.0, key.1, &key.2, key.3);
        if self.preloading || constants[29] != 0 {
            return;
        }
        let Some(path) = manifest_path() else { return };
        let words = std::iter::once(writes_depth as u32).chain(constants.iter().copied()).chain(lighting.iter().copied());
        let line = std::iter::once(format!("{vertex:x}")).chain(words.map(|word| format!("{word:x}"))).collect::<Vec<_>>().join(" ");
        if vertex != 0 {
            let file = vertex_path(&path, vertex);
            if let (false, Some(source)) = (file.exists(), self.vertex_sources.get(&vertex)) {
                let _ = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&file, source));
            }
        }
        let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
            use std::io::Write;
            let new = !path.exists();
            std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut file| {
                if new {
                    writeln!(file, "{MANIFEST_HEADER}")?;
                }
                writeln!(file, "{line}")
            })
        });
        if let Err(error) = written {
            log::warn!("could not add a shader to {}, {error}", path.display());
        }
    }

    fn framebuffer(&mut self, color: Option<usize>, depth: Option<usize>) -> Result<glow::Framebuffer, String> {
        if let Some(&framebuffer) = self.framebuffers.get(&(color, depth)) {
            return Ok(framebuffer);
        }
        let framebuffer = self.framebuffer_name()?;
        let (gl, color_texture, depth_texture) = (self.gl, color.map(|color| self.surfaces[color].texture), depth.map(|depth| self.surfaces[depth].texture));
        self.worker.post_keeping(move || {
            disturb();
            // SAFETY: on the GL thread, the textures are the surfaces'
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                match color_texture {
                    Some(texture) => gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0),
                    None => gl.draw_buffers(&[glow::NONE]),
                }
                if let Some(texture) = depth_texture {
                    gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::DEPTH_STENCIL_ATTACHMENT, glow::TEXTURE_2D, Some(texture), 0);
                }
                let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    log::error!("framebuffer incomplete, 0x{status:X}");
                }
            }
        });
        self.framebuffers.insert((color, depth), framebuffer);
        Ok(framebuffer)
    }

    fn texture_name(&mut self) -> Result<glow::Texture, String> {
        // SAFETY: on the GL thread, which makes the names
        pooled(&self.worker, self.gl, (&mut self.texture_names, &mut self.names_coming), |gl| unsafe { gl.create_texture() }.ok(), "texture")
    }

    fn framebuffer_name(&mut self) -> Result<glow::Framebuffer, String> {
        // SAFETY: on the GL thread, which makes the names
        pooled(&self.worker, self.gl, (&mut self.framebuffer_names, &mut self.framebuffers_coming), |gl| unsafe { gl.create_framebuffer() }.ok(), "framebuffer")
    }

    fn taller(&self, addr: u32, kind: Kind, width: u32, height: u32) -> u32 {
        self.surfaces
            .iter()
            .filter(|s| s.addr == addr && s.kind == kind && s.tiled && s.width == width)
            .map(|s| s.height)
            .fold(height, u32::max)
    }

    fn surface_rows<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        addr: u32,
        (width, height): (u32, u32),
        kind: Kind,
        tiled: bool,
        rows: Option<(u32, u32)>,
    ) -> Result<usize, String> {
        let _timed = timed(10);
        let found = self
            .surfaces
            .iter()
            .position(|s| s.addr == addr && s.width == width && s.height == height && s.kind == kind && s.tiled == tiled);
        if found.is_none_or(|index| !self.surfaces[index].checked) && !self.worker.here() {
            if let Some(index) = found {
                let size = self.surfaces[index].size();
                let row = self.surfaces[index].row_bytes();
                let (from, to) = rows.map_or((addr, size), |(start, end)| (addr + start * row, (end - start) * row));
                let others = (0..self.surfaces.len()).any(|i| i != index && self.surfaces[i].dirty_overlaps(from, to));
                if !others {
                    let same = match memory.slice(addr, size as usize) {
                        Some(bytes) => same(bytes, &self.surfaces[index].shadow),
                        None => {
                            let mut bytes = vec![0; size as usize];
                            memory.read(addr, &mut bytes);
                            bytes == self.surfaces[index].shadow
                        }
                    };
                    if same {
                        self.surfaces[index].checked = true;
                        self.surfaces[index].stale = false;
                        return Ok(index);
                    }
                }
            }
            let worker = self.worker.clone();
            return worker.run(|| self.surface_rows(memory, addr, (width, height), kind, tiled, rows));
        }
        let index = match found {
            Some(index) => index,
            None => {
                let gl = &self.gl;
                // SAFETY: the context is current
                let texture = unsafe {
                    let texture = gl.create_texture()?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    let format = match kind {
                        Kind::Color(_) => glow::RGBA8,
                        Kind::Depth(_) => glow::DEPTH24_STENCIL8,
                    };
                    gl.tex_storage_2d(glow::TEXTURE_2D, 1, format, width as i32, height as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                    texture
                };
                self.surfaces.push(Surface {
                    texture,
                    addr,
                    width,
                    height,
                    kind,
                    tiled,
                    shadow: Vec::new(),
                    checked: false,
                    dirty: None,
                    screen_batch: 0,
                    picture: None,
                    upright: None,
                    packed: None,
                    version: next_version(),
                    stale: false,
                });
                self.surfaces.len() - 1
            }
        };
        if self.surfaces[index].checked {
            return Ok(index);
        }
        let size = self.surfaces[index].size();
        let row = self.surfaces[index].row_bytes();
        let (from, to) = rows.map_or((addr, size), |(start, end)| (addr + start * row, (end - start) * row));
        let mut others = Vec::new();
        for other in (0..self.surfaces.len()).filter(|&i| i != index && self.surfaces[i].dirty_overlaps(from, to)).collect::<Vec<_>>() {
            if !self.adopt(index, other)? {
                others.push(other);
            }
        }
        if !others.is_empty() {
            self.write_back(memory, others)?;
        }
        if memory.slice(addr, size as usize).is_some_and(|bytes| same(bytes, &self.surfaces[index].shadow)) {
            self.surfaces[index].checked = true;
            self.surfaces[index].stale = false;
            return Ok(index);
        }
        let mut bytes = vec![0; size as usize];
        memory.read(addr, &mut bytes);
        if bytes != self.surfaces[index].shadow {
            self.upload(index, &bytes);
            self.surfaces[index].shadow = bytes;
            self.surfaces[index].picture = None;
        }
        self.surfaces[index].checked = true;
        self.surfaces[index].stale = false;
        Ok(index)
    }

    /// tiles another surface drew over this one's memory come over on the
    /// GPU, with the drawing, rather than through memory, when whole rows of
    /// tiles of both line up.
    fn adopt(&mut self, index: usize, other: usize) -> Result<bool, String> {
        let (s, o) = (&self.surfaces[index], &self.surfaces[other]);
        if s.kind != o.kind || !matches!(s.kind, Kind::Depth(_)) || !s.tiled || !o.tiled || !crate::fast_with(crate::speedup::TILES) || s.width % 8 != 0 || o.width % 8 != 0 || s.height % 8 != 0 || o.height % 8 != 0 {
            return Ok(false);
        }
        let (across, other_across) = ((s.width / 8) as i64, (o.width / 8) as i64);
        if across % other_across != 0 && other_across % across != 0 {
            return Ok(false);
        }
        let Some((first, last)) = o.dirty else { return Ok(false) };
        let tile = 64 * s.kind.bytes() as i64;
        let offset = s.addr as i64 - o.addr as i64;
        if offset % tile != 0 || first % 8 != 0 || last % 8 != 0 {
            return Ok(false);
        }
        let offset = offset / tile;
        let (start, end) = ((first / 8) as i64 * other_across, (last / 8) as i64 * other_across);
        let (start, end) = (start.max(offset), end.min(offset + (s.height / 8) as i64 * across));
        if start >= end || start % other_across != 0 || end % other_across != 0 || (start - offset) % across != 0 || (end - offset) % across != 0 {
            return Ok(false);
        }
        let (gone_from, gone_to) = ((start / other_across * 8) as u32, (end / other_across * 8) as u32);
        let left = match first < gone_from {
            true if gone_to < last => return Ok(false),
            true => Some((first, gone_from)),
            false if gone_to < last => Some((gone_to, last)),
            false => None,
        };
        let (from, to) = (((start - offset) / across * 8) as u32, ((end - offset) / across * 8) as u32);
        let merged = match s.dirty {
            None => (from, to),
            Some((a, b)) if b >= from && to >= a => (a.min(from), b.max(to)),
            Some(_) => return Ok(false),
        };
        let (kind, other_height, height) = (s.kind, o.height as i64, s.height as i64);
        let (source, target, mask) = match kind {
            Kind::Color(_) => (self.framebuffer(Some(other), None)?, self.framebuffer(Some(index), None)?, glow::COLOR_BUFFER_BIT),
            Kind::Depth(_) => (self.framebuffer(None, Some(other))?, self.framebuffer(None, Some(index))?, glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT),
        };
        disturb();
        let gl = &self.gl;
        // SAFETY: on the GL thread, every tile copied lies inside both
        // surfaces
        unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(target));
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.depth_mask(true);
            gl.stencil_mask(0xFF);
            let blit = |(x, y): (i64, i64), (to_x, to_y): (i64, i64), (wide, high): (i64, i64)| {
                gl.blit_framebuffer(
                    x as i32,
                    (other_height - y - high) as i32,
                    (x + wide) as i32,
                    (other_height - y) as i32,
                    to_x as i32,
                    (height - to_y - high) as i32,
                    (to_x + wide) as i32,
                    (height - to_y) as i32,
                    mask,
                    glow::NEAREST,
                )
            };
            if across == other_across {
                blit((0, start / across * 8), (0, (start - offset) / across * 8), (across * 8, (end - start) / across * 8));
            } else {
                let segment = across.min(other_across);
                for at in (start..end).step_by(segment as usize) {
                    let target = at - offset;
                    blit((at % other_across * 8, at / other_across * 8), (target % across * 8, target / across * 8), (segment * 8, 8));
                }
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        let o = &mut self.surfaces[other];
        o.dirty = left;
        o.checked = false;
        o.stale = true;
        o.shadow = Vec::new();
        o.picture = None;
        let s = &mut self.surfaces[index];
        s.dirty = Some(merged);
        s.picture = None;
        s.version = next_version();
        Ok(true)
    }

    fn upload(&mut self, index: usize, bytes: &[u8]) {
        let _timed = timed(2);
        self.surfaces[index].version = next_version();
        let s = &self.surfaces[index];
        let (width, height, kind, tiled, texture) = (s.width, s.height, s.kind, s.tiled, s.texture);
        let pixels = (width * height) as usize;
        let ranges: Vec<(u32, u32)> = match s.dirty {
            Some((start, end)) => [(0, start), (end, height)].into_iter().filter(|(from, to)| from < to).collect(),
            None => vec![(0, height)],
        };
        let rows = || ranges.iter().flat_map(|&(from, to)| from..to);
        let part = |staging: &[u8], (from, to): (u32, u32)| {
            let (top, count) = ((height - to) as i32, (to - from) as i32);
            (top, count, staging[(height - to) as usize * width as usize * 4..(height - from) as usize * width as usize * 4].to_vec())
        };
        disturb();
        let gl = &self.gl;
        match kind {
            Kind::Color(format) => {
                let bpp = format.bytes_per_pixel();
                let mut staging = vec![0u8; pixels * 4];
                for y in rows() {
                    for x in 0..width {
                        let at = pixel_offset(tiled, x, y, width, bpp as u32);
                        let out = (((height - 1 - y) * width + x) * 4) as usize;
                        staging[out..out + 4].copy_from_slice(&format.decode(&bytes[at..at + bpp]));
                    }
                }
                // SAFETY: the context is current and the staging covers the texture
                unsafe {
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    for &range in &ranges {
                        let (top, count, rows) = part(&staging, range);
                        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, top, width as i32, count, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&rows)));
                    }
                }
            }
            Kind::Depth(sample) => {
                let mut staging = vec![0u8; pixels * 4];
                for y in rows() {
                    for x in 0..width {
                        let at = morton_offset(x, y, width, sample) as usize;
                        let (depth, stencil) = match sample {
                            2 => {
                                let d16 = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u64;
                                (((d16 * 0xFF_FFFF + 0x7FFF) / 0xFFFF) as u32, 0)
                            }
                            3 => (u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], 0]), 0),
                            _ => {
                                let word = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
                                (word & 0xFF_FFFF, word >> 24)
                            }
                        };
                        let out = (((height - 1 - y) * width + x) * 4) as usize;
                        staging[out..out + 4].copy_from_slice(&(depth << 8 | stencil).to_ne_bytes());
                    }
                }
                // SAFETY: as above
                unsafe {
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    for &range in &ranges {
                        let (top, count, rows) = part(&staging, range);
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            0,
                            top,
                            width as i32,
                            count,
                            glow::DEPTH_STENCIL,
                            glow::UNSIGNED_INT_24_8,
                            glow::PixelUnpackData::Slice(Some(&rows)),
                        );
                    }
                }
            }
        }
    }

    fn clear(&mut self, index: usize, pixel: &[u8]) -> Result<(), String> {
        let _timed = timed(3);
        self.surfaces[index].version = next_version();
        let kind = self.surfaces[index].kind;
        let framebuffer = match kind {
            Kind::Color(_) => self.framebuffer(Some(index), None)?,
            Kind::Depth(_) => self.framebuffer(None, Some(index))?,
        };
        let (texture, pixel) = (self.surfaces[index].texture, pixel.to_vec());
        let gl = self.gl;
        let deferred = !self.worker.here();
        self.worker.post_keeping(move || {
            local(|local| {
                local.clears.retain(|clear| clear.texture != texture);
                if deferred {
                    local.clears.push(Clear { texture, framebuffer, kind, pixel });
                    return;
                }
                local.disturbed = true;
                // SAFETY: on the GL thread, the framebuffer holds the surface
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                    clear_bound(&gl, kind, &pixel);
                }
            })
        });
        Ok(())
    }

    pub(crate) fn filled(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        let _timed = timed(11);
        self.current()?;
        let end = addr as u64 + bytes.len() as u64;
        for index in 0..self.surfaces.len() {
            let surface = &self.surfaces[index];
            if !surface.overlaps(addr, bytes.len() as u32) {
                continue;
            }
            let size = surface.size() as usize;
            if surface.addr < addr || surface.addr as u64 + size as u64 > end {
                self.surfaces[index].checked = false;
                continue;
            }
            let offset = (surface.addr - addr) as usize;
            let filled = &bytes[offset..offset + size];
            let bpp = surface.kind.bytes() as usize;
            let uniform = filled.iter().enumerate().take(12).all(|(i, &b)| b == filled[i % bpp]);
            if !uniform {
                let surface = &mut self.surfaces[index];
                surface.dirty = None;
                surface.checked = false;
                continue;
            }
            let pixel = filled[..bpp].to_vec();
            self.clear(index, &pixel)?;
            let surface = &mut self.surfaces[index];
            surface.shadow.clear();
            surface.shadow.extend_from_slice(&bytes[offset..offset + size]);
            surface.dirty = None;
            surface.checked = false;
            surface.picture = None;
        }
        Ok(())
    }

    /// a fill of exactly a surface with one value, in fast mode, clears it
    /// on the GPU alone and leaves the memory to its next write back.
    pub(crate) fn fill_only(&mut self, addr: u32, len: u32, pattern: &[u8]) -> Result<bool, String> {
        if !crate::fast_with(crate::speedup::CLEARS) {
            return Ok(false);
        }
        let Some(index) = self.surfaces.iter().position(|s| s.addr == addr && s.size() == len) else { return Ok(false) };
        let bpp = self.surfaces[index].kind.bytes() as usize;
        if !bpp.is_multiple_of(pattern.len()) {
            return Ok(false);
        }
        let pixel: Vec<u8> = pattern.iter().copied().cycle().take(bpp).collect();
        self.current()?;
        self.clear(index, &pixel)?;
        let surface = &mut self.surfaces[index];
        surface.dirty = Some((0, surface.height));
        surface.checked = false;
        surface.picture = None;
        // what the surfaces inside the fill drew before it is gone
        let end = addr as u64 + len as u64;
        for (other, s) in self.surfaces.iter_mut().enumerate() {
            if other != index && s.dirty.is_some() && s.addr >= addr && s.addr as u64 + s.size() as u64 <= end {
                (s.dirty, s.picture) = (None, None);
            }
        }
        Ok(true)
    }

    pub(crate) fn prepare_read<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32, texture: Option<&crate::raster::DrawnTexture>) -> Result<(), String> {
        if self.surfaces.iter().any(|s| s.dirty_overlaps(addr, len)) {
            let details = match texture {
                Some(t) => format!("a {}x{} {:?} texture", t.width, t.height, t.format),
                None => "a texture in a format no drawing has".to_owned(),
            };
            left_to_cpu("a texture read", addr, len, details, &self.surfaces);
            sync_why("sync to read");
            self.sync(memory, addr, len)?;
        }
        Ok(())
    }

    pub(crate) fn before_fill<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32, _width: u32) -> Result<(), String> {
        let end = addr as u64 + len as u64;
        let (dropped, partial): (Vec<usize>, Vec<usize>) = (0..self.surfaces.len())
            .filter(|&i| {
                let s = &self.surfaces[i];
                s.dirty_overlaps(addr, len) && (s.addr < addr || s.addr as u64 + s.size() as u64 > end)
            })
            .partition(|&i| matches!(self.surfaces[i].kind, Kind::Depth(_)) && crate::fast_with(crate::speedup::CLEARS));
        // a depth buffer a fill runs over in part is another buffer's memory
        // by now, its depth past the fill went with the frame that drew it:
        // Smash clears its bottom screen over the top screen's depth
        for index in dropped {
            let s = &mut self.surfaces[index];
            s.shadow.resize(s.size() as usize, 0);
            memory.read(s.addr, &mut s.shadow);
            (s.dirty, s.checked, s.picture) = (None, false, None);
        }
        if !partial.is_empty() {
            self.write_back(memory, partial)?;
        }
        Ok(())
    }

    pub(crate) fn before_overwrite<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) -> Result<(), String> {
        let end = addr as u64 + len as u64;
        for s in &mut self.surfaces {
            let Some((start, stop)) = s.dirty else { continue };
            let (from, to) = (s.addr as u64 + (start * s.row_bytes()) as u64, s.addr as u64 + (stop * s.row_bytes()) as u64);
            if addr as u64 <= from && to <= end {
                s.dirty = None;
                s.shadow = Vec::new();
                s.picture = None;
            }
        }
        sync_why("sync before overwriting");
        self.sync(memory, addr, len)
    }

    pub(crate) fn flush<M: GpuMemory>(&mut self, memory: &mut M) -> Result<(), String> {
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from("flush");
            return worker.run(|| self.flush(memory));
        }
        let dirty = (0..self.surfaces.len()).filter(|&i| self.surfaces[i].dirty.is_some()).collect();
        self.write_back(memory, dirty)?;
        self.end_batch();
        Ok(())
    }

    fn end_batch(&mut self) {
        for surface in &mut self.surfaces {
            surface.checked = false;
        }
        self.flushes += 1;
        let flushes = self.flushes;
        if flushes.is_multiple_of(60) && std::env::var_os("ZAKURO_GLES_TIMES").is_some() {
            report_times();
        }
        if flushes.is_multiple_of(600) {
            log::debug!("{} surfaces, {} textures, {} programs", self.surfaces.len(), self.textures.len(), self.programs.len());
        }
        let mut gone = Vec::new();
        self.textures.retain(|_, texture| {
            let keep = flushes - texture.used < TEXTURE_KEEP_FLUSHES;
            if !keep {
                gone.push(texture.texture);
            }
            keep
        });
        if !gone.is_empty() {
            let gl = self.gl;
            self.worker.post_keeping(move || {
                disturb();
                for texture in gone {
                    // SAFETY: on the GL thread, after every draw that used it
                    unsafe { gl.delete_texture(texture) };
                }
            });
        }
    }

    pub(crate) fn sync<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) -> Result<(), String> {
        if !self.surfaces.iter().any(|s| s.dirty_overlaps(addr, len)) {
            self.surfaces.iter_mut().filter(|s| s.overlaps(addr, len)).for_each(|s| s.checked = false);
            return Ok(());
        }
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from(SYNC_WHY.with(|cell| cell.replace("sync")));
            return worker.run(|| self.sync(memory, addr, len));
        }
        let overlapping: Vec<usize> = (0..self.surfaces.len()).filter(|&i| self.surfaces[i].overlaps(addr, len)).collect();
        let dirty: Vec<usize> = overlapping.iter().copied().filter(|&i| self.surfaces[i].dirty_overlaps(addr, len)).collect();
        self.write_back(memory, dirty)?;
        for i in overlapping {
            self.surfaces[i].checked = false;
        }
        Ok(())
    }

    fn graphics(&self, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
        link(&self.gl, vertex, fragment)
    }

    pub(crate) fn present(&mut self, shown: [Option<(crate::PanelImage, [i32; 4])>; 2], cursor: Option<(i32, i32)>, overlay: Option<Arc<crate::Overlay>>, smooth: bool, interpolate: bool) {
        let mut parts = Vec::new();
        for (index, item) in shown.into_iter().enumerate() {
            let Some((image, area)) = item else { continue };
            match image {
                crate::PanelImage::Gpu(screen) => {
                    let (width, height) = screen.size;
                    let found = self.surfaces.iter().find(|s| {
                        s.addr == screen.addr && s.width == width && s.height == height && s.kind == Kind::Color(screen.format) && !s.tiled
                    });
                    if let Some(s) = found {
                        let screen = [screen.rows.1 as i32, screen.columns as i32, s.height as i32, screen.rows.0 as i32];
                        let top = index == 0;
                        parts.push(Part::Surface { texture: s.texture, screen, area, smooth: smooth && top, top, picture: (interpolate && top).then_some(s.version) });
                    }
                }
                crate::PanelImage::Pixels(pixels, width, height) => parts.push(Part::Pixels { index, pixels, width, height, area, smooth: smooth && index == 0 }),
            }
        }
        if let Some((x, y)) = cursor {
            parts.push(Part::Cursor { x, y });
        }
        parts.extend(overlay.map(Part::Overlay));
        // with two waiting for the panel already, this one waits its turn:
        // dropped, its frame would have been drawn for nothing, where held
        // up, the title falls behind and skips drawing frames instead
        while self.presenting.load(std::sync::atomic::Ordering::Acquire) >= 2 {
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        self.presenting.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let presenting = self.presenting.clone();
        let gl = self.gl;
        self.worker.post(move || {
            sim_present(&gl);
            if let Err(error) = show(gl, parts) {
                log::error!("could not show the screens, {error}");
            }
            presenting.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        });
    }

    fn read_depth(&mut self, index: usize, data: &mut [u8]) -> Result<(), String> {
        let _timed = timed(4);
        disturb();
        let (depth, stencil, empty) = match self.packers {
            Some(packers) => packers,
            None => {
                let depth = self.graphics(PACK_VERTEX_SOURCE, PACK_DEPTH_SOURCE)?;
                let stencil = self.graphics(PACK_VERTEX_SOURCE, PACK_STENCIL_SOURCE)?;
                // SAFETY: the context is current
                let empty = unsafe { self.gl.create_vertex_array()? };
                self.packers = Some((depth, stencil, empty));
                (depth, stencil, empty)
            }
        };
        let gl = &self.gl;
        let surface = &mut self.surfaces[index];
        let (width, height, texture) = (surface.width as i32, surface.height as i32, surface.texture);
        let with_stencil = surface.kind == Kind::Depth(4);
        // SAFETY: the context is current, the packed texture is the size of
        // the surface and the buffer covers it
        unsafe {
            let (_, framebuffer) = match surface.packed {
                Some(made) => made,
                None => {
                    let packed = gl.create_texture()?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(packed));
                    gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width, height);
                    let framebuffer = gl.create_framebuffer()?;
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                    gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(packed), 0);
                    surface.packed = Some((packed, framebuffer));
                    (packed, framebuffer)
                }
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.viewport(0, 0, width, height);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.bind_vertex_array(Some(empty));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_sampler(0, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.color_mask(true, true, true, true);
            gl.clear_buffer_f32_slice(glow::COLOR, 0, &[0.0; 4]);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::DEPTH_COMPONENT as i32);
            let pending = timed(15);
            gl.finish();
            drop(pending);
            let pass = timed(12);
            gl.use_program(Some(depth));
            gl.uniform_1_i32(gl.get_uniform_location(depth, "depth").as_ref(), 0);
            gl.color_mask(true, true, true, false);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.finish();
            drop(pass);
            if with_stencil {
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::STENCIL_INDEX as i32);
                let pass = timed(13);
                gl.use_program(Some(stencil));
                gl.uniform_1_i32(gl.get_uniform_location(stencil, "stencil").as_ref(), 0);
                gl.color_mask(false, false, false, true);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::DEPTH_STENCIL_TEXTURE_MODE, glow::DEPTH_COMPONENT as i32);
                gl.finish();
                drop(pass);
            }
            let _pass = timed(14);
            gl.color_mask(true, true, true, true);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.read_pixels(0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(data)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        }
        Ok(())
    }

    fn write_back<M: GpuMemory>(&mut self, memory: &mut M, dirty: Vec<usize>) -> Result<(), String> {
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from("write_back");
            return worker.run(|| self.write_back(memory, dirty));
        }
        let _timed = timed(5);
        if dirty.is_empty() {
            return Ok(());
        }
        sim_wait(&self.gl);
        self.current()?;
        for index in dirty {
            let s = &self.surfaces[index];
            let (width, height, tiled, kind) = (s.width, s.height, s.tiled, s.kind);
            let (first, last) = s.dirty.unwrap_or((0, height));
            // memory the CPU wrote since the GPU last looked is newer than the
            // drawing, as when a draw finds it changed, a title that freed the
            // buffer may keep its heap there
            let (addr, size) = (s.addr, s.size() as usize);
            let written = s.shadow.len() == size
                && match memory.slice(addr, size) {
                    Some(now) => now != &s.shadow[..],
                    None => {
                        let mut now = vec![0u8; size];
                        memory.read(addr, &mut now);
                        now != s.shadow
                    }
                };
            if written {
                log::debug!("the CPU wrote over 0x{addr:08X} since it was drawn, the drawing stays out of memory");
                let surface = &mut self.surfaces[index];
                (surface.dirty, surface.checked, surface.picture) = (None, false, None);
                continue;
            }
            let mut data = vec![0u8; (width * height * 4) as usize];
            match kind {
                Kind::Color(_) => {
                    let framebuffer = self.framebuffer(Some(index), None)?;
                    disturb();
                    let gl = &self.gl;
                    // SAFETY: the context is current, the buffer covers the surface
                    unsafe {
                        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
                        gl.read_buffer(glow::COLOR_ATTACHMENT0);
                        gl.read_pixels(0, 0, width as i32, height as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
                        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    }
                }
                Kind::Depth(_) => self.read_depth(index, &mut data)?,
            }
            let s = &self.surfaces[index];
            let bpp = kind.bytes() as usize;
            let mut bytes = vec![0u8; (width * height) as usize * bpp];
            let rows = (first, last);
            match kind {
                Kind::Color(ColorFormat::Rgba8) => store_rows::<4>(&data, &mut bytes, width, height, tiled, rows, |p| [p[3], p[2], p[1], p[0]]),
                Kind::Color(ColorFormat::Rgb8) => store_rows::<3>(&data, &mut bytes, width, height, tiled, rows, |p| [p[2], p[1], p[0]]),
                Kind::Color(format) => store_rows::<2>(&data, &mut bytes, width, height, tiled, rows, |p| {
                    let mut out = [0; 2];
                    format.encode(p, &mut out);
                    out
                }),
                Kind::Depth(sample) => {
                    for y in first..last {
                        for x in 0..width {
                            let at = (((height - 1 - y) * width + x) * 4) as usize;
                            let out = morton_offset(x, y, width, sample) as usize;
                            let d24 = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], 0]);
                            match sample {
                                2 => {
                                    let d16 = ((d24 as u64 * 0xFFFF + 0x7F_FFFF) / 0xFF_FFFF) as u16;
                                    bytes[out..out + 2].copy_from_slice(&d16.to_le_bytes());
                                }
                                3 => bytes[out..out + 3].copy_from_slice(&d24.to_le_bytes()[..3]),
                                _ => bytes[out..out + 4].copy_from_slice(&(d24 | (data[at + 3] as u32) << 24).to_le_bytes()),
                            }
                        }
                    }
                }
            }
            let row = s.row_bytes() as usize;
            memory.write(addr + (first as usize * row) as u32, &bytes[first as usize * row..last as usize * row]);
            // memory holds what was just written, and around it what was there
            if (first, last) != (0, height) {
                memory.read(addr, &mut bytes);
            }
            let surface = &mut self.surfaces[index];
            surface.shadow = bytes;
            surface.dirty = None;
        }
        self.end_batch();
        Ok(())
    }

    fn texture(&mut self, bound: &BoundTexture) -> Result<glow::Texture, String> {
        let _timed = timed(6);
        let key = bound.etc1.as_ref().map_or(Arc::as_ptr(&bound.texels) as *const u8, |blocks| Arc::as_ptr(blocks) as *const u8) as usize;
        if let Some(texture) = self.textures.get_mut(&key) {
            texture.used = self.flushes;
            return Ok(texture.texture);
        }
        // its texels go up after the draws before
        let texture = self.texture_name()?;
        let (gl, texels, etc1, width, height) = (self.gl, bound.texels.clone(), bound.etc1.clone(), bound.width as i32, bound.height as i32);
        self.worker.post_keeping(move || {
            disturb();
            // SAFETY: on the GL thread, the name is new and the texels cover
            // the texture
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                match etc1 {
                    Some(blocks) => {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAX_LEVEL, 0);
                        gl.compressed_tex_image_2d(glow::TEXTURE_2D, 0, ETC1_RGB8_OES as i32, width, height, 0, blocks.len() as i32, &blocks);
                    }
                    None => {
                        gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width, height);
                        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(texels.as_flattened())));
                    }
                }
            }
        });
        self.textures.insert(key, Texture { texture, _texels: (bound.texels.clone(), bound.etc1.clone()), used: self.flushes });
        Ok(texture)
    }

    fn sampler(&mut self, linear: bool, s: Wrap, t: Wrap) -> Result<glow::Sampler, String> {
        if let Some(&sampler) = self.samplers.get(&(linear, s, t)) {
            return Ok(sampler);
        }
        let mode = |wrap: Wrap| match wrap {
            Wrap::Repeat => glow::REPEAT,
            Wrap::MirroredRepeat => glow::MIRRORED_REPEAT,
            _ => glow::CLAMP_TO_EDGE,
        } as i32;
        let filter = if linear { glow::LINEAR } else { glow::NEAREST } as i32;
        // SAFETY: on the GL thread, which makes the names
        let sampler = pooled(&self.worker, self.gl, (&mut self.sampler_names, &mut self.samplers_coming), |gl| unsafe { gl.create_sampler() }.ok(), "sampler")?;
        let (gl, wrap_s, wrap_t) = (self.gl, mode(s), mode(t));
        self.worker.post_keeping(move || {
            // SAFETY: on the GL thread, the name is new
            unsafe {
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_MIN_FILTER, filter);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_MAG_FILTER, filter);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_S, wrap_s);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_T, wrap_t);
            }
        });
        self.samplers.insert((linear, s, t), sampler);
        Ok(sampler)
    }

    fn upload_tables(&mut self, tables: &Tables, procedural: &proctex::Tables) {
        let _timed = timed(7);
        use std::hash::Hasher;
        let generation = (tables.generation(), procedural.generation());
        if self.tables == Some(generation) {
            return;
        }
        self.tables = Some(generation);
        let mut key = [0u64; 25];
        for (table, (changed, hash)) in self.table_hashes.iter_mut().enumerate() {
            if *changed != tables.changed()[table] {
                let mut hasher = rustc_hash::FxHasher::default();
                tables.entries()[table].iter().for_each(|[value, step]| hasher.write_u64((value.to_bits() as u64) << 32 | step.to_bits() as u64));
                *hash = hasher.finish();
                *changed = tables.changed()[table];
            }
            key[table] = *hash;
        }
        key[24] = procedural.generation();
        if self.table_sets.len() > 256 {
            self.table_sets.clear();
        }
        let (id, bytes) = match self.table_sets.get(&key) {
            Some((id, bytes)) => (*id, bytes.clone()),
            None => {
                let (colors, steps) = procedural.colors();
                let parts = [
                    tables.entries().as_flattened().as_flattened(),
                    procedural.maps().as_flattened().as_flattened(),
                    colors.as_flattened(),
                    steps.as_flattened(),
                ];
                let mut bytes = Vec::with_capacity(TABLES_SIZE);
                for part in parts {
                    for value in part {
                        bytes.extend_from_slice(&value.to_le_bytes());
                    }
                }
                bytes.resize(TABLES_SIZE, 0);
                let bytes: Arc<[u8]> = bytes.into();
                self.table_set_ids += 1;
                self.table_sets.insert(key, (self.table_set_ids, bytes.clone()));
                (self.table_set_ids, bytes)
            }
        };
        // the GL thread puts them past what the draws before still read, the
        // draws after bind them there
        let gl = self.gl;
        self.worker.post_keeping(move || {
            local(|local| {
                if let Some(&(_, at)) = local.table_sets.get(&id).filter(|(laps, _)| *laps == local.tables.laps) {
                    local.tables_at = Some(at);
                    return;
                }
                match local.tables.push(&gl, glow::SHADER_STORAGE_BUFFER, &bytes) {
                    Ok(at) => {
                        local.tables_at = Some(at);
                        if local.table_sets.len() > 1024 {
                            local.table_sets.clear();
                        }
                        local.table_sets.insert(id, (local.tables.laps, at));
                    }
                    Err(error) => log::error!("could not upload the lighting tables, {error}"),
                }
            })
        });
    }

    fn triangles(triangles: &[[Screen; 3]], (width, height): (f32, f32), depth_map: DepthMap) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(triangles.len() * 3 * VERTEX_SIZE);
        for v in triangles.iter().flatten() {
            let w = 1.0 / v.inv_w;
            let x = v.x / width * 2.0 - 1.0;
            let y = v.y / height * 2.0 - 1.0;
            let depth = v.z * depth_map.scale + depth_map.offset;
            let (c, t, q, view) = (v.color_over_w, v.texcoords_over_w, v.quaternion_over_w, v.view_over_w);
            let values: [f32; 24] = [
                x * w,
                y * w,
                0.0,
                w,
                c[0] * w,
                c[1] * w,
                c[2] * w,
                c[3] * w,
                t[0][0] * w,
                t[0][1] * w,
                t[1][0] * w,
                t[1][1] * w,
                t[2][0] * w,
                t[2][1] * w,
                depth,
                0.0,
                q[0] * w,
                q[1] * w,
                q[2] * w,
                q[3] * w,
                view[0] * w,
                view[1] * w,
                view[2] * w,
                0.0,
            ];
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }

    pub(super) fn draw<M: GpuMemory>(&mut self, memory: &mut M, draw: &Draw) -> Result<(), String> {
        let _timed = timed(8);
        let empty = match draw.geometry {
            Geometry::Triangles(triangles) => triangles.is_empty(),
            Geometry::Shaded(shading) => shading.indices.is_empty(),
        };
        let [left, bottom, right, top] = draw.scissor;
        let (left, bottom) = (left.max(0), bottom.max(0));
        let (right, top) = (right.min(draw.width as i32), top.min(draw.height as i32));
        if empty || right <= left || top <= bottom {
            return Ok(());
        }
        self.current()?;
        let vertex = match draw.geometry {
            Geometry::Shaded(shading) => self.vertex_stage(shading)?,
            Geometry::Triangles(_) => 0,
        };

        let (cull, count) = match draw.geometry {
            Geometry::Shaded(shading) => (shading.cull, shading.indices.len()),
            Geometry::Triangles(triangles) => (0, triangles.len() * 3),
        };
        let r = draw.registers;
        // ending in the address of the mesh's indices, which tells one
        // mesh of the scenery from another
        let mesh = (r[crate::registers::REG_ATTRIBUTE_BASE] << 3).wrapping_add(r[crate::registers::REG_INDEX_ARRAY] & 0x0FFF_FFFF);
        let signature = || format!("{vertex:016x}/{cull}/{:x}/{:x}/{}x{}/{mesh:x}", r[REG_DEPTH_COLOR_MASK] & 0x1F01, draw.target, draw.width, draw.height);
        let (dropped, thin) = draw_filter(signature, count);
        if dropped || (count <= 6 && drops().depth.iter().any(|prefix| signature().starts_with(prefix.as_str())) && draw.textures.iter().flatten().filter_map(|texture| texture.drawn.as_ref()).any(|drawn| self.texture_source(drawn).is_some_and(|(index, _)| matches!(self.surfaces[index].kind, Kind::Depth(_))))) {
            return Ok(());
        }
        let rows = ((draw.height as i32 - top) as u32, (draw.height as i32 - bottom) as u32);
        let height = self.taller(draw.target, Kind::Color(draw.format), draw.width, draw.height);
        let height = match draw.depth {
            Some((addr, bytes)) if self.taller(addr, Kind::Depth(bytes), draw.width, draw.height) != height => draw.height,
            _ => height,
        };
        let raise = (height - draw.height) as i32;
        let (color, depth) = loop {
            let size = (draw.width, height);
            let color = self.surface_rows(memory, draw.target, size, Kind::Color(draw.format), true, Some(rows))?;
            let depth = match draw.depth {
                Some((addr, bytes)) => Some(self.surface_rows(memory, addr, size, Kind::Depth(bytes), true, Some(rows))?),
                None => None,
            };
            if self.surfaces[color].checked && depth.is_none_or(|d| self.surfaces[d].checked) {
                break (color, depth);
            }
        };
        let (bottom, top) = (bottom + raise, top + raise);

        let mut bound = [(self.blank, None); 3];
        let mut sources = [None; 3];
        let mut enabled = 0;
        for (unit, texture) in draw.textures.iter().enumerate() {
            if let Some(texture) = texture {
                let sampler = self.sampler(texture.linear, texture.wrap_s, texture.wrap_t)?;
                let image = match &texture.drawn {
                    Some(drawn) => match self.whole_surface(drawn).filter(|&index| index != color) {
                        Some(index) => {
                            sources[unit] = Some(self.surfaces[index].texture);
                            self.surfaces[index].texture
                        }
                        None => self.copy_texture(memory, drawn)?,
                    },
                    None => self.texture(texture)?,
                };
                bound[unit] = (image, Some(sampler));
                enabled |= 1 << unit;
            }
        }
        if draw.lighting.is_some() || draw.proctex {
            self.upload_tables(draw.tables, draw.proctex_tables);
        }

        let depth_map = draw.depth_map;
        let shaded = matches!(draw.geometry, Geometry::Shaded(_));
        let mapped = (depth_map.offset, depth_map.offset - depth_map.scale);
        let (writes_depth, depth_range) = match draw.geometry {
            _ if depth_map.w_buffer => (true, (0.0, 1.0)),
            Geometry::Triangles(_) => (false, (0.0, 1.0)),
            Geometry::Shaded(_) if [mapped.0, mapped.1].iter().all(|d| (0.0..=1.0).contains(d)) => (false, mapped),
            Geometry::Shaded(_) => (true, (0.0, 1.0)),
        };

        let worded = timed(19);
        let mut words = Vec::with_capacity(UNIFORM_WORDS);
        for base in STAGE_REGISTERS {
            words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], 0, 0, 0]);
        }
        let texture_config = (r[REG_TEXTURE_CONFIG] & !7) | enabled;
        words.extend([r[REG_UPDATE_BUFFER], r[REG_BUFFER_COLOR], r[REG_ALPHA_TEST], texture_config]);
        for (base, source) in TEXTURE_UNIT_BASES.into_iter().zip(sources) {
            words.extend([r[base + 2], r[base], source.is_some() as u32, 0]);
        }
        let depth_flags = depth_map.w_buffer as u32 | (shaded as u32) << 1;
        words.extend([depth_flags, draw.lighting.is_some() as u32, depth_map.scale.to_bits(), depth_map.offset.to_bits()]);
        match draw.lighting {
            Some(lighting) => lighting.pack(&mut words),
            None => words.resize(UNIFORM_WORDS - PROCTEX_WORDS - COLOR_WORDS, 0),
        }
        let base = proctex::REG_CONFIG;
        words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], r[base + 5], 0, 0]);
        words.extend(color_words(r));

        let mut constants = [0u32; FRAGMENT_CONSTANTS];
        for (i, base) in STAGE_REGISTERS.into_iter().enumerate() {
            constants[i] = r[base] & 0x0FFF_0FFF;
            constants[6 + i] = r[base + 1] & 0x0077_7FFF;
            constants[12 + i] = r[base + 2] & 0x000F_000F;
            constants[18 + i] = r[base + 4] & 0x0003_0003;
        }
        constants[24] = r[REG_UPDATE_BUFFER] & 0xFF00;
        constants[25] = texture_config & 0x2707;
        for (unit, (base, source)) in TEXTURE_UNIT_BASES.into_iter().zip(sources).enumerate() {
            let config = r[base + 2];
            let unbordered = enabled & 1 << unit != 0 && (config >> 8) & 3 != 1 && (config >> 12) & 3 != 1;
            constants[25] |= (source.is_some() as u32) << (16 + unit) | (unbordered as u32) << (20 + unit);
        }
        constants[26] = draw.lighting.is_some() as u32;
        constants[27] = r[REG_ALPHA_TEST] & 0x71 | unused_outputs(r);
        constants[28] = depth_flags | if writes_depth { WRITES_DEPTH } else { 0 };
        if self.generic {
            constants = generic_constants(&constants);
        }
        static GENERIC_LIGHTING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let lighting = if draw.lighting.is_some() && !self.generic && !*GENERIC_LIGHTING.get_or_init(|| crate::switch("ZAKURO_GENERIC_LIGHTING", "generic lighting")) {
            lighting_shape(&words)
        } else {
            Vec::new()
        };
        let program = match self.program(&constants, writes_depth, &lighting, vertex, false) {
            Ok(Some(program)) => program,
            // shaded on the CPU meanwhile
            Ok(None) if vertex != 0 => return Err(COMPILING.to_owned()),
            Ok(None) => self.program(&generic_constants(&constants), writes_depth, &[], 0, true)?.ok_or("no generic program")?,
            Err(error) => {
                if vertex != 0 {
                    for translation in self.translated.values_mut() {
                        if *translation == Some(vertex) {
                            *translation = None;
                        }
                    }
                }
                return Err(error);
            }
        };

        drop(worded);
        let color_mask = if r[REG_COLOR_BUFFER_WRITE] != 0 { (r[REG_DEPTH_COLOR_MASK] >> 8) & 0xF } else { 0 };
        let mask = r[REG_DEPTH_COLOR_MASK];
        let writable = r[REG_DEPTH_STENCIL_WRITE] != 0;
        let depth_test = mask & 1 != 0;
        let depth_write = writable && mask & (1 << 12) != 0;
        let stencil_test = draw.depth.is_some_and(|(_, bytes)| bytes == 4) && r[REG_STENCIL_TEST] & 1 != 0;
        let blend = (r[REG_COLOR_OPERATION] & 0x100 != 0).then_some(r[REG_BLEND_FUNC]);
        let mut color_mask = color_mask;
        match LogicOp::read(r) {
            Some(LogicOp::Noop) => color_mask = 0,
            Some(LogicOp::Copy) | None => {}
            Some(op) => {
                if !self.warned_logic_op {
                    log::warn!("OpenGL ES has no logic op {op:?}, drawing as copy");
                    self.warned_logic_op = true;
                }
            }
        }

        if color_mask != 0 {
            self.surfaces[color].drew(rows);
        }
        if let Some(depth) = depth {
            if depth_write || (stencil_test && writable) {
                self.surfaces[depth].drew(rows);
            }
        }
        let framebuffer = self.framebuffer(Some(color), depth)?;

        let geometry = match draw.geometry {
            Geometry::Triangles(triangles) => Submit::Triangles {
                viewport: [0, raise, draw.width as i32, draw.height as i32],
                vertices: Self::triangles(triangles, (draw.width as f32, draw.height as f32), depth_map),
                count: (triangles.len() * 3) as i32,
            },
            Geometry::Shaded(shading) => {
                let (x, y, width, height) = shading.viewport;
                let viewport = [x.round() as i32, (y + raise as f32).round() as i32, width.round() as i32, height.round() as i32];
                let shaded = timed(16);
                let (uniforms, inputs, stride) = Self::shading_bytes(shading, depth_map);
                let attributes = match shading.fetched {
                    Some(fetched) if crate::fast_with(crate::speedup::ARRAYS) => {
                        let read = shading.unit.inputs_read();
                        let slots = (0..INPUT_REGISTERS)
                            .filter(|r| read & (1 << r) != 0)
                            .map(|register| match fetched.inputs[register] {
                                Input::Array { array, offset, ty, count } => {
                                    (array, Attribute::Array { offset: offset as usize, ty, count, stride: fetched.arrays[array].stride })
                                }
                                Input::Constant(value) => (0, Attribute::Constant(value)),
                            })
                            .collect();
                        Attributes::Resident { arrays: fetched.arrays.iter().map(|array| array.bytes.clone()).collect(), slots }
                    }
                    Some(fetched) => {
                        let mut bytes = Vec::new();
                        let mut starts = Vec::with_capacity(fetched.arrays.len());
                        for array in &fetched.arrays {
                            bytes.resize(bytes.len().next_multiple_of(16), 0);
                            starts.push(bytes.len());
                            bytes.extend_from_slice(&array.bytes);
                        }
                        if bytes.is_empty() {
                            bytes.resize(16, 0);
                        }
                        let read = shading.unit.inputs_read();
                        let slots = (0..INPUT_REGISTERS)
                            .filter(|r| read & (1 << r) != 0)
                            .map(|register| match fetched.inputs[register] {
                                Input::Array { array, offset, ty, count } => {
                                    Attribute::Array { offset: starts[array] + offset as usize, ty, count, stride: fetched.arrays[array].stride }
                                }
                                Input::Constant(value) => Attribute::Constant(value),
                            })
                            .collect();
                        Attributes::Fetched { bytes, slots }
                    }
                    None => Attributes::Packed { bytes: inputs, stride },
                };
                let kept = if thin { shading.indices.len() / 6 * 3 } else { shading.indices.len() };
                let (indices, range, resident_indices) = match shading.prepared {
                    Some((bytes, highest)) if !thin && matches!(attributes, Attributes::Resident { .. }) => (Vec::new(), (0, highest), Some(bytes.clone())),
                    _ => {
                        let range = shading.indices[..kept].iter().fold((u32::MAX, 0), |(low, high), &index| (low.min(index), high.max(index)));
                        let indices: Vec<u8> = if range.1 <= 0xFFFF {
                            super::le_bytes(&shading.indices[..kept], |index| (index as u16).to_le_bytes())
                        } else {
                            super::le_bytes(&shading.indices[..kept], u32::to_le_bytes)
                        };
                        (indices, range, None)
                    }
                };
                drop(shaded);
                Submit::Shaded { viewport, cull: shading.cull, uniforms, attributes, indices, count: kept as i32, range, resident_indices }
            }
        };
        static CULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let culled = crate::fast() && !*CULL.get_or_init(|| crate::switch("ZAKURO_NO_CULL", "culling off")) && count >= 300 && color_mask != 0 && !stencil_test;
        let job = DrawJob {
            occlusion: culled.then(|| (mesh as u64 ^ vertex.rotate_left(17) ^ (draw.target as u64) << 32, self.pictures)),
            framebuffer,
            program,
            depth_range,
            scissor: [left, bottom, right - left, top - bottom],
            color_mask,
            blend: blend.map(|config| (config, r[REG_BLEND_COLOR])),
            depth: if depth.is_some() && (depth_test || depth_write) {
                Some(if depth_test { COMPARES[((mask >> 4) & 7) as usize] } else { glow::ALWAYS })
            } else {
                None
            },
            depth_mask: depth.is_some() && depth_write,
            stencil: stencil_test.then(|| (r[REG_STENCIL_TEST], r[REG_STENCIL_OP] & 0x777, writable)),
            bound,
            uniforms: words,
            vertex_array: self.vertex_array,
            shaded_array: self.shaded_array,
            geometry,
            targets: [Some(self.surfaces[color].texture), depth.map(|depth| self.surfaces[depth].texture)],
            sources,
        };
        let gl = self.gl;
        self.worker.post_draw(move || {
            if let Err(error) = job.submit(gl) {
                log::error!("the GPU could not draw, {error}");
            }
        });
        Ok(())
    }

    /// sends the draws held back.
    pub fn flush_draws(&self) {
        self.worker.flush();
    }

    fn vertex_stage(&mut self, shading: &Shading) -> Result<u64, String> {
        let unit = shading.unit;
        let key = (unit.fingerprint(), unit.entry_point, shading.semantics);
        if let Some(found) = self.translated.get(&key) {
            return found.ok_or_else(|| "the vertex program stays on the CPU".to_owned());
        }
        let translated = unit
            .prepared()
            .ok_or_else(|| "the program was not prepared".to_owned())
            .and_then(|program| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| glsl::translate(&program, key.1, &key.2)))
                    .unwrap_or_else(|_| Err("the translation panicked".to_owned()))
            })
            .and_then(|source| vertex_es(&source));
        let result = match translated {
            Ok(source) => {
                let hash = fnv(&[&source]);
                self.vertex_sources.insert(hash, source);
                Some(hash)
            }
            Err(error) => {
                log::warn!("program {:016X} from {} stays on the CPU, {error}", key.0, key.1);
                None
            }
        };
        self.translated.insert(key, result);
        result.ok_or_else(|| "the vertex program stays on the CPU".to_owned())
    }

    fn shading_bytes(shading: &Shading, depth_map: DepthMap) -> (Vec<u32>, Vec<u8>, usize) {
        let unit = shading.unit;
        let read = unit.inputs_read();
        let registers: Vec<usize> = (0..INPUT_REGISTERS).filter(|r| read & (1 << r) != 0).collect();
        let mut words: Vec<u32> = Vec::with_capacity(452);
        words.extend(unit.float_uniforms.iter().flatten().map(|value| value.to_bits()));
        for [count, start, step, _] in unit.int_uniforms {
            words.extend([count as u32, start as u32, step as i8 as u32, 0]);
        }
        words.extend([unit.bool_uniforms as u32, unit.entry_point, registers.len() as u32, 0]);
        words.extend([depth_map.scale.to_bits(), depth_map.offset.to_bits(), 0, 0]);
        let (x, y, width, height) = shading.viewport;
        words.extend([x, y, width, height].map(f32::to_bits));
        let mut inputs = Vec::with_capacity((shading.inputs.len() * registers.len()).max(1) * 16);
        for input in shading.inputs {
            for &register in &registers {
                for value in input[register] {
                    inputs.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        if inputs.is_empty() {
            inputs.resize(16, 0);
        }
        (words, inputs, registers.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// guest memory as one flat block from address zero.
    struct Flat(Vec<u8>);

    impl GpuMemory for Flat {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            out.copy_from_slice(&self.0[addr as usize..addr as usize + out.len()]);
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            self.0[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        }
    }

    /// the copy of a drawn texture as the GPU holds it, RGBA rows from the
    /// texture's first.
    fn read_copy(gles: &mut Gles, texture: &DrawnTexture) -> Vec<u8> {
        let (_, framebuffer) = gles.copies[texture];
        let (gl, width, height) = (gles.gl, texture.width, texture.height);
        let worker = gles.worker.clone();
        worker.run(move || unsafe {
            let mut data = vec![0u8; (width * height * 4) as usize];
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_pixels(0, 0, width as i32, height as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            data
        })
    }

    #[test]
    fn a_drawn_texture_that_starts_in_memory_is_copied_on_the_gpu() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let format = ColorFormat::Rgba8;
        let (base, width, lead, drawn) = (0x1000u32, 32u32, 16u32, 32u32);
        let mut memory = Flat(vec![0; 0x20000]);
        let bytes = width * (lead + drawn) * 4;
        for (i, byte) in memory.0[base as usize..(base + bytes) as usize].iter_mut().enumerate() {
            *byte = (i * 29 + i / 3) as u8;
        }
        let decoded = |memory: &Flat, addr: u32, rows: u32| {
            let mut out = vec![0u8; (width * rows * 4) as usize];
            for y in 0..rows {
                for x in 0..width {
                    let at = (addr + pixel_offset(true, x, y, width, 4) as u32) as usize;
                    let o = ((y * width + x) * 4) as usize;
                    out[o..o + 4].copy_from_slice(&format.decode(&memory.0[at..at + 4]));
                }
            }
            out
        };
        let drawing = base + lead / 8 * 8 * width * 4;
        let index = gles.surface_rows(&mut memory, drawing, (width, drawn), Kind::Color(format), true, None).unwrap();
        gles.surfaces[index].drew((0, drawn));
        // the case that already worked, a texture the drawing is the whole of
        // but for its copy, to check which way up the copy is
        let inside = DrawnTexture { addr: drawing, width, height: drawn, format };
        assert!(gles.holds(&inside));
        gles.copy_texture(&mut memory, &inside).unwrap();
        assert_eq!(read_copy(&mut gles, &inside), decoded(&memory, drawing, drawn));
        // memory's rows first, then the drawing's
        let over = DrawnTexture { addr: base, width, height: lead + drawn, format };
        assert!(gles.holds(&over));
        gles.copy_texture(&mut memory, &over).unwrap();
        assert_eq!(read_copy(&mut gles, &over), decoded(&memory, base, lead + drawn));
        // again unchanged, then with memory's rows changed, which the copy
        // takes up
        gles.copy_texture(&mut memory, &over).unwrap();
        assert_eq!(read_copy(&mut gles, &over), decoded(&memory, base, lead + drawn));
        for byte in &mut memory.0[base as usize..base as usize + 64] {
            *byte ^= 0x5A;
        }
        gles.copy_texture(&mut memory, &over).unwrap();
        assert_eq!(read_copy(&mut gles, &over), decoded(&memory, base, lead + drawn));
        // the drawing stayed on the GPU
        assert!(gles.surfaces.iter().any(|s| s.addr == drawing && s.dirty.is_some()));
    }

    #[test]
    fn a_shrinking_transfer_from_memory_into_a_drawing_matches_the_console() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let format = ColorFormat::Rgba8;
        let (input, output, width, lead, drawn) = (0x1000u32, 0x10000u32, 32u32, 16u32, 32u32);
        let mut memory = Flat(vec![0; 0x20000]);
        let bytes = width * (lead + drawn) * 4;
        for (i, byte) in memory.0[input as usize..(input + bytes) as usize].iter_mut().enumerate() {
            *byte = (i * 37 + i / 5) as u8;
        }
        // the drawing starts two rows of tiles in, as Fire Emblem Awakening's
        let drawing = input + lead / 8 * 8 * width * 4;
        let index = gles.surface_rows(&mut memory, drawing, (width, drawn), Kind::Color(format), true, None).unwrap();
        gles.surfaces[index].drew((0, drawn));
        let (out_width, out_height) = (width / 2, (lead + drawn) / 2);
        let transfer = Transfer {
            input,
            output,
            input_width: width,
            input_height: lead + drawn,
            output_width: out_width,
            output_height: out_height,
            copy: (out_width, out_height),
            scale: (2, 2),
            flip: false,
            input_linear: false,
            output_tiled: true,
            input_format: format,
            output_format: format,
            column: 0,
        };
        let mut expected = vec![0u8; (out_width * out_height * 4) as usize];
        for y in 0..out_height {
            for x in 0..out_width {
                let mut sum = [0u32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let at = (input + pixel_offset(true, x * 2 + dx, y * 2 + dy, width, 4) as u32) as usize;
                    let pixel = format.decode(&memory.0[at..at + 4]);
                    for c in 0..4 {
                        sum[c] += pixel[c] as u32;
                    }
                }
                let at = pixel_offset(true, x, y, out_width, 4);
                format.encode(sum.map(|c| ((c + 2) / 4) as u8), &mut expected[at..at + 4]);
            }
        }
        assert!(gles.display_transfer(&mut memory, &transfer).unwrap());
        // the drawing stayed on the GPU, nothing read it back
        assert!(gles.surfaces.iter().any(|s| s.addr == drawing && s.dirty.is_some()));
        gles.flush(&mut memory).unwrap();
        assert_eq!(&memory.0[output as usize..output as usize + expected.len()], &expected[..]);
    }

    #[test]
    fn a_texture_copy_of_a_drawing_stays_on_the_gpu_and_copies_it_exactly() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let (source, target, width, height) = (0x1000u32, 0x8000u32, 32u32, 32u32);
        for format in [ColorFormat::Rgba8, ColorFormat::Rgb565] {
            let size = width * height * format.bytes_per_pixel() as u32;
            let mut memory = Flat(vec![0; 0x20000]);
            for (i, byte) in memory.0[source as usize..(source + size) as usize].iter_mut().enumerate() {
                *byte = (i * 37 + i / 5) as u8;
            }
            let index = gles.surface_rows(&mut memory, source, (width, height), Kind::Color(format), true, None).unwrap();
            gles.surfaces[index].drew((0, height));
            // the second and third rows of tiles, as a game copies part of one
            let tile_rows = 8 * width * format.bytes_per_pixel() as u32;
            assert!(gles.texture_copy(&mut memory, source + tile_rows, target, 2 * tile_rows, (0, 0), (0, 0)).unwrap());
            gles.flush(&mut memory).unwrap();
            let copied = &memory.0[target as usize..(target + 2 * tile_rows) as usize];
            let original = &memory.0[(source + tile_rows) as usize..(source + 3 * tile_rows) as usize];
            assert_eq!(copied, original, "{format:?}");
            // part of the width, the way Fire Emblem Awakening copies a
            // strip of its picture for an effect: lines of a few tiles, the
            // rest of each row of tiles skipped, as the CPU would copy it
            let tile = 64 * format.bytes_per_pixel() as u32;
            for (first_tile, tiles, rows_of_tiles) in [(0u32, 2u32, 3u32), (1, 1, 2), (2, 2, 1)] {
                let start = source + tile_rows + first_tile * tile;
                let (line, skip) = (tiles * tile, tile_rows - tiles * tile);
                let size = line * rows_of_tiles;
                let mut expected = Vec::new();
                for i in 0..rows_of_tiles {
                    let at = (start + i * (line + skip)) as usize;
                    expected.extend_from_slice(&memory.0[at..at + line as usize]);
                }
                let target = 0xC000;
                // drawn again, the last check wrote it back
                let index = gles.surfaces.iter().position(|s| s.addr == source && s.kind == Kind::Color(format)).unwrap();
                gles.surfaces[index].drew((0, height));
                assert!(gles.texture_copy(&mut memory, start, target, size, (line, skip), (line, 0)).unwrap(), "{format:?} {first_tile} {tiles}");
                gles.flush(&mut memory).unwrap();
                assert_eq!(&memory.0[target as usize..(target + size) as usize], &expected[..], "{format:?} {first_tile} {tiles}");
            }
            // into the same layout, gaps and all, as its battles save the
            // picture: the columns between the lines keep what was there
            {
                let index = gles.surfaces.iter().position(|s| s.addr == source && s.kind == Kind::Color(format)).unwrap();
                gles.surfaces[index].drew((0, height));
                let (line, skip) = (tile_rows - tile, tile);
                let size = line * 2;
                let target = 0x14000usize;
                for (i, byte) in memory.0[target..target + 2 * tile_rows as usize].iter_mut().enumerate() {
                    *byte = (i * 11) as u8;
                }
                let mut expected = memory.0[target..target + 2 * tile_rows as usize].to_vec();
                for i in 0..2usize {
                    let from = (source + tile_rows) as usize + i * tile_rows as usize;
                    let to = i * tile_rows as usize;
                    expected[to..to + line as usize].copy_from_slice(&memory.0[from..from + line as usize]);
                }
                assert!(gles.texture_copy(&mut memory, source + tile_rows, target as u32, size, (line, skip), (line, skip)).unwrap(), "{format:?} strided");
                gles.flush(&mut memory).unwrap();
                assert_eq!(&memory.0[target..target + 2 * tile_rows as usize], &expected[..], "{format:?} strided");
            }
            // not starting on a tile is the CPU's
            assert!(!gles.texture_copy(&mut memory, source + 16, target, tile_rows, (0, 0), (0, 0)).unwrap());
        }
    }

    #[test]
    fn stored_rows_match_encoding_each_pixel() {
        let (width, height) = (24u32, 16u32);
        let data: Vec<u8> = (0..width * height * 4).map(|i| (i * 37 + i / 7) as u8).collect();
        for format in [ColorFormat::Rgba8, ColorFormat::Rgb8, ColorFormat::Rgb565, ColorFormat::Rgb5A1, ColorFormat::Rgba4] {
            let bpp = format.bytes_per_pixel();
            for tiled in [true, false] {
                for rows in [(0, height), (3, 11)] {
                    let mut expected = vec![0u8; (width * height) as usize * bpp];
                    for y in rows.0..rows.1 {
                        for x in 0..width {
                            let at = (((height - 1 - y) * width + x) * 4) as usize;
                            let out = pixel_offset(tiled, x, y, width, bpp as u32);
                            format.encode([data[at], data[at + 1], data[at + 2], data[at + 3]], &mut expected[out..out + bpp]);
                        }
                    }
                    let mut stored = vec![0u8; expected.len()];
                    match bpp {
                        4 => store_rows::<4>(&data, &mut stored, width, height, tiled, rows, |p| [p[3], p[2], p[1], p[0]]),
                        3 => store_rows::<3>(&data, &mut stored, width, height, tiled, rows, |p| [p[2], p[1], p[0]]),
                        _ => store_rows::<2>(&data, &mut stored, width, height, tiled, rows, |p| {
                            let mut out = [0; 2];
                            format.encode(p, &mut out);
                            out
                        }),
                    }
                    assert!(stored == expected, "{format:?}, tiled {tiled}, rows {rows:?}");
                }
            }
        }
    }
}
