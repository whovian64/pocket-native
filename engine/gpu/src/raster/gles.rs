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
use super::{color_words, BoundTexture, DepthMap, DrawnTexture, Lod, Screen, Wrap, COLOR_WORDS, TEXTURE_UNIT_BASES};
use crate::blend::LogicOp;
use crate::format::{morton_offset, ColorFormat};
use crate::texture::Packing;
use crate::lighting::Tables;
use crate::proctex;

use crate::registers::*;
use crate::shader::glsl::{self, SEMANTICS};
use crate::shader::{ShaderUnit, DESCRIPTOR_SIZE, INPUT_REGISTERS, PROGRAM_SIZE};
use crate::{GpuMemory, Picture, ScreenRef};

mod between;
mod display;
mod programs;
mod sources;

use display::*;
use programs::*;
use sources::*;
pub use display::{layout_scale, layout_size, panel_timing, second_window, set_second_shows_top, set_second_stretched, set_second_window, set_stacked, set_top_crop, set_window, stacked};
pub use programs::{compiling, compiling_stand_ins, note_compiler_crash};
pub(crate) use programs::COMPILING;

const VERTEX_SIZE: usize = 24 * 4;
const PROCTEX_WORDS: usize = 8;
/// the fog's color, and room.
const FOG_WORDS: usize = 4;
const UNIFORM_WORDS: usize = 12 * 4 + 4 + 3 * 4 + 4 + 328 + PROCTEX_WORDS + COLOR_WORDS + FOG_WORDS;
const TABLES_SIZE: usize = (24 * 256 + 3 * proctex::MAP_ENTRIES + 4 * proctex::COLOR_ENTRIES + crate::fog::ENTRIES) * 8;
const FRAGMENT_CONSTANTS: usize = 30;
const WRITES_DEPTH: u32 = 4;
/// bit 28 of the texture configuration constant, unit 0 projected.
const PROJECTED: u32 = 1 << 28;

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

/// batches a program nothing drew with is kept for once the driver ran out
/// of room.
const PROGRAM_KEEP_FLUSHES: u64 = 600;

/// batches a surface nothing drew into or read stays for, and how often the
/// ones past it are let go.
const SURFACE_KEEP_FLUSHES: u64 = 600;
const RETIRE_EVERY: u64 = 60;

/// batches a surface's memory goes without being compared with its shadow
/// while the journal says nothing wrote there.
const SURFACE_TRUSTED_FLUSHES: u64 = 60;

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
    /// the batch it was last drawn into or read in.
    used: u64,
    /// the journal's stamp when memory was last found to hold the shadow,
    /// and the batch that was in.
    matched: (u64, u64),
    /// bytes at the end of its memory a transfer into it never writes, which
    /// another surface has: a texture padded to a wider row whose last
    /// row's padding runs into the picture copied into it.
    tail_gap: u32,
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
        addr < self.addr + self.size() - self.tail_gap && self.addr < addr + len
    }

    fn row_bytes(&self) -> u32 {
        self.width * self.kind.bytes()
    }

    fn dirty_overlaps(&self, addr: u32, len: u32) -> bool {
        self.dirty.is_some_and(|(start, end)| {
            let (from, to) = (self.addr + start * self.row_bytes(), (self.addr + end * self.row_bytes()).min(self.addr + self.size() - self.tail_gap));
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
        worker.post_now(move || drop(send.send((0..64).filter_map(|_| make(&gl)).collect::<Vec<_>>())));
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

fn fnv(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.iter().flat_map(|part| part.bytes().chain([0])) {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
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
    units: [Option<(glow::Texture, Option<glow::Sampler>)>; 6],
    active_unit: Option<usize>,
    tables_bound: Option<usize>,
    viewport: Option<[i32; 4]>,
    cull: Option<u32>,
    vertex_array: Option<glow::VertexArray>,
    stride: Option<usize>,
    /// the resident arrays the shaded vertex array reads and how.
    resident: Option<(Vec<usize>, Vec<(usize, Attribute)>)>,
    uniforms: Vec<u32>,
    shading: Vec<u32>,
}

struct Ring {
    buffer: glow::Buffer,
    size: usize,
    offset: usize,
    align: usize,
    laps: u64,
    /// the whole buffer mapped for good, where the driver has buffer
    /// storage, written with no call into GL, and a fence after each
    /// quarter, waited on before that quarter is written again: Adreno's
    /// driver waits for the GPU on every map or update of a buffer in use
    mapped: Option<(*mut u8, [Option<glow::Fence>; 4])>,
}

/// whether rings are mapped for good: on Android, or with
/// ZAKURO_PERSISTENT, where the driver has buffer storage.
fn persistent(gl: &glow::Context) -> bool {
    static PERSISTENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PERSISTENT.get_or_init(|| {
        (cfg!(target_os = "android") || std::env::var_os("ZAKURO_PERSISTENT").is_some()) && gl.supported_extensions().contains("GL_EXT_buffer_storage")
    })
}

impl Ring {
    fn new(gl: &glow::Context, target: u32, size: usize, align: usize) -> Result<Ring, String> {
        // SAFETY: the context is current, and the mapping lasts as long as
        // the buffer, which a ring keeps
        unsafe {
            let mut buffer = gl.create_buffer()?;
            gl.bind_buffer(target, Some(buffer));
            if persistent(gl) {
                let flags = glow::MAP_WRITE_BIT | glow::MAP_PERSISTENT_BIT | glow::MAP_COHERENT_BIT;
                gl.buffer_storage(target, size as i32, None, flags);
                let base = gl.map_buffer_range(target, 0, size as i32, flags);
                if !base.is_null() {
                    return Ok(Ring { buffer, size, offset: 0, align, laps: 0, mapped: Some((base, Default::default())) });
                }
                log::warn!("could not map a buffer for good, GL error {:#x}, written through GL instead", gl.get_error());
                gl.delete_buffer(buffer);
                buffer = gl.create_buffer()?;
                gl.bind_buffer(target, Some(buffer));
            }
            gl.buffer_data_size(target, size as i32, glow::STREAM_DRAW);
            Ok(Ring { buffer, size, offset: 0, align, laps: 0, mapped: None })
        }
    }

    fn push(&mut self, gl: &glow::Context, target: u32, bytes: &[u8]) -> Result<usize, String> {
        self.push_aligned(gl, target, bytes, self.align)
    }

    fn push_aligned(&mut self, gl: &glow::Context, target: u32, bytes: &[u8], align: usize) -> Result<usize, String> {
        let _timed = timed(17);
        let mut at = self.offset.div_ceil(align) * align;
        if let Some((base, fences)) = &mut self.mapped {
            let quarter = self.size / 4;
            if bytes.is_empty() || bytes.len() > quarter {
                return Err(format!("{} bytes for a ring of {}", bytes.len(), self.size));
            }
            // each push inside one quarter, the next one over when it would
            // run across, so that a quarter's fence follows every draw of it
            if at / quarter != (at + bytes.len() - 1) / quarter {
                at = (at / quarter + 1) * quarter;
            }
            if at + bytes.len() > self.size {
                at = 0;
                self.laps += 1;
            }
            let (open, next) = (self.offset.saturating_sub(1) / quarter, at / quarter);
            // SAFETY: the context is current, the quarter written is one the
            // GPU is done with, and the range lies inside the mapping
            unsafe {
                if next != open {
                    if let Some(old) = fences[open].replace(gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)?) {
                        gl.delete_sync(old);
                    }
                    if let Some(fence) = fences[next].take() {
                        while gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, 100_000_000) == glow::TIMEOUT_EXPIRED {}
                        gl.delete_sync(fence);
                    }
                }
                gl.bind_buffer(target, Some(self.buffer));
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(at), bytes.len());
            }
            self.offset = at + bytes.len();
            return Ok(at);
        }
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
    /// ETC1A4's alpha, which ETC1 leaves out.
    alpha: Option<glow::Texture>,
    _texels: (Arc<[[u8; 4]]>, Option<crate::raster::Packed>),
    used: u64,
}

#[derive(PartialEq)]
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

/// how many bytes of arrays stay on the GPU before the oldest go, each
/// counted in whole pages.
const RESIDENT_BYTES: usize = 64 << 20;

/// how many arrays stay on the GPU before the oldest go, whatever their
/// size: each is a buffer the driver keeps pages and bookkeeping for, and
/// Ocarina of Time 3D makes some 85 small ones a second, which ran a 1 GB
/// handheld out of memory with this cache at a tenth of its bytes.
const RESIDENT_ARRAYS: usize = 2048;

/// what an array costs the driver at least, whole 4 KiB pages.
fn resident_cost(len: usize) -> usize {
    len.next_multiple_of(4096)
}

impl Resident {
    fn buffer(&mut self, gl: &glow::Context, array: &Arc<[u8]>) -> Result<glow::Buffer, String> {
        let key = (Arc::as_ptr(array) as *const u8 as usize, array.len());
        let draws = self.draws;
        if let Some((buffer, _, used)) = self.buffers.get_mut(&key) {
            *used = draws;
            return Ok(*buffer);
        }
        let cost = resident_cost(array.len());
        // the array no draw used for longest goes, one for each new one once
        // full rather than many in one draw, and never one the draw being
        // made already took
        while self.bytes + cost > RESIDENT_BYTES || self.buffers.len() >= RESIDENT_ARRAYS {
            let Some(oldest) = self.buffers.iter().filter(|(_, (_, _, used))| *used != draws).min_by_key(|(_, (_, _, used))| *used).map(|(&key, _)| key) else { break };
            let (buffer, array, _) = self.buffers.remove(&oldest).expect("an array just found");
            self.bytes -= resident_cost(array.len());
            // SAFETY: on the GL thread, the draws that read it were issued
            unsafe { gl.delete_buffer(buffer) };
        }
        // SAFETY: on the GL thread with the context current
        let buffer = unsafe {
            let buffer = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, array, glow::STATIC_DRAW);
            buffer
        };
        self.bytes += cost;
        self.buffers.insert(key, (buffer, array.clone(), draws));
        Ok(buffer)
    }
}

/// buffers the GPU thread fills for a draw, handed back once the GL thread
/// has sent them on rather than freed there: glibc frees a buffer made on
/// another thread through its arena's lock, and the two threads spent about
/// a fifth of their time making and freeing them.
struct Pool<T>(std::sync::Mutex<Vec<Vec<T>>>);

impl<T> Pool<T> {
    const fn new() -> Self {
        Pool(std::sync::Mutex::new(Vec::new()))
    }

    fn take(&self, capacity: usize) -> Vec<T> {
        let mut buffer = self.0.lock().unwrap().pop().unwrap_or_default();
        buffer.reserve(capacity);
        buffer
    }

    fn give(&self, mut buffer: Vec<T>) {
        // a big one goes, rather than stay held
        if buffer.capacity() == 0 || buffer.capacity() * std::mem::size_of::<T>() > 1 << 20 {
            return;
        }
        buffer.clear();
        let mut pool = self.0.lock().unwrap();
        if pool.len() < 64 {
            pool.push(buffer);
        }
    }
}

static WORDS: Pool<u32> = Pool::new();
static BYTES: Pool<u8> = Pool::new();

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
    /// the units' textures, then ETC1A4's alphas.
    bound: [(glow::Texture, Option<glow::Sampler>); 6],
    uniforms: Vec<u32>,
    vertex_array: glow::VertexArray,
    shaded_array: glow::VertexArray,
    geometry: Submit,
    targets: [Option<glow::Texture>; 2],
    sources: [Option<glow::Texture>; 3],
    tables: Option<(u64, Arc<[u8]>)>,
}

impl DrawJob {
    fn submit_logged(self, gl: GlRef) {
        if let Err(error) = self.submit(gl) {
            if super::first_time(&error) {
                log::error!("the GPU could not draw, {error}");
            }
        }
    }

    fn submit(self, gl: GlRef) -> Result<(), String> {
        let _timed_gl = timed(9);
        for source in self.sources.into_iter().flatten() {
            flush_clears(Some(source));
        }
        local(|local| {
            // the GL thread puts them past what the draws before still read,
            // this draw and those after bind them there
            if let Some((id, bytes)) = &self.tables {
                match local.table_sets.get(id).filter(|(laps, _)| *laps == local.tables.laps) {
                    Some(&(_, at)) => local.tables_at = Some(at),
                    None => match local.tables.push(&gl, glow::SHADER_STORAGE_BUFFER, bytes) {
                        Ok(at) => {
                            local.tables_at = Some(at);
                            if local.table_sets.len() > 1024 {
                                local.table_sets.clear();
                            }
                            local.table_sets.insert(*id, (local.tables.laps, at));
                        }
                        Err(error) => log::error!("could not upload the lighting tables, {error}"),
                    },
                }
            }
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
                // what only draws set stays: the uniform blocks and tables
                // they bound and the shaded vertex array's attributes
                let old = std::mem::take(&mut local.cache);
                local.cache = Cached { uniforms: old.uniforms, shading: old.shading, tables_bound: old.tables_bound, stride: old.stride, resident: old.resident, ..Cached::default() };
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
                    bind_program(&gl, self.program);
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
                // the texture and the sampler each only when it changed, the
                // alpha units mostly share their colour unit's sampler
                for (unit, (texture, sampler)) in self.bound.into_iter().enumerate() {
                    let was = cache.units[unit];
                    if was.map(|(texture, _)| texture) != Some(texture) {
                        if cache.active_unit != Some(unit) {
                            gl.active_texture(glow::TEXTURE0 + unit as u32);
                            cache.active_unit = Some(unit);
                        }
                        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    }
                    if was.map(|(_, sampler)| sampler) != Some(sampler) {
                        gl.bind_sampler(unit as u32, sampler);
                    }
                    cache.units[unit] = Some((texture, sampler));
                }
                if cache.uniforms != self.uniforms {
                    let offset = local.uniforms.push(&gl, glow::UNIFORM_BUFFER, word_bytes(&self.uniforms))?;
                    gl.bind_buffer_range(glow::UNIFORM_BUFFER, 3, Some(local.uniforms.buffer), offset as i32, self.uniforms.len() as i32 * 4);
                    WORDS.give(std::mem::replace(&mut local.cache.uniforms, self.uniforms));
                } else {
                    WORDS.give(self.uniforms);
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
                        BYTES.give(vertices);
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
                            WORDS.give(std::mem::replace(&mut local.cache.shading, uniforms));
                        } else {
                            WORDS.give(uniforms);
                        }
                        let cache = &mut local.cache;
                        if cache.vertex_array != Some(self.shaded_array) {
                            gl.bind_vertex_array(Some(self.shaded_array));
                            cache.vertex_array = Some(self.shaded_array);
                        }
                        let base_vertex = match attributes {
                            Attributes::Fetched { bytes, slots } => {
                                let at = local.inputs.push_aligned(&gl, glow::ARRAY_BUFFER, &bytes, 16)?;
                                BYTES.give(bytes);
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
                                (local.cache.stride, local.cache.resident) = (None, None);
                                0
                            }
                            Attributes::Resident { arrays, slots } => {
                                local.resident.draws += 1;
                                // the draw before's arrays read the same way,
                                // the vertex array still has them
                                let key: Vec<usize> = arrays.iter().map(|array| Arc::as_ptr(array) as *const u8 as usize).collect();
                                if !local.cache.resident.as_ref().is_some_and(|(last, read)| *last == key && *read == slots) {
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
                                    local.cache.resident = Some((key, slots));
                                }
                                0
                            }
                            Attributes::Packed { bytes, stride } => {
                                let vertex_bytes = stride.max(1) * 16;
                                let offset = local.inputs.push_aligned(&gl, glow::ARRAY_BUFFER, &bytes, vertex_bytes)?;
                                BYTES.give(bytes);
                                local.cache.resident = None;
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
                                let offset = local.indices.push(&gl, glow::ELEMENT_ARRAY_BUFFER, &indices)?;
                                BYTES.give(indices);
                                (offset, kind)
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
    /// the picture it was last drawn in, and whether it was drawn twice in
    /// one: a mesh the game draws in more than one place, as Super Mario 3D
    /// Land does its coins, blocks and enemies, is the same mesh to the key,
    /// and one of them out of view would leave out all of them
    seen: Option<u64>,
    shared: bool,
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
            self.meshes.retain(|_, mesh| mesh.query.is_some() || mesh.shared);
        }
        let Occlusion { meshes, free } = self;
        let mesh = meshes.entry(key).or_default();
        if mesh.seen == Some(picture) {
            mesh.shared = true;
        }
        mesh.seen = Some(picture);
        if mesh.shared {
            return (true, None);
        }
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

struct AssertSend<T>(T);

// SAFETY: only used for work the sending thread waits on, so nothing it
// holds is touched from two threads at once
unsafe impl<T> Send for AssertSend<T> {}

struct Worker {
    jobs: std::sync::mpsc::Sender<(bool, Job)>,
    thread: ThreadId,
    /// draws waiting to go over together, one wakeup for many
    held: std::sync::Mutex<Vec<Held>>,
}

/// a draw, or work between draws.
enum Held {
    Draw(DrawJob, GlRef),
    Job(Job),
}

/// the lists of draws sent over, handed back empty to be filled again.
static DRAWN: Pool<Held> = Pool::new();

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
                            let (width, height) = offscreen_size();
                            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, width, height);
                            let framebuffer = gl.create_framebuffer().expect("a framebuffer for the panel");
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(image));
                            Some(Output::Offscreen(framebuffer))
                        },
                        None => None,
                    };
                    if let Some(output) = output {
                        let ((width, height), turn) = panel_shape(&output);
                        set_layout(if turn.is_some() { (height, width) } else { (width, height) });
                        PANEL.with(|screen| *screen.borrow_mut() = Some(Panel { egl, output, program: None, textures: [None, None], empty: None, linear: None, overlay: None, overlay_textures: HashMap::new(), between: Between::default(), canvas: None, second: None }));
                    }
                    let gl: &'static glow::Context = Box::leak(Box::new(gl));
                    GL_CONTEXT.set(Some(gl));
                    SHARE_GROUP.set(egl as *const Egl as usize);
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

    /// posts work that touches no surface, the clears left for draws stay,
    /// in among the draws held when there are some rather than sending them
    /// off short.
    fn post_keeping(&self, job: impl FnOnce() + Send + 'static) {
        if self.here() {
            job();
            return;
        }
        let held = self.held.lock().unwrap();
        if held.is_empty() {
            drop(held);
            self.jobs.send((true, Box::new(job))).expect("the GL thread stopped");
        } else {
            self.hold(held, Held::Job(Box::new(job)));
        }
    }

    /// posts work something waits on, the draws held going first.
    fn post_now(&self, job: impl FnOnce() + Send + 'static) {
        if self.here() {
            job();
        } else {
            self.flush();
            self.jobs.send((true, Box::new(job))).expect("the GL thread stopped");
        }
    }

    /// a draw, held to go with the next few.
    fn post_draw(&self, job: DrawJob, gl: GlRef) {
        if self.here() {
            job.submit_logged(gl);
            return;
        }
        self.hold(self.held.lock().unwrap(), Held::Draw(job, gl));
    }

    fn hold(&self, mut held: std::sync::MutexGuard<Vec<Held>>, item: Held) {
        if held.capacity() == 0 {
            *held = DRAWN.take(16);
        }
        held.push(item);
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

    fn send_draws(&self, mut draws: Vec<Held>) {
        self.jobs
            .send((
                true,
                Box::new(move || {
                    draws.drain(..).for_each(|held| match held {
                        Held::Draw(draw, gl) => draw.submit_logged(gl),
                        Held::Job(job) => job(),
                    });
                    DRAWN.give(draws);
                }),
            ))
            .expect("the GL thread stopped");
    }

    fn run<R>(&self, job: impl FnOnce() -> R) -> R {
        self.run_with(false, job)
    }

    /// run for work that touches no surface but those whose clears it sees to
    /// itself, the clears left for draws stay.
    fn run_keeping<R>(&self, job: impl FnOnce() -> R) -> R {
        self.run_with(true, job)
    }

    fn run_with<R>(&self, keeps_clears: bool, job: impl FnOnce() -> R) -> R {
        if self.here() {
            if !keeps_clears {
                flush_clears(None);
            }
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
        self.jobs.send((keeps_clears, wrapped)).expect("the GL thread stopped");
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
    /// what each translation came from, for the manifest.
    vertex_inputs: HashMap<u64, (u64, u32, [u32; SEMANTICS])>,
    /// what each program being made came from, for the manifest.
    inputs: HashMap<ProgramKey, String>,
    shaded_array: glow::VertexArray,
    generic: bool,
    vertex_array: glow::VertexArray,
    tables: Option<(u64, u64, u64)>,
    /// tables changed since the last draw went, they go with the next.
    tables_due: Option<(u64, Arc<[u8]>)>,
    table_hashes: [(u64, u64); 24],
    table_sets: rustc_hash::FxHashMap<[u64; 26], (u64, Arc<[u8]>)>,
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
    samplers: rustc_hash::FxHashMap<(bool, Wrap, Wrap, Lod, bool), glow::Sampler>,
    blank: glow::Texture,
    transfer: Option<glow::Program>,
    reinterpreter: Option<glow::Program>,
    /// the last draw's colour words and program and what they came from.
    fragment: Option<((u64, u32, [bool; 3], bool, bool, u64), Vec<u32>, glow::Program)>,
    upright: Option<glow::Program>,
    packers: Option<(glow::Program, glow::Program, glow::VertexArray)>,
    copiers: Option<([glow::Program; 3], glow::VertexArray)>,
    copies: rustc_hash::FxHashMap<DrawnTexture, (glow::Texture, glow::Framebuffer)>,
    copied: rustc_hash::FxHashMap<DrawnTexture, (glow::Texture, u64, u32)>,
    /// the memory rows a texture over a drawing last had, to upload them
    /// again only when they change.
    over_drawing: rustc_hash::FxHashMap<DrawnTexture, Vec<u8>>,
    flushes: u64,
    /// the batch surfaces left unused are next looked for.
    retire_at: u64,
    /// the batch each program last drew in, and the last one stamped.
    program_used: rustc_hash::FxHashMap<glow::Program, u64>,
    stamped: Option<(glow::Program, u64)>,
    /// the batch room was last made in, and the programs that got a second
    /// try after it.
    room_made: Option<u64>,
    retried: rustc_hash::FxHashSet<ProgramKey>,
    /// fingerprints of the sources the driver crashed on before.
    crashed: rustc_hash::FxHashSet<u64>,
    /// display transfers so far, the clock occlusion culling keeps time by.
    pictures: u64,
    warned_logic_op: bool,
    preloading: bool,
    /// the console's resolution times this over 4 is what the surfaces are
    /// drawn at, 4 native.
    quarters: u32,
    /// textures and framebuffers the console's size, by depth or not and
    /// size, which a surface drawn bigger goes through to and from memory.
    natives: rustc_hash::FxHashMap<(bool, u32, u32), (glow::Texture, glow::Framebuffer)>,
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
            let _ = RENDERER_NAME.set(name.clone());
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
            // the generic programs check the fog at run time and so read the
            // tables whatever the draw, which Panfrost faults on with nothing
            // bound there: the ring's start stands in until a draw's go up
            gl.bind_buffer_range(glow::SHADER_STORAGE_BUFFER, 4, Some(tables.buffer), 0, TABLES_SIZE as i32);
            let blank = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(blank));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, 1, 1);
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 1, 1, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 255])));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::DITHER);
            log::info!("drawing with OpenGL ES on {name}");
            let extensions = gl.supported_extensions();
            log::info!(
                "shader compiling: KHR_parallel_shader_compile {}, ARB_parallel_shader_compile {}",
                if extensions.contains("GL_KHR_parallel_shader_compile") { "yes" } else { "no" },
                if extensions.contains("GL_ARB_parallel_shader_compile") { "yes" } else { "no" }
            );
            ADRENO.store(name.contains("Adreno"), std::sync::atomic::Ordering::Relaxed);
            if gl.supported_extensions().contains("GL_OES_compressed_ETC1_RGB8_texture") && !crate::switch("ZAKURO_NO_ETC1", "ETC1 textures decoded") {
                crate::raster::SAMPLES_ETC1.store(true, std::sync::atomic::Ordering::Relaxed);
                let split = !crate::switch("ZAKURO_NO_ETC1A4", "ETC1A4 textures decoded");
                crate::raster::SAMPLES_ETC1A4.store(split, std::sync::atomic::Ordering::Relaxed);
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
                vertex_inputs: HashMap::new(),
                inputs: HashMap::new(),
                shaded_array,
                generic: crate::switch("ZAKURO_GENERIC_SHADERS", "generic shaders"),
                vertex_array,
                tables: None,
                tables_due: None,
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
                reinterpreter: None,
                fragment: None,
                upright: None,
                packers: None,
                copiers: None,
                copies: Default::default(),
                copied: Default::default(),
                over_drawing: Default::default(),
                flushes: 0,
                retire_at: RETIRE_EVERY,
                program_used: Default::default(),
                stamped: None,
                room_made: None,
                retried: Default::default(),
                crashed: crashed(),
                pictures: 0,
                warned_logic_op: false,
                preloading: false,
                quarters: 4,
                natives: Default::default(),
            };
            gles.prepare_helpers();
            gles.preload();
            gles.prepare_stand_ins();
            Ok(gles)
        }
    }

    /// builds the programs copies, transfers and read backs use before the
    /// compiler thread has work, rather than the first one a game needs
    /// waiting on the GL thread for the compiler's lock while it builds the
    /// game's own, which stopped everything for half a minute on a first run.
    fn prepare_helpers(&mut self) {
        let mut build = || -> Result<(), String> {
            self.transfer = Some(self.compute(TRANSFER_SOURCE)?);
            self.reinterpreter = Some(self.compute(REINTERPRET_SOURCE)?);
            self.upright = Some(self.compute(UPRIGHT_SOURCE)?);
            let depth = self.graphics(PACK_VERTEX_SOURCE, PACK_DEPTH_SOURCE)?;
            let stencil = self.graphics(PACK_VERTEX_SOURCE, PACK_STENCIL_SOURCE)?;
            let copiers = [
                self.graphics(PACK_VERTEX_SOURCE, COPY_COLOR_SOURCE)?,
                self.graphics(PACK_VERTEX_SOURCE, COPY_DEPTH_SOURCE)?,
                self.graphics(PACK_VERTEX_SOURCE, COPY_STENCIL_SOURCE)?,
            ];
            // SAFETY: on the GL thread
            let (packing, copying) = unsafe { (self.gl.create_vertex_array()?, self.gl.create_vertex_array()?) };
            self.packers = Some((depth, stencil, packing));
            self.copiers = Some((copiers, copying));
            Ok(())
        };
        // the ones not built are built when first needed, as before
        if let Err(error) = build() {
            log::warn!("the copy and transfer programs wait until needed, {error}");
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
                    let _ = self.program(&generic_constants(&constants), writes_depth, &[], 0, false, None);
                }
            }
        }
    }

    fn preload(&mut self) {
        let Some(path) = manifest_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        if text.lines().next() != Some(MANIFEST_HEADER) {
            log::info!("{} is from another version, starting it over", path.display());
            let _ = std::fs::remove_file(&path);
            let _ = vertex_path(&path, 0).parent().map(std::fs::remove_dir_all);
            return;
        }
        let started = std::time::Instant::now();
        self.preloading = true;
        // with several compiler threads they all take the list, the start
        // waiting for the last of them, rather than the GL thread building
        // one program after another
        let queue = self.compiler.is_some() && compilers() > 1;
        let built_before = BUILD_MICROS.load(std::sync::atomic::Ordering::Relaxed);
        let (mut translating, mut programs) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let mut units: HashMap<u64, Option<ShaderUnit>> = HashMap::new();
        let mut count = 0;
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let vertex = match fields.next() {
                Some("0") => 0,
                Some(field) => {
                    let Ok(parts) = field.split(':').map(|part| u64::from_str_radix(part, 16)).collect::<Result<Vec<u64>, _>>() else { continue };
                    let Some((&[fingerprint, entry], semantics)) = parts.split_first_chunk::<2>() else { continue };
                    let Ok(semantics) = <[u32; SEMANTICS]>::try_from(semantics.iter().map(|&value| value as u32).collect::<Vec<u32>>()) else { continue };
                    let unit = units.entry(fingerprint).or_insert_with(|| {
                        let bytes = std::fs::read(vertex_path(&path, fingerprint)).ok().filter(|bytes| bytes.len() == (PROGRAM_SIZE + DESCRIPTOR_SIZE) * 4)?;
                        let mut unit = ShaderUnit::new();
                        for (word, chunk) in unit.program.iter_mut().chain(unit.descriptors.iter_mut()).zip(bytes.as_chunks::<4>().0) {
                            *word = u32::from_le_bytes(*chunk);
                        }
                        unit.prepare();
                        Some(unit)
                    });
                    let Some(unit) = unit else { continue };
                    unit.entry_point = entry as u32;
                    let began = std::time::Instant::now();
                    let stage = self.vertex_stage(unit, semantics);
                    translating += began.elapsed();
                    let Ok(hash) = stage else { continue };
                    hash
                }
                None => continue,
            };
            let Some(Ok(flags)) = fields.next().map(|field| u32::from_str_radix(field, 16)) else { continue };
            let mut r = [0u32; 0x300];
            for (register, value) in fields.filter_map(|field| field.split_once('=')) {
                if let (Ok(register @ 0..0x300), Ok(value)) = (usize::from_str_radix(register, 16), u32::from_str_radix(value, 16)) {
                    r[register] = value;
                }
            }
            let mut packed = Vec::new();
            if flags & 1 << 17 != 0 {
                let Some(lighting) = crate::lighting::Lighting::read(&r) else { continue };
                lighting.pack(&mut packed);
            }
            let (writes_depth, sources) = (flags & 1 << 16 != 0, std::array::from_fn(|unit| flags & 1 << (8 + unit) != 0));
            let (constants, lighting) = fragment_constants(&r, flags & 7, flags >> 4 & 7, sources, flags >> 12 & 3, writes_depth, (flags & 1 << 17 != 0).then_some(&packed[..]));
            let began = std::time::Instant::now();
            match self.program(&constants, writes_depth, &lighting, vertex, !queue, None) {
                Ok(_) => count += 1,
                Err(error) => log::warn!("could not prepare a shader from {}, {error}", path.display()),
            }
            programs += began.elapsed();
        }
        let waiting = std::time::Instant::now();
        while self.compiler.as_ref().is_some_and(|compiler| !compiler.pending.is_empty()) {
            self.take_compiled(true);
        }
        self.preloading = false;
        let built = BUILD_MICROS.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(built_before) as f64 / 1e6;
        log::info!(
            "prepared {count} shaders from {} in {:.1} s: {:.1} s translating vertex programs, {:.1} s handing out programs, {:.1} s waiting on the compiler threads, {:.1} s building them in all",
            path.display(),
            started.elapsed().as_secs_f64(),
            translating.as_secs_f64(),
            programs.as_secs_f64(),
            waiting.elapsed().as_secs_f64(),
            built
        );
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

    /// the scale pictures read back come at: always the console's own, the
    /// surfaces drawn bigger are brought down to it.
    pub fn scale(&self) -> u32 {
        1
    }

    /// draws at scale times the console's resolution from now on, in
    /// quarters from 1 to 4, and says what it draws at.
    pub fn set_scale(&mut self, scale: f32) -> f32 {
        self.quarters = (scale * 4.0).round().clamp(4.0, 16.0) as u32;
        self.quarters as f32 / 4.0
    }

    /// a length of the console's in the surfaces' pixels: whole for any
    /// multiple of 4, as every tile and surface edge is.
    fn up(&self, length: u32) -> u32 {
        length * self.quarters / 4
    }

    fn scaled(&self) -> bool {
        self.quarters != 4
    }

    /// the console-size texture and framebuffer for a kind of surface and
    /// size.
    /// SAFETY: on the GL thread
    unsafe fn native(&mut self, depth: bool, width: u32, height: u32) -> Result<(glow::Texture, glow::Framebuffer), String> {
        if let Some(&made) = self.natives.get(&(depth, width, height)) {
            return Ok(made);
        }
        let gl = &self.gl;
        let texture = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_storage_2d(glow::TEXTURE_2D, 1, if depth { glow::DEPTH24_STENCIL8 } else { glow::RGBA8 }, width as i32, height as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
        let framebuffer = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        if depth {
            gl.draw_buffers(&[glow::NONE]);
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::DEPTH_STENCIL_ATTACHMENT, glow::TEXTURE_2D, Some(texture), 0);
        } else {
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        self.natives.insert((depth, width, height), (texture, framebuffer));
        Ok((texture, framebuffer))
    }

    /// rows of a surface, from the bottom as GL counts them, between it and
    /// its console-size twin, the nearest pixel scaled each way: up after an
    /// upload into the twin, or down to read it back.
    /// SAFETY: on the GL thread
    unsafe fn through_native(&mut self, index: usize, up: bool, rows: &[(u32, u32)]) -> Result<(glow::Texture, glow::Framebuffer), String> {
        let (width, height, kind) = (self.surfaces[index].width, self.surfaces[index].height, self.surfaces[index].kind);
        let depth = matches!(kind, Kind::Depth(_));
        let native = self.native(depth, width, height)?;
        let surface = if depth { self.framebuffer(None, Some(index))? } else { self.framebuffer(Some(index), None)? };
        let (from, to) = if up { (native.1, surface) } else { (surface, native.1) };
        let mask = if depth { glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT } else { glow::COLOR_BUFFER_BIT };
        disturb();
        let (big_width, small_width) = (self.up(width) as i32, width as i32);
        let gl = &self.gl;
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(from));
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(to));
        gl.disable(glow::SCISSOR_TEST);
        gl.color_mask(true, true, true, true);
        gl.depth_mask(true);
        gl.stencil_mask(0xFF);
        for &(bottom, count) in rows {
            let (small, big) = ([0, bottom as i32, small_width, (bottom + count) as i32], [0, self.up(bottom) as i32, big_width, self.up(bottom + count) as i32]);
            let (a, b) = if up { (small, big) } else { (big, small) };
            gl.blit_framebuffer(a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], mask, glow::NEAREST);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        Ok(native)
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
        // above the console's resolution this stays with memory for now
        if self.scaled() {
            return None;
        }
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

    /// a texture inside rows drawings of other widths drew, as Moon's bloom
    /// samples the smaller buffers it drew into the bigger one's memory, and
    /// Kingdom Hearts 3D a 256x512 texture over its picture and the glow
    /// drawn below it: each drawing, how many tiles past the texture's start
    /// it starts, and the texture's tiles it holds, or none for whole rows of
    /// tiles no drawing holds, which come from memory at the console's size.
    /// the copy takes the drawings' tiles over a run at a time on the GPU
    /// rather than reading them back. RGBA8 alone, which a blit leaves as it
    /// is. drawings of other kinds inside the rows from memory, as Smash's
    /// small buffers below the picture it reads as 512x512, are written
    /// back first, those alone.
    fn retiled_sources(&self, texture: &DrawnTexture) -> Option<(Vec<(Option<usize>, i64, i64, i64)>, Vec<usize>)> {
        let kind = Kind::Color(ColorFormat::Rgba8);
        if texture.format != ColorFormat::Rgba8 || !texture.width.is_multiple_of(8) || !texture.height.is_multiple_of(8) {
            return None;
        }
        let (tile, len) = ((64 * kind.bytes()) as i64, texture.width * texture.height * kind.bytes());
        let (across, tiles) = ((texture.width / 8) as i64, (texture.width / 8 * texture.height / 8) as i64);
        let (mut pieces, mut apart) = (Vec::new(), Vec::new());
        for (index, s) in self.surfaces.iter().enumerate().filter(|(_, s)| s.dirty_overlaps(texture.addr, len)) {
            // its dirty rows are what it drew, though others took rows of it
            let (first, last) = s.dirty?;
            let offset = s.addr as i64 - texture.addr as i64;
            let other_across = (s.width / 8) as i64;
            let run = across.min(other_across);
            let (from, to) = ((offset / tile + first as i64 / 8 * other_across).max(0), (offset / tile + last as i64 / 8 * other_across).min(tiles));
            let fits = s.kind == kind
                && s.tiled
                && s.width % 8 == 0
                && first % 8 == 0
                && last % 8 == 0
                && offset % tile == 0
                && (other_across % across == 0 || across % other_across == 0)
                && from % run == 0
                && to % run == 0
                && (from - offset / tile) % run == 0;
            if !fits {
                apart.push(index);
                continue;
            }
            pieces.push((Some(index), offset / tile, from, to));
        }
        if pieces.is_empty() {
            return None;
        }
        // the whole texture, a piece after another
        pieces.sort_by_key(|piece| piece.2);
        let mut whole = Vec::with_capacity(pieces.len() + 1);
        let mut at = 0;
        for piece in pieces.into_iter().chain([(None, 0, tiles, tiles)]) {
            if piece.2 < at {
                return None;
            }
            if piece.2 > at {
                if self.scaled() || at % across != 0 || piece.2 % across != 0 {
                    return None;
                }
                whole.push((None, 0, at, piece.2));
            }
            if piece.0.is_some() {
                at = piece.3;
                whole.push(piece);
            }
        }
        let start = texture.addr as i64;
        let inside_memory = |s: &Surface| {
            let (from, to) = ((s.addr as i64 - start).max(0) / tile, ((s.addr as i64 + s.size() as i64 - start).min(len as i64) + tile - 1) / tile);
            whole.iter().any(|piece| piece.0.is_none() && piece.2 <= from && to <= piece.3)
        };
        apart.iter().all(|&index| inside_memory(&self.surfaces[index])).then_some((whole, apart))
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
        self.texture_source(texture).is_some_and(|(index, _)| self.surfaces[index].dirty.is_some()) || self.texture_over_drawing(texture).is_some() || crate::fast_with(crate::speedup::TILES) && self.retiled_sources(texture).is_some()
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
        let (gl, width, height) = (self.gl, self.up(texture.width) as i32, self.up(texture.height) as i32);
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

    /// lets go of the surfaces nothing drew into or read for a while, what
    /// they drew written back first, with their framebuffers and the copies
    /// made from them. a title drawing into new places scene after scene
    /// would fill a handheld's memory otherwise, Mario Kart 7 sixty-four at
    /// a time. no surface index is held between draws, so they can move.
    fn retire<M: GpuMemory>(&mut self, memory: &mut M) -> Result<(), String> {
        self.retire_at = self.flushes + RETIRE_EVERY;
        let flushes = self.flushes;
        let old: Vec<usize> = (0..self.surfaces.len()).filter(|&i| flushes - self.surfaces[i].used > SURFACE_KEEP_FLUSHES).collect();
        if old.is_empty() {
            return Ok(());
        }
        let dirty = old.iter().copied().filter(|&i| self.surfaces[i].dirty.is_some()).collect();
        self.write_back(memory, dirty)?;
        let mut remap = vec![None; self.surfaces.len()];
        let mut kept = 0;
        for (i, slot) in remap.iter_mut().enumerate() {
            if !old.contains(&i) {
                *slot = Some(kept);
                kept += 1;
            }
        }
        let (mut textures, mut framebuffers) = (Vec::new(), Vec::new());
        let mut ranges = Vec::new();
        for &i in &old {
            let s = &self.surfaces[i];
            textures.push(s.texture);
            for (texture, framebuffer) in [s.upright, s.packed].into_iter().flatten() {
                textures.push(texture);
                framebuffers.push(framebuffer);
            }
            ranges.push((s.addr, s.size()));
        }
        let made_from = |t: &DrawnTexture| {
            let size = t.width * t.height * t.format.bytes_per_pixel() as u32;
            ranges.iter().any(|&(addr, len)| t.addr < addr + len && addr < t.addr + size)
        };
        self.copies.retain(|t, &mut (texture, framebuffer)| {
            let keep = !made_from(t);
            if !keep {
                textures.push(texture);
                framebuffers.push(framebuffer);
            }
            keep
        });
        self.copied.retain(|t, _| !made_from(t));
        self.over_drawing.retain(|t, _| !made_from(t));
        for ((color, depth), framebuffer) in std::mem::take(&mut self.framebuffers) {
            let (color, depth) = (color.map(|i| remap[i]), depth.map(|i| remap[i]));
            match (color, depth) {
                (Some(None), _) | (_, Some(None)) => framebuffers.push(framebuffer),
                _ => {
                    self.framebuffers.insert((color.flatten(), depth.flatten()), framebuffer);
                }
            }
        }
        let mut i = 0;
        self.surfaces.retain(|_| {
            i += 1;
            remap[i - 1].is_some()
        });
        log::debug!("let go of {} surfaces nothing used for {SURFACE_KEEP_FLUSHES} batches, {} left", old.len(), self.surfaces.len());
        let gl = self.gl;
        self.worker.post_keeping(move || {
            disturb();
            // SAFETY: on the GL thread, after every draw that used them
            unsafe {
                for framebuffer in framebuffers {
                    gl.delete_framebuffer(framebuffer);
                }
                for texture in textures {
                    gl.delete_texture(texture);
                }
            }
        });
        Ok(())
    }

    fn copy_texture<M: GpuMemory>(&mut self, memory: &mut M, texture: &DrawnTexture) -> Result<glow::Texture, String> {
        if !self.texture_source(texture).is_some_and(|(index, _)| self.surfaces[index].dirty.is_some()) {
            if let Some((pieces, apart)) = self.retiled_sources(texture) {
                self.write_back(memory, apart)?;
                return self.copy_retiled(memory, texture, pieces);
            }
        }
        if self.texture_source(texture).is_none() {
            if let Some((source, lead)) = self.texture_over_drawing(texture) {
                return self.copy_over_drawing(memory, texture, source, lead);
            }
        }
        let (source, row) = self.texture_source(texture).ok_or("the buffer a texture was drawn into is gone")?;
        self.surfaces[source].used = self.flushes;
        let (([color, depth, stencil], empty), (copy, framebuffer)) = self.copy_objects(texture)?;
        let surface = &self.surfaces[source];
        let stamp = (surface.texture, surface.version, row);
        if self.copied.insert(*texture, stamp) == Some(stamp) {
            return Ok(copy);
        }
        let (kind, image) = (surface.kind, surface.texture);
        let base = self.up(surface.height) as i32 - 1 - self.up(row) as i32;
        let (width, height) = (self.up(texture.width) as i32, self.up(texture.height) as i32);
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

    /// the copy of a texture inside drawings of other widths, their tiles
    /// blitted over from each a run at a time as adopt does, and the rows no
    /// drawing holds uploaded from memory.
    fn copy_retiled<M: GpuMemory>(&mut self, memory: &mut M, texture: &DrawnTexture, pieces: Vec<(Option<usize>, i64, i64, i64)>) -> Result<glow::Texture, String> {
        let (_, (copy, framebuffer)) = self.copy_objects(texture)?;
        let (width, across) = (texture.width, (texture.width / 8) as i64);
        let row_bytes = (width * 4) as usize;
        let (mut runs, mut rows, mut bytes) = (Vec::with_capacity(pieces.len()), Vec::new(), Vec::new());
        for (index, offset, from, to) in pieces {
            let Some(index) = index else {
                let (first, last) = ((from / across * 8) as u32, (to / across * 8) as u32);
                let start = bytes.len();
                bytes.resize(start + (last - first) as usize * row_bytes, 0);
                memory.read(texture.addr + first * row_bytes as u32, &mut bytes[start..]);
                rows.push((first, last, start));
                continue;
            };
            self.surfaces[index].used = self.flushes;
            let read = self.framebuffer(Some(index), None)?;
            let s = &self.surfaces[index];
            runs.push((read, s.texture, s.version, (s.width / 8) as i64, s.height as i64, offset, from, to));
        }
        let version = runs.iter().fold(0, |hash: u64, run| hash.rotate_left(17) ^ run.2);
        let stamp = (runs[0].1, version, 0x8000_0000 | runs.len() as u32);
        let drawn_again = self.copied.insert(*texture, stamp) != Some(stamp);
        let unchanged = self.over_drawing.get(texture).is_some_and(|before| *before == bytes);
        if !drawn_again && unchanged {
            return Ok(copy);
        }
        // memory's rows the right way up, as the copy is
        let decoded: Vec<(u32, u32, Vec<u8>)> = rows
            .iter()
            .map(|&(first, last, start)| {
                let mut out = vec![0u8; (last - first) as usize * row_bytes];
                for y in first..last {
                    for x in 0..width {
                        let at = start + pixel_offset(true, x, y - first, width, 4);
                        let o = ((y - first) * width + x) as usize * 4;
                        out[o..o + 4].copy_from_slice(&ColorFormat::Rgba8.decode(&bytes[at..at + 4]));
                    }
                }
                (first, last, out)
            })
            .collect();
        self.over_drawing.insert(*texture, bytes);
        let (quarters, gl) = (self.quarters as i64, self.gl);
        self.worker.post_keeping(move || {
            for run in &runs {
                flush_clears(Some(run.1));
            }
            disturb();
            // SAFETY: on the GL thread, every run lies inside both the
            // drawing and the copy, and the rows uploaded inside the copy
            unsafe {
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(framebuffer));
                gl.disable(glow::SCISSOR_TEST);
                gl.color_mask(true, true, true, true);
                let up = |length: i64| (length * quarters / 4) as i32;
                for &(read, _, _, other_across, height, offset, from, to) in &runs {
                    gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
                    let run = across.min(other_across);
                    // the drawing upside down, the copy the right way up
                    for at in (from..to).step_by(run as usize) {
                        let (x, y) = ((at - offset) % other_across * 8, (at - offset) / other_across * 8);
                        let (to_x, to_y) = (at % across * 8, at / across * 8);
                        gl.blit_framebuffer(up(x), up(height - y - 8), up(x + run * 8), up(height - y), up(to_x), up(to_y + 8), up(to_x + run * 8), up(to_y), glow::COLOR_BUFFER_BIT, glow::NEAREST);
                    }
                }
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                if !decoded.is_empty() {
                    gl.bind_texture(glow::TEXTURE_2D, Some(copy));
                    for (first, last, out) in &decoded {
                        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, *first as i32, width as i32, (last - first) as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(out)));
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
        self.surfaces[source].used = self.flushes;
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
        // above the console's resolution these stay with the CPU for now
        if size == 0 || self.scaled() {
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
            if offset >= s.size() || !offset.is_multiple_of(tile) {
                return None;
            }
            // a line of the input is whole rows of tiles, or the same few
            // tiles of each row of tiles, part of the picture's width
            let line = match input_lines {
                (0, _) | (_, 0) => tile_row,
                (bytes, skip) if bytes + skip == tile_row => bytes,
                _ => return None,
            };
            // whole rows of tiles can run on past the drawing into memory,
            // as Federation Force copies 64 rows out of 56
            if offset + size > s.size() && line != tile_row {
                return None;
            }
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
        if t.copy.0 == 0 || t.copy.1 == 0 || *CPU.get_or_init(|| crate::switch("ZAKURO_CPU_TRANSFERS", "display transfers on the CPU")) {
            return Ok(false);
        }
        let rows = t.copy.1 * t.scale.1;
        let first = if t.flip { t.input_height - rows } else { 0 };
        // a linear input is a picture an earlier transfer left linear, as
        // Paper Mario's, read the same way, row by row rather than eight
        let tiled = !t.input_linear;
        let step = if tiled { 8 } else { 1 };
        let tile_rows = step * t.input_width * input_kind.bytes();
        let row_in = |s: &Surface| {
            let offset = t.input.checked_sub(s.addr).filter(|offset| offset % tile_rows == 0)? / tile_rows * step;
            (offset + first + rows <= s.height).then_some(offset)
        };
        let Some((addr, input_size, row)) = self
            .surfaces
            .iter()
            .filter(|s| s.kind == input_kind && s.tiled == tiled && s.width == t.input_width)
            .filter_map(|s| Some((s, row_in(s)?)))
            .max_by_key(|(s, _)| (s.dirty.is_some(), s.height))
            .map(|(s, row)| (s.addr, (s.width, s.height), row))
        else {
            return self.transfer_below_memory(memory, t);
        };
        let input_end = addr as u64 + (input_size.0 * input_size.1 * input_kind.bytes()) as u64;
        let output_end = t.output as u64 + (t.output_width * t.output_height * output_kind.bytes()) as u64;
        // a tiled output wider than the copy has its last row of tiles' end
        // left as it was, which may be the input's start: Yo-kai Watch 2
        // copies its 240 wide top screen into a texture 256 wide just before it
        let gap = if t.output_tiled && t.copy.0 < t.output_width && t.copy.1 == t.output_height { ((t.output_width - t.copy.0) * 8 * output_kind.bytes()) as u64 } else { 0 };
        if (addr as u64) < output_end - gap && (t.output as u64) < input_end {
            return Ok(false);
        }
        let tail = ((addr as u64) < output_end && (t.output as u64) < input_end).then_some(gap as u32);
        self.current()?;
        let (source, target) = loop {
            let source = self.surface_rows(memory, addr, input_size, input_kind, tiled, None)?;
            let rows = tail.map(|_| (0, t.output_height - 8));
            let target = self.surface_rows(memory, t.output, (t.output_width, t.output_height), output_kind, t.output_tiled, rows)?;
            if let Some(gap) = tail {
                self.surfaces[target].tail_gap = gap;
            }
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
        // in the surfaces' pixels: the copy, the rows, and where it reads
        let quarters = self.quarters;
        let up = move |length: u32| length * quarters / 4;
        self.worker.post_keeping(move || unsafe {
            flush_clears(Some(source_texture));
            flush_clears(Some(target_texture));
            disturb();
            // SAFETY: on the GL thread, both textures have immutable rgba8
            // storage and the dispatch stays inside them
            gl.use_program(Some(program));
            let int = |name: &str, value: i32| gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), value);
            let pair = |name: &str, x: u32, y: u32| gl.uniform_2_i32(gl.get_uniform_location(program, name).as_ref(), x as i32, y as i32);
            pair("size", up(t.copy.0), up(t.copy.1));
            pair("scale", t.scale.0, t.scale.1);
            int("input_height", up(t.input_height) as i32);
            int("output_height", up(t.output_height) as i32);
            int("flip", t.flip as i32);
            int("input_format", format_index(t.input_format));
            int("output_format", format_index(t.output_format));
            int("source_row", up(row) as i32);
            int("source_column", up(t.column) as i32);
            int("target_row", 0);
            int("source_height", up(input_size.1) as i32);
            gl.bind_image_texture(0, Some(source_texture), 0, false, 0, glow::READ_ONLY, glow::RGBA8);
            gl.bind_image_texture(1, Some(target_texture), 0, false, 0, glow::WRITE_ONLY, glow::RGBA8);
            gl.dispatch_compute(up(t.copy.0).div_ceil(8), up(t.copy.1).div_ceil(8), 1);
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
    /// its picture for an effect, or goes on past a drawing into rows nothing
    /// drew, as Metroid Samus Returns shrinks its 400 rows as 512: the rows
    /// from memory and the drawing each go through the transfer on the GPU,
    /// into their parts of the output, rather than the drawing being read
    /// back for the CPU.
    fn transfer_below_memory<M: GpuMemory>(&mut self, memory: &mut M, t: &Transfer) -> Result<bool, String> {
        // above the console's resolution this stays with the CPU for now
        if self.scaled() || t.flip || t.input_linear || t.column != 0 {
            return Ok(false);
        }
        let (input_kind, output_kind) = (Kind::Color(t.input_format), Kind::Color(t.output_format));
        let tile_rows = 8 * t.input_width * input_kind.bytes();
        let rows = t.copy.1 * t.scale.1;
        // the drawing, its size, and the rows of memory before or after it
        let Some((addr, size, lead, before)) = self.surfaces.iter().find_map(|s| {
            if s.kind != input_kind || !s.tiled || s.width != t.input_width || s.dirty.is_none() {
                return None;
            }
            match s.addr.checked_sub(t.input).filter(|&offset| offset % tile_rows == 0) {
                Some(0) => (s.height < rows && s.height % 8 == 0 && s.height.is_multiple_of(t.scale.1)).then_some((s.addr, (s.width, s.height), rows - s.height, false)),
                Some(offset) => {
                    let lead = offset / tile_rows * 8;
                    (lead < rows && lead.is_multiple_of(t.scale.1) && rows <= lead + s.height).then_some((s.addr, (s.width, s.height), lead, true))
                }
                None => None,
            }
        }) else {
            return Ok(false);
        };
        // the rows of memory come as they are, which another drawing there
        // would make a read back again
        let rest = if before { t.input } else { addr + size.1 / 8 * tile_rows };
        if self.surfaces.iter().any(|s| s.dirty_overlaps(rest, lead / 8 * tile_rows)) {
            return Ok(false);
        }
        let input_end = (t.input as u64 + (rows.div_ceil(8) * tile_rows) as u64).max(addr as u64 + (size.0 * size.1 * input_kind.bytes()) as u64);
        let output_end = t.output as u64 + (t.output_width * t.output_height * output_kind.bytes()) as u64;
        if (t.input as u64) < output_end && (t.output as u64) < input_end {
            return Ok(false);
        }
        self.current()?;
        let (memory_rows, drawing, target) = loop {
            let memory_rows = self.surface_rows(memory, rest, (t.input_width, lead), input_kind, true, None)?;
            let drawing = self.surface_rows(memory, addr, size, input_kind, true, None)?;
            let target = self.surface_rows(memory, t.output, (t.output_width, t.output_height), output_kind, t.output_tiled, None)?;
            if [memory_rows, drawing, target].iter().all(|&i| self.surfaces[i].checked) {
                break (memory_rows, drawing, target);
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
        // each part: its source, the source's height, and the output rows
        // it fills, from and how many
        let (memory_rows, drawing) = ((self.surfaces[memory_rows].texture, lead), (self.surfaces[drawing].texture, size.1));
        let [(a, a_height), (b, b_height)] = if before { [memory_rows, drawing] } else { [drawing, memory_rows] };
        let split = a_height / t.scale.1;
        let parts = [(a, a_height, 0, split), (b, b_height, split, t.copy.1 - split)];
        let target_texture = self.surfaces[target].texture;
        self.worker.post_keeping(move || unsafe {
            for (source, ..) in parts {
                flush_clears(Some(source));
            }
            flush_clears(Some(target_texture));
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
        let _compiling = compiler_lock();
        let _watched = Watched;
        at_compiler([source, ""]);
        // SAFETY: the context is current, the shader is deleted once linked
        unsafe {
            let program = gl.create_program()?;
            let shader = gl.create_shader(glow::COMPUTE_SHADER)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let error = format!("could not compile a compute shader, {}", gl.get_shader_info_log(shader));
                gl.delete_shader(shader);
                gl.delete_program(program);
                return Err(error);
            }
            gl.attach_shader(program, shader);
            gl.link_program(program);
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
            if !gl.get_program_link_status(program) {
                let error = format!("could not link a compute program, {}", gl.get_program_info_log(program));
                gl.delete_program(program);
                return Err(error);
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
            return worker.run_keeping(|| self.picture(screen));
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
        // the screen's clear only, the others wait for their draws
        flush_clears(Some(self.surfaces[index].texture));
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
        let quarters = self.quarters;
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
            gl.uniform_1_i32(gl.get_uniform_location(program, "quarters").as_ref(), quarters as i32);
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
    fn program(&mut self, constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32], vertex: u64, wait: bool, inputs: Option<&dyn Fn() -> String>) -> Result<Option<glow::Program>, String> {
        let key = (*constants, writes_depth, lighting.to_vec(), vertex);
        if let Some(&program) = self.programs.get(&key) {
            return Ok(Some(program));
        }
        // what the compiler finished, still compiles or could not, and what
        // it is given, are seen to here, only a program built here and now
        // waits on the GL thread
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
        if [fnv(&[&compiled]), fnv(&[&fragment]), fnv(&[&compiled, &fragment])].iter().any(|fingerprint| self.crashed.contains(fingerprint)) {
            return Err("the driver crashed on its program before".to_owned());
        }
        let cached = program_cache_path(&self.name, &compiled, &fragment);
        if let Some(inputs) = inputs {
            self.inputs.insert(key.clone(), inputs());
        }
        if !wait && (!self.preloading || compilers() > 1) {
            if let Some(compiler) = self.compiler.as_mut() {
                compiler.pending.insert(key.clone());
                BUILDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if compiler.jobs.send(Compile { key, vertex: compiled, fragment, cached }).is_err() {
                    BUILDING.store(0, std::sync::atomic::Ordering::Relaxed);
                    BUILDING_STAND_INS.store(0, std::sync::atomic::Ordering::Relaxed);
                    self.compiler = None;
                }
                return Ok(None);
            }
        }
        if !self.worker.here() {
            let worker = self.worker.clone();
            wait_from("program");
            return worker.run(|| self.program(constants, writes_depth, lighting, vertex, wait, None));
        }
        disturb();
        let program = match build_program(&self.gl, &compiled, &fragment, cached.as_deref()) {
            Ok(program) => program,
            Err(error) if self.retried.insert(key.clone()) => {
                log::warn!("{error}");
                self.make_room();
                build_program(&self.gl, &compiled, &fragment, cached.as_deref())?
            }
            Err(error) => return Err(error),
        };
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
                    // the driver out of memory, most likely: the next draw
                    // that needs it tries again once room is made
                    if self.retried.insert(key.clone()) {
                        self.make_room();
                        continue;
                    }
                    self.inputs.remove(&key);
                    if key.3 != 0 {
                        for translation in self.translated.values_mut() {
                            if *translation == Some(key.3) {
                                *translation = None;
                            }
                        }
                    }
                    self.compiler.as_mut().expect("the compiler is still there").failed.insert(key, error);
                }
            }
        }
    }

    /// writes a new program the title used into the manifest, for the next
    /// run to prepare at startup.
    fn note_program(&mut self, key: &ProgramKey) {
        let Some(inputs) = self.inputs.remove(key) else { return };
        if self.preloading || key.0[29] != 0 {
            return;
        }
        let vertex = key.3;
        let Some(path) = manifest_path() else { return };
        let vertex = match vertex {
            0 => "0".to_owned(),
            hash => {
                let Some((fingerprint, entry, semantics)) = self.vertex_inputs.get(&hash) else { return };
                [*fingerprint, *entry as u64].into_iter().chain(semantics.iter().map(|&value| value as u64)).map(|value| format!("{value:x}")).collect::<Vec<_>>().join(":")
            }
        };
        let line = format!("{vertex} {inputs}");
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
        if let Some(index) = found {
            self.surfaces[index].used = self.flushes;
        }
        if found.is_none_or(|index| !self.surfaces[index].checked) && !self.worker.here() {
            if let Some(index) = found {
                let size = self.surfaces[index].size();
                let row = self.surfaces[index].row_bytes();
                let (from, to) = rows.map_or((addr, size), |(start, end)| (addr + start * row, (end - start) * row));
                // what others drew there comes over without waiting on the GL thread
                if self.adopt_all(index, from, to)?.is_empty() {
                    // memory nothing wrote over since it held the shadow
                    // still does, it is compared again now and then all the
                    // same in case a write went past the journal
                    let (stamp, batch) = self.surfaces[index].matched;
                    let (written, now) = crate::journal::written_since(stamp, from, to);
                    if written == crate::journal::Written::Nothing && self.flushes - batch < SURFACE_TRUSTED_FLUSHES {
                        let surface = &mut self.surfaces[index];
                        (surface.checked, surface.stale, surface.matched.0) = (true, false, now);
                        return Ok(index);
                    }
                    let same = match memory.slice(addr, size as usize) {
                        Some(bytes) => same(bytes, &self.surfaces[index].shadow),
                        None => {
                            let mut bytes = vec![0; size as usize];
                            memory.read(addr, &mut bytes);
                            bytes == self.surfaces[index].shadow
                        }
                    };
                    if same {
                        let flushes = self.flushes;
                        let surface = &mut self.surfaces[index];
                        (surface.checked, surface.stale, surface.matched) = (true, false, (now, flushes));
                        return Ok(index);
                    }
                    // all of it drawn, as after taking another's tiles, no
                    // row of memory goes up: memory is only kept as the GL
                    // thread would
                    if self.surfaces[index].dirty == Some((0, height)) {
                        let mut bytes = vec![0; size as usize];
                        memory.read(addr, &mut bytes);
                        let flushes = self.flushes;
                        let surface = &mut self.surfaces[index];
                        (surface.shadow, surface.picture, surface.checked, surface.stale, surface.matched) = (bytes, None, true, false, (now, flushes));
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
                let (drawn_width, drawn_height) = (self.up(width) as i32, self.up(height) as i32);
                let gl = &self.gl;
                // SAFETY: the context is current
                let texture = unsafe {
                    let texture = gl.create_texture()?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    let format = match kind {
                        Kind::Color(_) => glow::RGBA8,
                        Kind::Depth(_) => glow::DEPTH24_STENCIL8,
                    };
                    gl.tex_storage_2d(glow::TEXTURE_2D, 1, format, drawn_width, drawn_height);
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
                    used: self.flushes,
                    matched: (0, 0),
                    tail_gap: 0,
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
        let others = self.adopt_all(index, from, to)?;
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

    /// the surfaces drawn over a surface's memory from addr for len bytes
    /// whose tiles could not come over, tried again while any does, the
    /// rows one brings may join those of one that could not come before it.
    fn adopt_all(&mut self, index: usize, addr: u32, len: u32) -> Result<Vec<usize>, String> {
        let mut others: Vec<usize> = (0..self.surfaces.len()).filter(|&i| i != index && self.surfaces[i].dirty_overlaps(addr, len)).collect();
        loop {
            let before = others.len();
            let mut left = Vec::new();
            for other in others {
                if !self.adopt(index, other)? {
                    left.push(other);
                }
            }
            others = left;
            if others.is_empty() || others.len() == before {
                return Ok(others);
            }
        }
    }

    /// tiles another surface drew over this one's memory come over on the
    /// GPU, with the drawing, rather than through memory, when whole rows of
    /// tiles of both line up.
    fn adopt(&mut self, index: usize, other: usize) -> Result<bool, String> {
        let (s, o) = (&self.surfaces[index], &self.surfaces[other]);
        if matches!((s.kind, o.kind), (Kind::Color(_), Kind::Color(_))) && (s.kind != o.kind || s.tiled != o.tiled) && crate::fast_with(crate::speedup::TILES) {
            return self.reinterpret(index, other);
        }
        if s.kind != o.kind || !s.tiled || !o.tiled || !crate::fast_with(crate::speedup::TILES) || s.width % 8 != 0 || o.width % 8 != 0 || s.height % 8 != 0 || o.height % 8 != 0 {
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
        let (quarters, gl) = (self.quarters as i64, self.gl);
        let textures = (self.surfaces[other].texture, self.surfaces[index].texture);
        // from the GPU thread as well, the draws before it go first
        self.worker.post_keeping(move || {
            // what either was cleared to is in before tiles go over
            flush_clears(Some(textures.0));
            flush_clears(Some(textures.1));
            disturb();
            // SAFETY: on the GL thread, every tile copied lies inside both
            // surfaces
            unsafe {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(target));
                gl.disable(glow::SCISSOR_TEST);
                gl.color_mask(true, true, true, true);
                gl.depth_mask(true);
                gl.stencil_mask(0xFF);
                // in the surfaces' pixels, whole for tiles at any scale
                let up = |length: i64| (length * quarters / 4) as i32;
                let blit = |(x, y): (i64, i64), (to_x, to_y): (i64, i64), (wide, high): (i64, i64)| {
                    gl.blit_framebuffer(
                        up(x),
                        up(other_height - y - high),
                        up(x + wide),
                        up(other_height - y),
                        up(to_x),
                        up(height - to_y - high),
                        up(to_x + wide),
                        up(height - to_y),
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
        });
        let o = &mut self.surfaces[other];
        o.dirty = left;
        o.checked = false;
        o.stale = true;
        // the rows that came over no longer match memory, the rest still say
        // what it held, for a write back to see the CPU wrote there since,
        // as Kid Icarus keeps its heap where a picture's last rows were drawn
        let row = o.row_bytes() as usize;
        if let Some(rows) = o.shadow.get_mut(gone_from as usize * row..gone_to as usize * row) {
            rows.iter_mut().for_each(|byte| *byte = !*byte);
        }
        o.picture = None;
        let s = &mut self.surfaces[index];
        s.dirty = Some(merged);
        s.picture = None;
        s.version = next_version();
        Ok(true)
    }

    /// the rows another colour surface drew over this one's memory in
    /// another format or layout, as Kingdom Hearts 3D draws the same 128 KiB
    /// as RGBA4 and as RGBA8 in turn: its pixels go through memory's bytes
    /// into this one's on the GPU rather than being read back. whole rows of
    /// both, at the console's size.
    fn reinterpret(&mut self, index: usize, other: usize) -> Result<bool, String> {
        let (s, o) = (&self.surfaces[index], &self.surfaces[other]);
        let Some((first, last)) = o.dirty else { return Ok(false) };
        let unit = |x: &Surface| x.row_bytes() * if x.tiled { 8 } else { 1 };
        let lo = (o.addr + first * o.row_bytes()).max(s.addr);
        let hi = (o.addr + last * o.row_bytes()).min(s.addr + s.size());
        if self.scaled() || lo >= hi || (lo - s.addr) % unit(s) != 0 || (hi - s.addr) % unit(s) != 0 || (lo - o.addr) % unit(o) != 0 || (hi - o.addr) % unit(o) != 0 {
            return Ok(false);
        }
        let (from, to) = ((lo - s.addr) / s.row_bytes(), (hi - s.addr) / s.row_bytes());
        let (gone_from, gone_to) = ((lo - o.addr) / o.row_bytes(), (hi - o.addr) / o.row_bytes());
        let left = match (first < gone_from, gone_to < last) {
            (true, true) => return Ok(false),
            (true, false) => Some((first, gone_from)),
            (false, true) => Some((gone_to, last)),
            (false, false) => None,
        };
        let merged = match s.dirty {
            None => (from, to),
            Some((a, b)) if b >= from && to >= a => (a.min(from), b.max(to)),
            Some(_) => return Ok(false),
        };
        let shape = |x: &Surface| {
            let Kind::Color(format) = x.kind else { unreachable!() };
            ([x.width as i32, x.height as i32, x.kind.bytes() as i32, format_index(format)], x.tiled as i32, x.texture)
        };
        let ((target_shape, target_tiled, target), (source_shape, source_tiled, source)) = (shape(s), shape(o));
        let delta = (lo - o.addr) as i32 - (from * s.row_bytes()) as i32;
        let program = match self.reinterpreter {
            Some(program) => program,
            None => {
                let program = if self.worker.here() {
                    self.compute(REINTERPRET_SOURCE)?
                } else {
                    let worker = self.worker.clone();
                    worker.run(|| self.compute(REINTERPRET_SOURCE))?
                };
                self.reinterpreter = Some(program);
                program
            }
        };
        let gl = self.gl;
        self.worker.post_keeping(move || {
            flush_clears(Some(source));
            flush_clears(Some(target));
            disturb();
            // SAFETY: on the GL thread, both textures have immutable rgba8
            // storage and every pixel read and written lies inside them
            unsafe {
                gl.use_program(Some(program));
                let location = |name: &str| gl.get_uniform_location(program, name);
                gl.uniform_4_i32_slice(location("target_shape").as_ref(), &target_shape);
                gl.uniform_4_i32_slice(location("source_shape").as_ref(), &source_shape);
                gl.uniform_2_i32(location("tiled").as_ref(), target_tiled, source_tiled);
                gl.uniform_2_i32(location("rows").as_ref(), from as i32, (to - from) as i32);
                gl.uniform_1_i32(location("delta").as_ref(), delta);
                gl.bind_image_texture(0, Some(source), 0, false, 0, glow::READ_ONLY, glow::RGBA8);
                gl.bind_image_texture(1, Some(target), 0, false, 0, glow::WRITE_ONLY, glow::RGBA8);
                gl.dispatch_compute((target_shape[0] as u32).div_ceil(8), (to - from).div_ceil(8), 1);
                gl.memory_barrier(glow::ALL_BARRIER_BITS);
            }
        });
        let o = &mut self.surfaces[other];
        (o.dirty, o.checked, o.stale, o.shadow, o.picture) = (left, false, true, Vec::new(), None);
        let s = &mut self.surfaces[index];
        (s.dirty, s.picture, s.version) = (Some(merged), None, next_version());
        Ok(true)
    }

    fn upload(&mut self, index: usize, bytes: &[u8]) {
        let _timed = timed(2);
        self.surfaces[index].version = next_version();
        let s = &self.surfaces[index];
        let (width, height, kind, tiled, mut texture) = (s.width, s.height, s.kind, s.tiled, s.texture);
        // drawn bigger, the pixels go into the console-size twin first
        if self.scaled() {
            // SAFETY: on the GL thread, as uploads are
            match unsafe { self.native(matches!(kind, Kind::Depth(_)), width, height) } {
                Ok((native, _)) => texture = native,
                Err(error) => log::error!("could not upload a surface drawn bigger, {error}"),
            }
        }
        let s = &self.surfaces[index];
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
        if self.scaled() {
            let rows: Vec<(u32, u32)> = ranges.iter().map(|&(from, to)| (height - to, to - from)).collect();
            // SAFETY: on the GL thread, as uploads are
            if let Err(error) = unsafe { self.through_native(index, true, &rows) } {
                log::error!("could not upload a surface drawn bigger, {error}");
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
                gone.extend([Some(texture.texture), texture.alpha].into_iter().flatten());
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

    /// lets go of what the driver holds that can be made again when it
    /// could not build a program, on a handheld its memory running out: the
    /// textures, the vertex arrays kept on the GPU and the programs nothing
    /// drew with for a while, rather than every draw after it going to the
    /// CPU. Once a batch at most.
    fn make_room(&mut self) {
        if self.room_made == Some(self.flushes) {
            return;
        }
        self.room_made = Some(self.flushes);
        // the batch being made keeps what its draws already took
        let mut textures = Vec::new();
        let flushes = self.flushes;
        self.textures.retain(|_, texture| {
            let keep = texture.used == flushes;
            if !keep {
                textures.extend([Some(texture.texture), texture.alpha].into_iter().flatten());
            }
            keep
        });
        let recent = self.flushes.saturating_sub(PROGRAM_KEEP_FLUSHES);
        let mut programs = Vec::new();
        let used = &self.program_used;
        self.programs.retain(|_, program| {
            let keep = used.get(program).is_some_and(|&at| at >= recent);
            if !keep {
                programs.push(*program);
            }
            keep
        });
        for program in &programs {
            self.program_used.remove(program);
        }
        self.fragment = None;
        self.stamped = None;
        log::warn!("the driver could not build a program: letting go of {} textures, {} programs and the vertex arrays kept on the GPU", textures.len(), programs.len());
        let gl = self.gl;
        self.worker.post_keeping(move || {
            disturb();
            local(|local| {
                for (_, (buffer, _, _)) in local.resident.buffers.drain() {
                    // SAFETY: on the GL thread, after every draw that used it
                    unsafe { gl.delete_buffer(buffer) };
                }
                local.resident.bytes = 0;
                (local.cache.resident, local.cache.stride) = (None, None);
            });
            // SAFETY: on the GL thread, after every draw that used them, the
            // finish letting the driver take their memory back
            unsafe {
                textures.into_iter().for_each(|texture| gl.delete_texture(texture));
                programs.into_iter().for_each(|program| delete_program(&gl, program));
                gl.finish();
            }
        });
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

    pub(crate) fn present(&mut self, shown: [Option<(crate::PanelImage, [i32; 4])>; 2], cursor: Option<(i32, i32)>, overlays: Vec<Arc<crate::Overlay>>, smooth: bool, interpolate: bool, frames: u32) {
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
                        let up = |length: u32| self.up(length) as i32;
                        let screen = [up(screen.rows.1), up(screen.columns), up(s.height), up(screen.rows.0)];
                        let top = index == 0;
                        // the bottom screen too once it is drawn bigger, its pixels no longer the console's
                        let smooth = smooth && (top || self.scaled());
                        parts.push(Part::Surface { texture: s.texture, screen, area, smooth, top, picture: (interpolate && top).then_some(s.version) });
                    }
                }
                crate::PanelImage::Pixels(pixels, width, height) => parts.push(Part::Pixels { index, pixels, width, height, area, smooth: smooth && index == 0 }),
            }
        }
        if let Some((x, y)) = cursor {
            parts.push(Part::Cursor { x, y });
        }
        parts.extend(overlays.into_iter().map(Part::Overlay));
        // with two waiting for the panel already, this one waits its turn:
        // dropped, its frame would have been drawn for nothing, where held
        // up, the title falls behind and skips drawing frames instead
        while self.presenting.load(std::sync::atomic::Ordering::Acquire) >= 2 {
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        self.presenting.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let presenting = self.presenting.clone();
        let gl = self.gl;
        self.worker.post_now(move || {
            // the shown screens' clears, those of pictures still being drawn
            // wait for their draws
            for part in &parts {
                if let Part::Surface { texture, .. } = part {
                    flush_clears(Some(*texture));
                }
            }
            sim_present(&gl);
            // with the next picture already waiting this one goes up at
            // once: held for even pacing, it would hold up the title, which
            // then skips frames and stays behind, Pokémon X at 16 fps
            let frames = if presenting.load(std::sync::atomic::Ordering::Acquire) >= 2 { 1 } else { frames };
            if let Err(error) = show(gl, parts, frames) {
                log::error!("could not show the screens, {error}");
            }
            presenting.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        });
    }

    /// a depth surface's depth and stencil packed into color and read, from
    /// its console-size twin when it is drawn bigger.
    fn read_depth(&mut self, index: usize, native: Option<glow::Texture>, data: &mut [u8]) -> Result<(), String> {
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
        let (width, height, texture) = (surface.width as i32, surface.height as i32, native.unwrap_or(surface.texture));
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
            // buffer may keep its heap there: the rows that would go out are
            // the ones compared
            let (addr, size) = (s.addr, s.size() as usize);
            let (from, to) = (first as usize * s.row_bytes() as usize, (last as usize * s.row_bytes() as usize).min(size - s.tail_gap as usize));
            let written = s.shadow.len() == size
                && match memory.slice(addr + from as u32, to - from) {
                    Some(now) => now != &s.shadow[from..to],
                    None => {
                        let mut now = vec![0u8; to - from];
                        memory.read(addr + from as u32, &mut now);
                        now != s.shadow[from..to]
                    }
                };
            if written {
                log::debug!("the CPU wrote over 0x{addr:08X} since it was drawn, the drawing stays out of memory");
                let surface = &mut self.surfaces[index];
                (surface.dirty, surface.checked, surface.picture) = (None, false, None);
                continue;
            }
            let mut data = vec![0u8; (width * height * 4) as usize];
            // drawn bigger, it comes down to the console's size first
            // SAFETY: on the GL thread, as write backs are
            let native = if self.scaled() { Some(unsafe { self.through_native(index, false, &[(0, height)])? }) } else { None };
            match kind {
                Kind::Color(_) => {
                    let framebuffer = match native {
                        Some((_, framebuffer)) => framebuffer,
                        None => self.framebuffer(Some(index), None)?,
                    };
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
                Kind::Depth(_) => self.read_depth(index, native.map(|(texture, _)| texture), &mut data)?,
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
            let end = (last as usize * row).min(bytes.len() - s.tail_gap as usize);
            memory.write(addr + (first as usize * row) as u32, &bytes[first as usize * row..end]);
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

    fn texture(&mut self, bound: &BoundTexture) -> Result<(glow::Texture, Option<glow::Texture>), String> {
        let _timed = timed(6);
        let key = bound.packed.as_ref().map_or(Arc::as_ptr(&bound.texels) as *const u8, |(_, bytes)| Arc::as_ptr(bytes) as *const u8) as usize;
        if let Some(texture) = self.textures.get_mut(&key) {
            texture.used = self.flushes;
            return Ok((texture.texture, texture.alpha));
        }
        // its texels go up after the draws before
        let texture = self.texture_name()?;
        let alpha = match bound.packed {
            Some((Packing::Etc1A4, _)) => Some(self.texture_name()?),
            _ => None,
        };
        let (gl, texels, packed, width, height, levels) = (self.gl, bound.texels.clone(), bound.packed.clone(), bound.width as i32, bound.height as i32, bound.levels as i32);
        self.worker.post_keeping(move || {
            disturb();
            // SAFETY: on the GL thread, the name is new and the texels cover
            // the texture
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                match packed {
                    Some((packing @ (Packing::Etc1 | Packing::Etc1A4), blocks)) => {
                        // each size's blocks, then with ETC1A4 its alphas.
                        // ETC1 is ETC2 too, whose storage takes every size,
                        // where ETC1's own way drew the smaller ones wrong
                        gl.tex_storage_2d(glow::TEXTURE_2D, levels, glow::COMPRESSED_RGB8_ETC2, width, height);
                        if let (Packing::Etc1A4, Some(alpha)) = (packing, alpha) {
                            gl.bind_texture(glow::TEXTURE_2D, Some(alpha));
                            gl.tex_storage_2d(glow::TEXTURE_2D, levels, glow::R8, width, height);
                        }
                        let mut at = 0;
                        for level in 0..levels {
                            let (width, height) = (width >> level, height >> level);
                            let (size, alphas) = ((width * height / 2) as usize, (width * height) as usize);
                            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                            gl.compressed_tex_sub_image_2d(glow::TEXTURE_2D, level, 0, 0, width, height, glow::COMPRESSED_RGB8_ETC2, glow::CompressedPixelUnpackData::Slice(&blocks[at..at + size]));
                            at += size;
                            if let (Packing::Etc1A4, Some(alpha)) = (packing, alpha) {
                                gl.bind_texture(glow::TEXTURE_2D, Some(alpha));
                                gl.tex_sub_image_2d(glow::TEXTURE_2D, level, 0, 0, width, height, glow::RED, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&blocks[at..at + alphas])));
                                at += alphas;
                            }
                        }
                    }
                    Some((packing, bytes)) => {
                        use glow::{GREEN, ONE, RED, ZERO};
                        let (internal, format, kind, swizzle, bytes_each) = match packing {
                            Packing::Rgb565 => (glow::RGB565, glow::RGB, glow::UNSIGNED_SHORT_5_6_5, None, 2),
                            Packing::Rgba5551 => (glow::RGB5_A1, glow::RGBA, glow::UNSIGNED_SHORT_5_5_5_1, None, 2),
                            Packing::Rgba4 => (glow::RGBA4, glow::RGBA, glow::UNSIGNED_SHORT_4_4_4_4, None, 2),
                            Packing::Luminance => (glow::R8, glow::RED, glow::UNSIGNED_BYTE, Some([RED, RED, RED, ONE]), 1),
                            Packing::Alpha => (glow::R8, glow::RED, glow::UNSIGNED_BYTE, Some([ZERO, ZERO, ZERO, RED]), 1),
                            Packing::LuminanceAlpha => (glow::RG8, glow::RG, glow::UNSIGNED_BYTE, Some([RED, RED, RED, GREEN]), 2),
                            Packing::Hilo => (glow::RG8, glow::RG, glow::UNSIGNED_BYTE, Some([ZERO, RED, GREEN, ONE]), 2),
                            Packing::Etc1 | Packing::Etc1A4 => unreachable!(),
                        };
                        if let Some(swizzle) = swizzle {
                            for (parameter, source) in [glow::TEXTURE_SWIZZLE_R, glow::TEXTURE_SWIZZLE_G, glow::TEXTURE_SWIZZLE_B, glow::TEXTURE_SWIZZLE_A].into_iter().zip(swizzle) {
                                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, source as i32);
                            }
                        }
                        gl.tex_storage_2d(glow::TEXTURE_2D, levels, internal, width, height);
                        let mut at = 0;
                        for level in 0..levels {
                            let (width, height) = (width >> level, height >> level);
                            let size = (width * height) as usize * bytes_each;
                            gl.tex_sub_image_2d(glow::TEXTURE_2D, level, 0, 0, width, height, format, kind, glow::PixelUnpackData::Slice(Some(&bytes[at..at + size])));
                            at += size;
                        }
                    }
                    None => {
                        gl.tex_storage_2d(glow::TEXTURE_2D, levels, glow::RGBA8, width, height);
                        let mut at = 0;
                        for level in 0..levels {
                            let (width, height) = (width >> level, height >> level);
                            let size = (width * height) as usize;
                            gl.tex_sub_image_2d(glow::TEXTURE_2D, level, 0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(texels[at..at + size].as_flattened())));
                            at += size;
                        }
                    }
                }
            }
        });
        self.textures.insert(key, Texture { texture, alpha, _texels: (bound.texels.clone(), bound.packed.clone()), used: self.flushes });
        Ok((texture, alpha))
    }

    /// a sampler magnifying bilinear or not, minifying as lod has it,
    /// between sizes when there are some.
    fn sampler(&mut self, linear: bool, s: Wrap, t: Wrap, lod: Lod, sizes: bool) -> Result<glow::Sampler, String> {
        let lod = Lod { bias: 0, ..lod };
        let lod = if sizes { lod } else { Lod { min: 0, max: 0, between: false, ..lod } };
        if let Some(&sampler) = self.samplers.get(&(linear, s, t, lod, sizes)) {
            return Ok(sampler);
        }
        let mode = |wrap: Wrap| match wrap {
            Wrap::Repeat => glow::REPEAT,
            Wrap::MirroredRepeat => glow::MIRRORED_REPEAT,
            _ => glow::CLAMP_TO_EDGE,
        } as i32;
        let filter = if linear { glow::LINEAR } else { glow::NEAREST } as i32;
        let minify = match (sizes, lod.linear, lod.between) {
            (false, false, _) => glow::NEAREST,
            (false, true, _) => glow::LINEAR,
            (true, false, false) => glow::NEAREST_MIPMAP_NEAREST,
            (true, true, false) => glow::LINEAR_MIPMAP_NEAREST,
            (true, false, true) => glow::NEAREST_MIPMAP_LINEAR,
            (true, true, true) => glow::LINEAR_MIPMAP_LINEAR,
        } as i32;
        // SAFETY: on the GL thread, which makes the names
        let sampler = pooled(&self.worker, self.gl, (&mut self.sampler_names, &mut self.samplers_coming), |gl| unsafe { gl.create_sampler() }.ok(), "sampler")?;
        let (gl, wrap_s, wrap_t) = (self.gl, mode(s), mode(t));
        self.worker.post_keeping(move || {
            // SAFETY: on the GL thread, the name is new
            unsafe {
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_MIN_FILTER, minify);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_MAG_FILTER, filter);
                gl.sampler_parameter_f32(sampler, glow::TEXTURE_MIN_LOD, lod.min as f32);
                gl.sampler_parameter_f32(sampler, glow::TEXTURE_MAX_LOD, lod.max as f32);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_S, wrap_s);
                gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_T, wrap_t);
            }
        });
        self.samplers.insert((linear, s, t, lod, sizes), sampler);
        Ok(sampler)
    }

    fn upload_tables(&mut self, tables: &Tables, procedural: &proctex::Tables, fog: &crate::fog::Table) {
        let _timed = timed(7);
        use std::hash::Hasher;
        let generation = (tables.generation(), procedural.generation(), fog.generation());
        if self.tables == Some(generation) {
            return;
        }
        self.tables = Some(generation);
        let mut key = [0u64; 26];
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
        key[25] = fog.generation();
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
                    fog.entries().as_flattened(),
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
        // they go with the next draw rather than on their own, which would
        // send the draws held so far off early
        self.tables_due = Some((id, bytes));
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
                view[3] * w,
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
        if self.flushes >= self.retire_at {
            self.retire(memory)?;
        }
        self.current()?;
        let vertex = match draw.geometry {
            Geometry::Shaded(shading) => self.vertex_stage(shading.unit, shading.semantics)?,
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

        let mut bound = [(self.blank, None); 6];
        let mut sources = [None; 3];
        let (mut enabled, mut split) = (0, 0);
        for (unit, texture) in draw.textures.iter().enumerate() {
            if let Some(texture) = texture {
                let sampler = self.sampler(texture.linear, texture.wrap_s, texture.wrap_t, texture.lod, texture.levels > 1 && texture.drawn.is_none())?;
                let image = match &texture.drawn {
                    Some(drawn) => match self.whole_surface(drawn).filter(|&index| index != color && self.surfaces[index].dirty.is_some()) {
                        Some(index) => {
                            self.surfaces[index].used = self.flushes;
                            sources[unit] = Some(self.surfaces[index].texture);
                            self.surfaces[index].texture
                        }
                        None => self.copy_texture(memory, drawn)?,
                    },
                    None => {
                        let (image, alpha) = self.texture(texture)?;
                        if let Some(alpha) = alpha {
                            bound[3 + unit] = (alpha, Some(sampler));
                            split |= 1 << unit;
                        }
                        image
                    }
                };
                bound[unit] = (image, Some(sampler));
                enabled |= 1 << unit;
            }
        }
        if draw.lighting.is_some() || draw.proctex || draw.fog {
            self.upload_tables(draw.tables, draw.proctex_tables, draw.fog_table);
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
        // most draws' fragment registers are the last one's, as they stay set
        // on the console until a list changes them, and so are the words
        static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let check = *CHECK.get_or_init(|| crate::switch("ZAKURO_CHECK_REUSE", "reused colour words checked"));
        let key = (draw.fragment_state, enabled | split << 4, sources.map(|source| source.is_some()), shaded, writes_depth, vertex);
        let same = self.fragment.as_ref().is_some_and(|(last, ..)| *last == key);
        let (words, program) = if same && !check {
            let (_, last, program) = self.fragment.as_ref().expect("the last draw's");
            let mut words = WORDS.take(UNIFORM_WORDS);
            words.extend_from_slice(last);
            (words, *program)
        } else {
            let mut words = WORDS.take(UNIFORM_WORDS);
            for base in STAGE_REGISTERS {
                words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], 0, 0, 0]);
            }
            let texture_config = (r[REG_TEXTURE_CONFIG] & !7) | enabled;
            words.extend([r[REG_UPDATE_BUFFER], r[REG_BUFFER_COLOR], r[REG_ALPHA_TEST], texture_config]);
            for (unit, (base, source)) in TEXTURE_UNIT_BASES.into_iter().zip(sources).enumerate() {
                // the bias in the top half of the in place flag's word
                let bias = ((r[base + 3] << 19) as i32 >> 19) as u16 as u32;
                words.extend([r[base + 2], r[base], source.is_some() as u32 | bias << 16, split >> unit & 1]);
            }
            let depth_flags = depth_map.w_buffer as u32 | (shaded as u32) << 1;
            words.extend([depth_flags, draw.lighting.is_some() as u32, depth_map.scale.to_bits(), depth_map.offset.to_bits()]);
            match draw.lighting {
                Some(lighting) => lighting.pack(&mut words),
                None => words.resize(UNIFORM_WORDS - PROCTEX_WORDS - COLOR_WORDS - FOG_WORDS, 0),
            }
            let base = proctex::REG_CONFIG;
            words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], r[base + 5], 0, 0]);
            words.extend(color_words(r));
            words.extend([r[crate::fog::REG_COLOR], 0, 0, 0]);

            let sources = sources.map(|source| source.is_some());
            let (mut constants, mut lighting) = fragment_constants(r, enabled, split, sources, depth_flags, writes_depth, draw.lighting.map(|_| &words[LIGHTING_BASE..]));
            if self.generic {
                constants = generic_constants(&constants);
            }
            static GENERIC_LIGHTING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if self.generic || *GENERIC_LIGHTING.get_or_init(|| crate::switch("ZAKURO_GENERIC_LIGHTING", "generic lighting")) {
                lighting.clear();
            }
            // what the manifest keeps of the draw, for a later run to work
            // the program out from again
            let flags = enabled | split << 4 | sources.into_iter().enumerate().map(|(unit, source)| (source as u32) << (8 + unit)).sum::<u32>() | depth_flags << 12 | (writes_depth as u32) << 16 | (draw.lighting.is_some() as u32) << 17;
            let inputs = || std::iter::once(format!("{flags:x}")).chain(key_registers().filter(|&register| r[register] != 0).map(|register| format!("{register:x}={:x}", r[register]))).collect::<Vec<_>>().join(" ");
            let mut exact = true;
            // a stand-in takes a program of its own only when one was built,
            // its own vertex program comes first and makes it needless
            let built = match draw.stand_in {
                true => Ok(self.programs.get(&(constants, writes_depth, lighting.clone(), vertex)).copied()),
                false => self.program(&constants, writes_depth, &lighting, vertex, draw.small, Some(&inputs)),
            };
            let program = match built {
                Ok(Some(program)) => program,
                // shaded on the CPU meanwhile
                Ok(None) if vertex != 0 => return Err(COMPILING.to_owned()),
                Ok(None) if draw.skippable => return Ok(()),
                Ok(None) => {
                    exact = false;
                    self.program(&generic_constants(&constants), writes_depth, &[], 0, true, None)?.ok_or("no generic program")?
                }
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
            if same {
                let (_, last, last_program) = self.fragment.as_ref().expect("the last draw's");
                if *last != words || *last_program != program {
                    let changed: Vec<usize> = (0..words.len()).filter(|&i| last.get(i) != words.get(i)).collect();
                    log::warn!("reused colour words differ at {changed:?}, program the same {}", *last_program == program);
                }
            }
            if exact {
                match &mut self.fragment {
                    Some((last_key, last, last_program)) => {
                        (*last_key, *last_program) = (key, program);
                        last.clear();
                        last.extend_from_slice(&words);
                    }
                    None => self.fragment = Some((key, words.to_vec(), program)),
                }
            }
            (words, program)
        };
        if self.stamped != Some((program, self.flushes)) {
            self.stamped = Some((program, self.flushes));
            self.program_used.insert(program, self.flushes);
        }

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

        // the viewport and scissor in the surfaces' pixels
        let k = self.quarters as f32 / 4.0;
        let up = |length: i32| (length as f32 * k).round() as i32;
        let geometry = match draw.geometry {
            Geometry::Triangles(triangles) => Submit::Triangles {
                viewport: [0, up(raise), up(draw.width as i32), up(draw.height as i32)],
                vertices: Self::triangles(triangles, (draw.width as f32, draw.height as f32), depth_map),
                count: (triangles.len() * 3) as i32,
            },
            Geometry::Shaded(shading) => {
                let (x, y, width, height) = shading.viewport;
                let viewport = [(x * k).round() as i32, ((y + raise as f32) * k).round() as i32, (width * k).round() as i32, (height * k).round() as i32];
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
                        let mut bytes = BYTES.take(fetched.arrays.iter().map(|array| array.bytes.len() + 16).sum());
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
                        let mut indices = BYTES.take(kept * 4);
                        if range.1 <= 0xFFFF {
                            shading.indices[..kept].iter().for_each(|&index| indices.extend_from_slice(&(index as u16).to_le_bytes()));
                        } else {
                            shading.indices[..kept].iter().for_each(|&index| indices.extend_from_slice(&index.to_le_bytes()));
                        }
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
            scissor: [up(left), up(bottom), up(right) - up(left), up(top) - up(bottom)],
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
            tables: self.tables_due.take(),
        };
        self.worker.post_draw(job, self.gl);
        Ok(())
    }

    /// lets the compiler thread end after the program it builds, and the GL
    /// thread finish what it was sent, before the process ends: the driver's
    /// exit freeing what a build still used corrupted the heap.
    pub fn stop(&mut self) {
        if let Some(Compiler { jobs, thread, done, .. }) = self.compiler.take() {
            drop((jobs, done));
            let _ = thread.join();
        }
        self.worker.run(|| ());
    }

    /// the surfaces of two eyes' buffers, colour then depth, alike in kind
    /// and size, a copy can go between.
    fn eye_surfaces(&self, first: [(u32, u32); 2], other: [(u32, u32); 2]) -> Option<[(usize, usize); 2]> {
        let find = |(addr, size): (u32, u32), depth: bool| self.surfaces.iter().position(|s| s.addr == addr && s.tiled && s.size() == size && matches!(s.kind, Kind::Depth(_)) == depth);
        let pairs = [(find(first[0], false)?, find(other[0], false)?), (find(first[1], true)?, find(other[1], true)?)];
        let shape = |i: usize| (self.surfaces[i].width, self.surfaces[i].height, self.surfaces[i].kind);
        pairs.iter().all(|&(a, b)| shape(a) == shape(b)).then_some(pairs)
    }

    pub(crate) fn can_mirror(&self, first: [(u32, u32); 2], other: [(u32, u32); 2]) -> bool {
        self.eye_surfaces(first, other).is_some()
    }

    /// copies one eye's colour and depth over the other's, the draws the
    /// other left out as stereo twins.
    pub(crate) fn mirror(&mut self, first: [(u32, u32); 2], other: [(u32, u32); 2]) -> Result<(), String> {
        let pairs = self.eye_surfaces(first, other).ok_or("an eye's buffers are gone")?;
        let mut copies = Vec::new();
        for (i, &(from, to)) in pairs.iter().enumerate() {
            let (read, draw) = match i {
                0 => (self.framebuffer(Some(from), None)?, self.framebuffer(Some(to), None)?),
                _ => (self.framebuffer(None, Some(from))?, self.framebuffer(None, Some(to))?),
            };
            let mask = if i == 0 { glow::COLOR_BUFFER_BIT } else { glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT };
            let size = (self.up(self.surfaces[from].width) as i32, self.up(self.surfaces[from].height) as i32);
            copies.push((read, draw, mask, size, self.surfaces[from].texture, self.surfaces[to].texture));
        }
        let gl = self.gl;
        self.worker.post_keeping(move || {
            for &(.., from, to) in &copies {
                flush_clears(Some(from));
                flush_clears(Some(to));
            }
            disturb();
            // SAFETY: on the GL thread, the framebuffers are the surfaces'
            // and both of a copy the same size
            unsafe {
                gl.disable(glow::SCISSOR_TEST);
                gl.color_mask(true, true, true, true);
                gl.depth_mask(true);
                gl.stencil_mask(0xFF);
                for (read, draw, mask, (width, height), ..) in copies {
                    gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
                    gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(draw));
                    gl.blit_framebuffer(0, 0, width, height, 0, 0, width, height, mask, glow::NEAREST);
                }
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
        });
        for (_, to) in pairs {
            let height = self.surfaces[to].height;
            self.surfaces[to].drew((0, height));
            self.surfaces[to].used = self.flushes;
        }
        Ok(())
    }

    /// sends the draws held back.
    pub fn flush_draws(&self) {
        self.worker.flush();
    }

    fn vertex_stage(&mut self, unit: &ShaderUnit, semantics: [u32; SEMANTICS]) -> Result<u64, String> {
        let key = (unit.fingerprint(), unit.entry_point, semantics);
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
                self.vertex_inputs.insert(hash, key);
                if let (false, Some(path)) = (self.preloading, manifest_path()) {
                    let file = vertex_path(&path, key.0);
                    let bytes: Vec<u8> = unit.program.iter().chain(unit.descriptors.iter()).flat_map(|word| word.to_le_bytes()).collect();
                    if std::fs::metadata(&file).map(|meta| meta.len()).ok() != Some(bytes.len() as u64) {
                        let _ = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&file, bytes));
                    }
                }
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
        let registers = || (0..INPUT_REGISTERS).filter(move |r| read & (1 << r) != 0);
        let count = registers().count();
        let mut words = WORDS.take(452);
        words.extend(unit.float_uniforms.iter().flatten().map(|value| value.to_bits()));
        for [count, start, step, _] in unit.int_uniforms {
            words.extend([count as u32, start as u32, step as i8 as u32, 0]);
        }
        words.extend([unit.bool_uniforms as u32, unit.entry_point, count as u32, 0]);
        words.extend([depth_map.scale.to_bits(), depth_map.offset.to_bits(), 0, 0]);
        let (x, y, width, height) = shading.viewport;
        words.extend([x, y, width, height].map(f32::to_bits));
        // the vertices the GPU fetches itself have no inputs to pack
        if shading.fetched.is_some() {
            return (words, Vec::new(), count);
        }
        let mut inputs = BYTES.take((shading.inputs.len() * count).max(1) * 16);
        for input in shading.inputs {
            for register in registers() {
                for value in input[register] {
                    inputs.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        if inputs.is_empty() {
            inputs.resize(16, 0);
        }
        (words, inputs, count)
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
    fn a_texture_inside_a_drawing_of_another_width_is_copied_on_the_gpu() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let format = ColorFormat::Rgba8;
        let (base, width, height) = (0x1000u32, 256u32, 128u32);
        let mut memory = Flat(vec![0; 0x40000]);
        for (i, byte) in memory.0[base as usize..(base + width * height * 4) as usize].iter_mut().enumerate() {
            *byte = (i * 31 + i / 7) as u8;
        }
        let index = gles.surface_rows(&mut memory, base, (width, height), Kind::Color(format), true, None).unwrap();
        gles.surfaces[index].drew((0, height));
        // Moon's bloom samples the smaller buffers it drew in the bigger one's memory
        for (offset, w, h) in [(0x10000u32, 64u32, 32u32), (0x8000, 128, 64), (0, 512, 16)] {
            let texture = DrawnTexture { addr: base + offset, width: w, height: h, format };
            assert!(gles.retiled_sources(&texture).is_some());
            gles.copy_texture(&mut memory, &texture).unwrap();
            let mut expected = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let at = (texture.addr + pixel_offset(true, x, y, w, 4) as u32) as usize;
                    let o = ((y * w + x) * 4) as usize;
                    expected[o..o + 4].copy_from_slice(&format.decode(&memory.0[at..at + 4]));
                }
            }
            assert_eq!(read_copy(&mut gles, &texture), expected, "{w}x{h} at {offset:X}");
        }
        assert!(gles.surfaces[index].dirty.is_some(), "the drawing stayed on the GPU");
        // and over two drawings, as Kingdom Hearts 3D's picture and the glow
        // below it
        let (top, below) = (0x20000u32, 0x20000 + 256 * 64 * 4);
        for (i, byte) in memory.0[top as usize..(top + 0x20000) as usize].iter_mut().enumerate() {
            *byte = (i * 13 + i / 5) as u8;
        }
        for (addr, w, h) in [(top, 256, 64), (below, 128, 64)] {
            let index = gles.surface_rows(&mut memory, addr, (w, h), Kind::Color(format), true, None).unwrap();
            gles.surfaces[index].drew((0, h));
        }
        // and on past them into rows nothing drew, from memory, a small
        // drawing of another kind there written back first, as Smash's
        let small = gles.surface_rows(&mut memory, top + 256 * 112 * 4, (64, 16), Kind::Color(ColorFormat::Rgb5A1), true, None).unwrap();
        gles.surfaces[small].drew((0, 16));
        for (height, pieces, apart) in [(96, 2, 0), (128, 3, 1)] {
            let texture = DrawnTexture { addr: top, width: 256, height, format };
            assert_eq!(gles.retiled_sources(&texture).map(|(pieces, apart)| (pieces.len(), apart.len())), Some((pieces, apart)));
            gles.copy_texture(&mut memory, &texture).unwrap();
            let mut expected = vec![0u8; (256 * height * 4) as usize];
            for y in 0..height {
                for x in 0..256 {
                    let at = (top + pixel_offset(true, x, y, 256, 4) as u32) as usize;
                    let o = ((y * 256 + x) * 4) as usize;
                    expected[o..o + 4].copy_from_slice(&format.decode(&memory.0[at..at + 4]));
                }
            }
            assert_eq!(read_copy(&mut gles, &texture), expected, "256x{height} over two drawings");
        }
        assert!(gles.surfaces[small].dirty.is_none(), "the small drawing was written back");
    }

    #[test]
    fn a_game_program_gets_its_combiners_as_straight_code() {
        let fragment = fragment_source(&[0u32; FRAGMENT_CONSTANTS], false, &[]);
        assert!(!fragment.lines().any(|line| line.trim_start().starts_with("combine_stage(")), "the combiner stages went in as calls");
    }

    #[test]
    fn every_generic_stand_in_builds() {
        let gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let worker = gles.worker.clone();
        let times = worker.run(|| {
            let gl = GL_CONTEXT.with(|gl| gl.get()).expect("the GL thread's context");
            let mut times = Vec::new();
            for writes_depth in [false, true] {
                for unbordered in [0, 0x400] {
                    for lighting in [0, 1] {
                        let mut constants = [0u32; FRAGMENT_CONSTANTS];
                        constants[25] = unbordered;
                        constants[26] = lighting;
                        constants[28] = if writes_depth { WRITES_DEPTH } else { 0 };
                        let fragment = fragment_source(&generic_constants(&constants), writes_depth, &[]);
                        let started = std::time::Instant::now();
                        if let Err(error) = build_program(gl, VERTEX_SOURCE, &fragment, None) {
                            panic!("a stand-in did not build, {error}");
                        }
                        times.push(started.elapsed().as_millis());
                    }
                }
            }
            times
        });
        eprintln!("the stand-ins built in {times:?} ms");
    }

    #[test]
    fn separable_stages_put_together_validate() {
        let gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        if (0..PIPELINE_FNS.len()).any(|index| pipeline_fn(index) == 0) {
            eprintln!("no program pipelines here, skipped");
            return;
        }
        let worker = gles.worker.clone();
        let failures = worker.run(|| {
            let gl = GL_CONTEXT.with(|gl| gl.get()).expect("the GL thread's context");
            let mut failures = Vec::new();
            for writes_depth in [false, true] {
                for lighting in [0, 1] {
                    let mut constants = [0u32; FRAGMENT_CONSTANTS];
                    constants[26] = lighting;
                    constants[28] = if writes_depth { WRITES_DEPTH } else { 0 };
                    let fragment = fragment_source(&generic_constants(&constants), writes_depth, &[]);
                    let token = build_separable(gl, VERTEX_SOURCE, &fragment).expect("the stages build");
                    let (vertex, fragment) = SEPARATE.lock().unwrap().as_ref().unwrap()[&(SHARE_GROUP.get(), token)];
                    // SAFETY: on the GL thread with its context current
                    unsafe {
                        let generate: extern "system" fn(i32, *mut u32) = std::mem::transmute(pipeline_fn(1));
                        let stages_of: extern "system" fn(u32, u32, u32) = std::mem::transmute(pipeline_fn(3));
                        let mut pipeline = 0u32;
                        generate(1, &mut pipeline);
                        stages_of(pipeline, VERTEX_SHADER_BIT, vertex.0.get());
                        stages_of(pipeline, FRAGMENT_SHADER_BIT, fragment.0.get());
                        let validate: extern "system" fn(u32) = std::mem::transmute(pipeline_fn(5));
                        let status: extern "system" fn(u32, u32, *mut i32) = std::mem::transmute(pipeline_fn(6));
                        let log: extern "system" fn(u32, i32, *mut i32, *mut u8) = std::mem::transmute(pipeline_fn(7));
                        validate(pipeline);
                        let mut valid = 0;
                        status(pipeline, glow::VALIDATE_STATUS, &mut valid);
                        if valid == 0 {
                            let mut text = vec![0u8; 4096];
                            let mut length = 0;
                            log(pipeline, text.len() as i32, &mut length, text.as_mut_ptr());
                            failures.push(format!("depth {writes_depth}, lighting {lighting}: {}", String::from_utf8_lossy(&text[..length.max(0) as usize])));
                        }
                    }
                    delete_program(gl, token);
                }
            }
            failures
        });
        assert!(failures.is_empty(), "pipelines that did not validate: {failures:#?}");
    }

    #[test]
    fn the_compiler_queue_takes_what_draws_need_before_stand_ins() {
        let job = |name: &str, stand_in: bool| {
            let mut constants = [0u32; FRAGMENT_CONSTANTS];
            constants[29] = stand_in as u32;
            Compile { key: (constants, false, Vec::new(), 0), vertex: name.to_owned(), fragment: String::new(), cached: None }
        };
        let queue = Queue::new();
        for (name, stand_in) in [("stand-in 1", true), ("stand-in 2", true), ("draw 1", false), ("stand-in 3", true), ("draw 2", false)] {
            assert!(queue.send(job(name, stand_in)).is_ok());
        }
        let order: Vec<String> = std::iter::from_fn(|| {
            let mut jobs = queue.jobs.lock().unwrap();
            jobs.0.pop_front().or_else(|| jobs.1.pop_front()).map(|job| job.vertex)
        })
        .collect();
        assert_eq!(order, ["draw 1", "draw 2", "stand-in 1", "stand-in 2", "stand-in 3"]);
        // closed, a waiting thread is let go and nothing more is taken
        queue.close();
        assert!(queue.take().is_none());
        assert!(queue.send(job("late", false)).is_err());
    }

    #[test]
    fn programs_sharing_a_vertex_stage_compile_it_once() {
        let gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        // separable stages are kept as programs of their own, and loaded
        // from the program cache before any shader is compiled
        if separable() {
            return;
        }
        const VERTEX: &str = "#version 310 es\nvoid main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
        let fragment = |red: f32| format!("#version 310 es\nprecision mediump float;\nlayout(location = 0) out vec4 color;\nvoid main() {{ color = vec4({red:.1}, 0.0, 0.0, 1.0); }}\n");
        let (first, second) = (fragment(0.5), fragment(1.0));
        let worker = gles.worker.clone();
        let kept = worker.run(move || {
            let gl = GL_CONTEXT.with(|gl| gl.get()).expect("the GL thread's context");
            assert!(build_program(gl, VERTEX, &first, None).is_ok());
            let after_one = SHADERS.with(|shaders| shaders.borrow().len());
            assert!(build_program(gl, VERTEX, &second, None).is_ok());
            // a program whose fragment stage fails leaves what was kept usable
            assert!(build_program(gl, VERTEX, "#version 310 es\nthis is not glsl\n", None).is_err());
            assert!(build_program(gl, VERTEX, &first, None).is_ok());
            (after_one, SHADERS.with(|shaders| shaders.borrow().len()))
        });
        // the vertex stage and the first fragment stage, then only the second
        // fragment stage new
        assert_eq!(kept, (2, 3));
    }

    #[test]
    fn a_shrinking_transfer_from_memory_into_a_drawing_matches_the_console() {
        // memory first, as Fire Emblem Awakening's, or the drawing first, as
        // Metroid Samus Returns'
        shrinking_transfer(false);
        shrinking_transfer(true);
    }

    fn shrinking_transfer(drawing_first: bool) {
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
        let drawing = if drawing_first { input } else { input + lead / 8 * 8 * width * 4 };
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
        assert_eq!(&memory.0[output as usize..output as usize + expected.len()], &expected[..], "drawing first {drawing_first}");
    }

    #[test]
    fn a_transfer_out_of_a_linear_picture_stays_on_the_gpu() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        // a picture an earlier transfer left linear, transferred back into a
        // tiled texture, as Paper Mario: Sticker Star does every frame
        let format = ColorFormat::Rgba8;
        let (input, output, width, height) = (0x1000u32, 0x10000u32, 32u32, 48u32);
        let mut memory = Flat(vec![0; 0x20000]);
        for (i, byte) in memory.0[input as usize..(input + width * height * 4) as usize].iter_mut().enumerate() {
            *byte = (i * 37 + i / 5) as u8;
        }
        for flip in [false, true] {
            let index = gles.surface_rows(&mut memory, input, (width, height), Kind::Color(format), false, None).unwrap();
            gles.surfaces[index].drew((0, height));
            let transfer = Transfer {
                input,
                output,
                input_width: width,
                input_height: height,
                output_width: width,
                output_height: height,
                copy: (width, height),
                scale: (1, 1),
                flip,
                input_linear: true,
                output_tiled: true,
                input_format: format,
                output_format: format,
                column: 0,
            };
            let mut expected = vec![0u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let from = if flip { height - 1 - y } else { y };
                    let at = (input + pixel_offset(false, x, from, width, 4) as u32) as usize;
                    let to = pixel_offset(true, x, y, width, 4);
                    expected[to..to + 4].copy_from_slice(&memory.0[at..at + 4]);
                }
            }
            assert!(gles.display_transfer(&mut memory, &transfer).unwrap());
            assert!(gles.surfaces.iter().any(|s| s.addr == input && !s.tiled && s.dirty.is_some()), "the picture stayed on the GPU");
            gles.flush(&mut memory).unwrap();
            assert_eq!(&memory.0[output as usize..output as usize + expected.len()], &expected[..], "flip {flip}");
        }
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
    fn a_copy_into_a_wider_texture_whose_padding_holds_the_drawing_stays_on_the_gpu() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        // Yo-kai Watch 2's top screen, 240x400, into a texture 256 wide that
        // ends 512 bytes into it
        let (width, height, padded) = (240u32, 400u32, 256u32);
        let source = 0x80000u32;
        let (line, skip) = (8 * width * 4, 8 * (padded - width) * 4);
        let target = source - (height / 8 * (line + skip) - skip);
        let size = width * height * 4;
        let mut memory = Flat(vec![0; 0x100000]);
        for (i, byte) in memory.0[source as usize..(source + size) as usize].iter_mut().enumerate() {
            *byte = (i * 37 + i / 5) as u8;
        }
        let drawn = memory.0[source as usize..(source + size) as usize].to_vec();
        let index = gles.surface_rows(&mut memory, source, (width, height), Kind::Color(ColorFormat::Rgba8), true, None).unwrap();
        gles.surfaces[index].drew((0, height));
        assert!(gles.texture_copy(&mut memory, source, target, size, (line, 0), (line, skip)).unwrap());
        assert!(gles.surfaces[index].dirty.is_some(), "the drawing was not read back");
        gles.flush(&mut memory).unwrap();
        for row in 0..height / 8 {
            let at = (target + row * (line + skip)) as usize;
            assert_eq!(&memory.0[at..at + line as usize], &drawn[(row * line) as usize..((row + 1) * line) as usize], "row of tiles {row}");
        }
        assert_eq!(&memory.0[source as usize..(source + size) as usize], &drawn[..], "the drawing under the padding");
    }

    #[test]
    fn a_texture_copy_running_past_a_drawing_takes_the_rest_from_memory() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        let (source, target, width, height) = (0x1000u32, 0x8000u32, 32u32, 24u32);
        let tile_rows = 8 * width * 4;
        let mut memory = Flat(vec![0; 0x20000]);
        for (i, byte) in memory.0[source as usize..(source + 4 * tile_rows) as usize].iter_mut().enumerate() {
            *byte = (i * 37 + i / 5) as u8;
        }
        let index = gles.surface_rows(&mut memory, source, (width, height), Kind::Color(ColorFormat::Rgba8), true, None).unwrap();
        gles.surfaces[index].drew((0, height));
        // a row of tiles more than the drawing has, as Federation Force copies 64 rows out of 56
        assert!(gles.texture_copy(&mut memory, source, target, 4 * tile_rows, (0, 0), (0, 0)).unwrap());
        gles.flush(&mut memory).unwrap();
        assert_eq!(&memory.0[target as usize..(target + 4 * tile_rows) as usize], &memory.0[source as usize..(source + 4 * tile_rows) as usize]);
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

    #[test]
    fn a_drawing_in_another_format_over_a_surface_goes_over_through_memory_s_bytes() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => {
                eprintln!("no OpenGL ES here, skipped: {error}");
                return;
            }
        };
        // Kingdom Hearts 3D draws the same 128 KiB as 256x256 RGBA4 and as
        // 128x256 RGBA8
        let (addr, len) = (0x1000u32, 0x20000usize);
        let mut memory = Flat(vec![0; 0x30000]);
        let fill = |memory: &mut Flat, seed: usize| {
            for (i, byte) in memory.0[addr as usize..addr as usize + len].iter_mut().enumerate() {
                *byte = (i * seed + i / 7) as u8;
            }
        };
        fill(&mut memory, 11);
        let rgba4 = gles.surface_rows(&mut memory, addr, (256, 256), Kind::Color(ColorFormat::Rgba4), true, None).unwrap();
        fill(&mut memory, 29);
        let drawn = memory.0.clone();
        let rgba8 = gles.surface_rows(&mut memory, addr, (128, 256), Kind::Color(ColorFormat::Rgba8), true, None).unwrap();
        gles.surfaces[rgba8].drew((0, 256));
        // memory no longer holds the drawing, only the GPU does
        fill(&mut memory, 3);
        assert!(gles.reinterpret(rgba4, rgba8).unwrap());
        assert_eq!((gles.surfaces[rgba4].dirty, gles.surfaces[rgba8].dirty), (Some((0, 256)), None));
        let framebuffer = gles.framebuffer(Some(rgba4), None).unwrap();
        let (gl, worker) = (gles.gl, gles.worker.clone());
        let read = worker.run(move || unsafe {
            let mut data = vec![0u8; 256 * 256 * 4];
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_pixels(0, 0, 256, 256, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            data
        });
        for y in 0..256u32 {
            for x in 0..256u32 {
                let at = (addr + pixel_offset(true, x, y, 256, 2) as u32) as usize;
                let o = (((255 - y) * 256 + x) * 4) as usize;
                assert_eq!(read[o..o + 4], ColorFormat::Rgba4.decode(&drawn[at..at + 2]), "at ({x}, {y})");
            }
        }
        // and back, the bytes the RGBA8 drawing had
        assert!(gles.reinterpret(rgba8, rgba4).unwrap());
        let framebuffer = gles.framebuffer(Some(rgba8), None).unwrap();
        let read = worker.run(move || unsafe {
            let mut data = vec![0u8; 128 * 256 * 4];
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_pixels(0, 0, 128, 256, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            data
        });
        for y in 0..256u32 {
            for x in 0..128u32 {
                let at = (addr + pixel_offset(true, x, y, 128, 4) as u32) as usize;
                let o = (((255 - y) * 128 + x) * 4) as usize;
                assert_eq!(read[o..o + 4], ColorFormat::Rgba8.decode(&drawn[at..at + 4]), "back at ({x}, {y})");
            }
        }
    }
}
