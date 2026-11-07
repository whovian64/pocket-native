//! building GL programs: the cache on disk and the manifest of what a
//! game used, the compiler threads and their queue, separable stages, and
//! noting a shader the driver's compiler crashed on.

use super::*;

/// the diagnostic feature's span around one call, `$body` itself without
/// it: the call, its arguments and their order stay as they are either way.
/// `none` records no context, before a worker has one.
#[cfg(feature = "shader-attribution")]
macro_rules! observe {
    ($kind:ident, none, $aux:expr, $body:expr) => {{
        let span = attribution::Span::begin(attribution::Kind::$kind, None, $aux);
        let value = $body;
        span.end(attribution::Observed::outcome(&value));
        value
    }};
    ($kind:ident, $gl:expr, $aux:expr, $body:expr) => {{
        let span = attribution::Span::begin(attribution::Kind::$kind, Some($gl), $aux);
        let value = $body;
        span.end(attribution::Observed::outcome(&value));
        value
    }};
}

#[cfg(not(feature = "shader-attribution"))]
macro_rules! observe {
    ($kind:ident, $gl:expr, $aux:expr, $body:expr) => {
        $body
    };
}

pub(super) const MANIFEST_HEADER: &str = "zakuro-gles 5";

/// the vertex programs' words, kept by fingerprint, so a run translates
/// them again with its own translator.
pub(super) fn vertex_path(manifest: &std::path::Path, fingerprint: u64) -> std::path::PathBuf {
    let mut dir = manifest.as_os_str().to_owned();
    dir.push(".vertex");
    std::path::PathBuf::from(dir).join(format!("{fingerprint:016x}.pica"))
}

pub(super) fn manifest_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("ZAKURO_SHADER_MANIFEST").filter(|value| !value.is_empty()) {
        return Some(path.into());
    }
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let dir = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?;
    Some(dir.join("zakuro").join("gles").join("programs.txt"))
}

pub(super) fn program_cache_path(renderer: &str, vertex: &str, fragment: &str) -> Option<std::path::PathBuf> {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| crate::switch("ZAKURO_NO_PROGRAM_BINARY_CACHE", "disk program binaries disabled; manifest and in-process shader reuse remain enabled")) {
        return None;
    }
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let dir = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?;
    Some(dir.join("zakuro").join("gles").join(format!("{:016x}.bin", fnv(&[renderer, vertex, fragment]))))
}

#[cfg(test)]
thread_local! {
    static BINARY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read a driver binary only when there is somewhere to retain it. Evaluating
/// get_program_binary in a tuple with an absent path still performs the costly
/// driver operation, including on Android installations without HOME.
fn write_program_binary(gl: &glow::Context, program: glow::Program, cached: Option<&std::path::Path>) {
    let Some(path) = cached else { return };
    #[cfg(test)]
    BINARY_READS.set(BINARY_READS.get() + 1);
    // SAFETY: the linked program belongs to the current compiler/GL context.
    if let Some(binary) = observe!(BinaryRetrieve, gl, 0, unsafe { gl.get_program_binary(program) }) {
        let mut bytes = binary.format.to_le_bytes().to_vec();
        bytes.extend_from_slice(&binary.buffer);
        let _ = observe!(CacheWrite, gl, bytes.len() as u64, path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(path, bytes)));
    }
}

/// what a draw gives back while the program it needs is still compiling, the
/// vertices then shaded on the CPU.
pub(crate) const COMPILING: &str = "its program is compiling";

/// programs the compiler thread has yet to build.
pub(super) static BUILDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// how many programs are still being built, for the screen to say so.
pub fn compiling() -> usize {
    BUILDING.load(std::sync::atomic::Ordering::Relaxed)
}

/// the generic stand-ins of those, which a first run, or one after an
/// update, builds once for every game, the slowest programs there are.
pub(super) static BUILDING_STAND_INS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// how many of the generic stand-ins are still being built, and how many
/// there are, for the screen to say so: a long wait the first time is them.
pub fn compiling_stand_ins() -> (usize, usize) {
    (BUILDING_STAND_INS.load(std::sync::atomic::Ordering::Relaxed), STAND_IN_COUNT)
}

/// the stand-ins prepare_stand_ins makes: with and without depth written,
/// with and without the border, with and without lighting.
pub(super) const STAND_IN_COUNT: usize = 8;

/// held while the GL thread or the compiler thread compiles: Mali's driver
/// on the RX6H crashed in the compiler thread with both at it.
pub(super) static COMPILER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// the sources the driver compiles or links and the thread at it, for a
/// crash in its compiler to name them in the log and keep them out of the
/// next runs: Mali's r13p0 crashes on the same source every time.
pub(super) struct AtCompiler {
    pub(super) thread: std::sync::atomic::AtomicI64,
    pub(super) fingerprint: std::sync::atomic::AtomicU64,
    pub(super) sources: [(std::sync::atomic::AtomicPtr<u8>, std::sync::atomic::AtomicUsize); 2],
}

pub(super) static AT_COMPILER: AtCompiler = AtCompiler {
    thread: std::sync::atomic::AtomicI64::new(0),
    fingerprint: std::sync::atomic::AtomicU64::new(0),
    sources: [
        (std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), std::sync::atomic::AtomicUsize::new(0)),
        (std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), std::sync::atomic::AtomicUsize::new(0)),
    ],
};

/// where the fingerprints of what the compiler crashed on go, for the crash
/// handler to open.
pub(super) static CRASHED_PATH: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

pub(super) fn crashed_path() -> Option<std::path::PathBuf> {
    manifest_path().map(|path| path.with_file_name("shaders-crashed.txt"))
}

/// the fingerprints a driver crashed on in an earlier run.
pub(super) fn crashed() -> rustc_hash::FxHashSet<u64> {
    let Some(path) = crashed_path() else { return Default::default() };
    if let Ok(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        let _ = CRASHED_PATH.set(path);
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    text.lines().filter_map(|line| u64::from_str_radix(line.trim(), 16).ok()).collect()
}

/// marks what the driver is about to compile, nothing once it is done.
pub(super) fn at_compiler(sources: [&str; 2]) {
    use std::sync::atomic::Ordering::Relaxed;
    let present: Vec<&str> = sources.into_iter().filter(|source| !source.is_empty()).collect();
    AT_COMPILER.fingerprint.store(fnv(&present), Relaxed);
    for ((pointer, len), source) in AT_COMPILER.sources.iter().zip(sources) {
        pointer.store(source.as_ptr().cast_mut(), Relaxed);
        len.store(source.len(), Relaxed);
    }
    // SAFETY: gettid has no arguments and cannot fail
    let thread = if present.is_empty() { 0 } else { unsafe { libc::syscall(libc::SYS_gettid) } };
    AT_COMPILER.thread.store(thread, Relaxed);
}

/// clears at_compiler's mark once a build is over, however it ends: the
/// driver may do the work when asked how it went or at first use, not only
/// in the compile and link calls.
pub(super) struct Watched;

impl Drop for Watched {
    fn drop(&mut self) {
        at_compiler(["", ""]);
    }
}

/// from a fatal signal's handler: when the crashing thread was in the
/// driver's compiler, writes what it compiled into the log and its
/// fingerprint into the list the next runs leave out.
///
/// # Safety
///
/// on the crashing thread in its signal handler, log a file to write to;
/// nothing here allocates
pub unsafe fn note_compiler_crash(log: libc::c_int) {
    use std::sync::atomic::Ordering::Relaxed;
    if AT_COMPILER.thread.load(Relaxed) != libc::syscall(libc::SYS_gettid) {
        return;
    }
    let write = |file: libc::c_int, bytes: &[u8]| {
        libc::write(file, bytes.as_ptr().cast(), bytes.len());
    };
    write(log, b"the driver's compiler crashed on this, the next runs draw what needs it in software:\n");
    for (pointer, len) in &AT_COMPILER.sources {
        let pointer = pointer.load(Relaxed);
        if !pointer.is_null() {
            write(log, std::slice::from_raw_parts(pointer, len.load(Relaxed)));
            write(log, b"\n");
        }
    }
    let Some(path) = CRASHED_PATH.get() else { return };
    let file = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o644);
    if file >= 0 {
        let fingerprint = AT_COMPILER.fingerprint.load(Relaxed);
        let mut line = [b'\n'; 17];
        for (i, digit) in line[..16].iter_mut().enumerate() {
            *digit = b"0123456789abcdef"[(fingerprint >> (60 - i * 4)) as usize & 15];
        }
        write(file, &line);
        libc::close(file);
    }
}

/// how many threads compile programs, ZAKURO_COMPILERS, 1 to 4: more build
/// a first run's programs sooner on a driver that takes them at once, which
/// not all do, the RX6H's Mali crashing with two. Three on Android, where
/// the RG DS's Mali-G52 built its stand-ins in 15 s rather than 36 with
/// them, one elsewhere.
pub(super) fn compilers() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        let default = if cfg!(target_os = "android") { 3 } else { 1 };
        let count = std::env::var("ZAKURO_COMPILERS").ok().and_then(|value| value.parse().ok()).unwrap_or(default).clamp(1, 4);
        if count > 1 {
            log::info!("{count} threads compiling programs, at once (ZAKURO_COMPILERS)");
        }
        count
    })
}

/// COMPILER_LOCK, held while compiling, unless several compiler threads
/// are asked for, which only works where the driver lets them all at it.
pub(super) fn compiler_lock() -> Option<std::sync::MutexGuard<'static, ()>> {
    (compilers() == 1).then(|| COMPILER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

pub(super) fn link(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
    let _compiling = compiler_lock();
    let _watched = Watched;
    #[cfg(feature = "shader-attribution")]
    let mut span = attribution::Span::begin(attribution::Kind::Graphics, Some(gl), 0);
    at_compiler([vertex, fragment]);
    // SAFETY: on the GL thread, the shaders are deleted once linked
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (stage, source) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let shader = gl.create_shader(stage)?;
            gl.shader_source(shader, source);
            observe!(ShaderCompile, gl, stage as u64, gl.compile_shader(shader));
            if !observe!(CompileStatus, gl, stage as u64, gl.get_shader_compile_status(shader)) {
                let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                shaders.into_iter().chain([shader]).for_each(|shader| gl.delete_shader(shader));
                gl.delete_program(program);
                #[cfg(feature = "shader-attribution")]
                span.set(attribution::COMPILE_FAILED);
                return Err(error);
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        observe!(ProgramLink, gl, 0, gl.link_program(program));
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !observe!(LinkStatus, gl, 0, gl.get_program_link_status(program)) {
            let error = format!("could not link a program, {}", gl.get_program_info_log(program));
            gl.delete_program(program);
            #[cfg(feature = "shader-attribution")]
            span.set(attribution::LINK_FAILED);
            return Err(error);
        }
        #[cfg(feature = "shader-attribution")]
        span.set(attribution::COMPILED);
        Ok(program)
    }
}

/// the program pipeline calls separable programs need, which glow does not
/// wrap, as the loader found them, in this order.
/// The last three only check a pipeline, in the tests.
pub(super) const PIPELINE_NAMES: [&str; 8] = ["glProgramParameteri", "glGenProgramPipelines", "glDeleteProgramPipelines", "glUseProgramStages", "glBindProgramPipeline", "glValidateProgramPipeline", "glGetProgramPipelineiv", "glGetProgramPipelineInfoLog"];
pub(super) static PIPELINE_FNS: [std::sync::atomic::AtomicUsize; 8] = [const { std::sync::atomic::AtomicUsize::new(0) }; 8];
pub(super) const PIPELINE_NEEDED: usize = 5;

pub(super) fn pipeline_fn(index: usize) -> usize {
    PIPELINE_FNS[index].load(std::sync::atomic::Ordering::Relaxed)
}

/// ZAKURO_SEPARABLE_SHADERS: each program's two stages built as programs of
/// their own and put together at draw time in a program pipeline, as
/// Azahar's OpenGL renderer does on desktops, rather than linked together
/// for every pairing: a vertex stage is linked once whatever fragment
/// stages it meets, which on Mali took 300 ms to 1.8 s a pairing. Azahar
/// keeps it off on OpenGL ES, where drivers have had trouble with it, so it
/// is off unless asked for, and needs the driver's pipeline calls.
pub(super) fn separable() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let asked = crate::switch("ZAKURO_SEPARABLE_SHADERS", "shader stages built apart and put together when drawn");
        let loaded = (0..PIPELINE_NEEDED).all(|index| pipeline_fn(index) != 0);
        if asked && !loaded {
            log::warn!("the driver has no program pipelines, shader stages stay linked together");
        }
        asked && loaded
    })
}

pub(super) const PROGRAM_SEPARABLE: u32 = 0x8258;
pub(super) const VERTEX_SHADER_BIT: u32 = 0x1;
pub(super) const FRAGMENT_SHADER_BIT: u32 = 0x2;

/// Ownership is separate from GL destruction: compiler jobs can reserve a
/// stage before publishing a pair, and only the GL thread prunes programs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stage {
    pub(super) program: glow::Program,
}

pub(super) type StagePrograms = rustc_hash::FxHashMap<(usize, u32, u64), Arc<Stage>>;
pub(super) type Separate = rustc_hash::FxHashMap<(usize, glow::Program), (Arc<Stage>, Arc<Stage>)>;

/// the stage programs built, by the contexts sharing them, stage and
/// source, shared by every thread that builds programs.
pub(super) static STAGE_PROGRAMS: std::sync::Mutex<Option<StagePrograms>> = std::sync::Mutex::new(None);

/// the stand-in program a separable program goes by, everywhere a linked
/// one does, and the stage programs it stands for, by the contexts sharing
/// them too: two renderers' programs can have the same name.
pub(super) static SEPARATE: std::sync::Mutex<Option<Separate>> = std::sync::Mutex::new(None);

/// the renderer's name, which the program cache's file names start from.
pub(super) static RENDERER_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

thread_local! {
    /// the contexts this thread's shares objects with, the EGL they came
    /// from, which stage programs are kept by.
    pub(super) static SHARE_GROUP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static PAIR_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// the GL thread's pipelines, which unlike programs are its context's
    /// own, by the stand-in they are for.
    pub(super) static PIPELINES: std::cell::RefCell<rustc_hash::FxHashMap<glow::Program, u32>> = std::cell::RefCell::new(Default::default());
}

/// a stage of a separable program, built or loaded once and kept.
pub(super) fn stage_program(gl: &glow::Context, stage: u32, source: &str) -> Result<Arc<Stage>, String> {
    let key = (SHARE_GROUP.get(), stage, fnv(&[source]));
    if let Some(found) = STAGE_PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(Default::default).get(&key) {
        return Ok(found.clone());
    }
    let _compiling = compiler_lock();
    let _watched = Watched;
    #[cfg(feature = "shader-attribution")]
    let mut span = attribution::Span::begin(attribution::Kind::Stage, Some(gl), stage as u64);
    let started = std::time::Instant::now();
    let cached = RENDERER_NAME.get().and_then(|name| program_cache_path(name, if stage == glow::VERTEX_SHADER { "separable vertex" } else { "separable fragment" }, source));
    // SAFETY: the context is current on this thread, the pipeline calls
    // were loaded from it and take the program's name
    let program = unsafe {
        let program = gl.create_program()?;
        let parameter: extern "system" fn(u32, u32, i32) = std::mem::transmute(pipeline_fn(0));
        parameter(program.0.get(), PROGRAM_SEPARABLE, 1);
        let loaded = cached.as_deref().and_then(|path| observe!(CacheRead, gl, 0, std::fs::read(path).ok())).filter(|bytes| bytes.len() > 4).is_some_and(|bytes| {
            let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            observe!(BinaryLoad, gl, bytes.len() as u64 - 4, gl.program_binary(program, &glow::ProgramBinary { buffer: bytes[4..].to_vec(), format }));
            observe!(BinaryStatus, gl, 0, gl.get_program_link_status(program))
        });
        if !loaded {
            let (shader, kept) = match compiled_shader(stage, source) {
                Some(shader) => (shader, true),
                None => {
                    let shader = gl.create_shader(stage)?;
                    gl.shader_source(shader, source);
                    at_compiler([source, ""]);
                    observe!(ShaderCompile, gl, stage as u64, gl.compile_shader(shader));
                    (shader, false)
                }
            };
            if !kept && !observe!(CompileStatus, gl, stage as u64, gl.get_shader_compile_status(shader)) {
                let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                gl.delete_shader(shader);
                gl.delete_program(program);
                #[cfg(feature = "shader-attribution")]
                span.set(attribution::COMPILE_FAILED);
                return Err(error);
            }
            gl.attach_shader(program, shader);
            if cached.is_some() {
                gl.program_binary_retrievable_hint(program, true);
            }
            at_compiler([source, ""]);
            observe!(ProgramLink, gl, 0, gl.link_program(program));
            gl.detach_shader(program, shader);
            if !kept {
                // The linked stage is cached by source below. Every later
                // request reuses that program before looking for a shader,
                // so retaining its detached shader cannot save any work.
                // A borrowed shader stays with the cache that owns it.
                gl.delete_shader(shader);
            }
            if !observe!(LinkStatus, gl, 0, gl.get_program_link_status(program)) {
                let error = format!("could not link a shader stage, {}", gl.get_program_info_log(program));
                gl.delete_program(program);
                #[cfg(feature = "shader-attribution")]
                span.set(attribution::LINK_FAILED);
                return Err(error);
            }
            write_program_binary(gl, program, cached.as_deref());
        }
        #[cfg(feature = "shader-attribution")]
        span.set(if loaded { attribution::LOADED } else { attribution::COMPILED });
        if stage == glow::FRAGMENT_SHADER {
            for unit in 0..3 {
                if let Some(location) = gl.get_uniform_location(program, &format!("texture{unit}")) {
                    gl.program_uniform_1_i32(program, Some(&location), unit);
                }
            }
        }
        let took = started.elapsed();
        if !loaded && took >= std::time::Duration::from_millis(100) {
            log::info!("a slow {} stage: {:.0} ms, {} lines", if stage == glow::VERTEX_SHADER { "vertex" } else { "fragment" }, took.as_secs_f64() * 1000.0, source.lines().count());
        }
        program
    };
    // another thread may have built the same stage meanwhile, its stays
    let mut stages = STAGE_PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let stages = stages.get_or_insert_with(Default::default);
    if let Some(found) = stages.get(&key) {
        #[cfg(feature = "shader-attribution")]
        span.raced();
        // SAFETY: the context is current, nothing has used this one
        unsafe { gl.delete_program(program) };
        return Ok(found.clone());
    }
    let stage = Arc::new(Stage { program });
    stages.insert(key, stage.clone());
    Ok(stage)
}

/// a separable program: its stages, built once each, behind a stand-in
/// program that goes by the program's name everywhere else.
pub(super) fn build_separable(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
    // a separable program's stages meet as they are declared, which OpenGL
    // ES has match in precision too: fast mode takes colors and the
    // quaternion in at half, so the vertex stage hands them on at half,
    // which loses nothing the fragment stage kept. Left at full, the
    // pipeline does not validate and Mali leaves out every draw
    let vertex = if fragment.contains("precision mediump float;") {
        vertex.replace("out vec4 out_color;", "out mediump vec4 out_color;").replace("out vec4 out_quaternion;", "out mediump vec4 out_quaternion;")
    } else {
        vertex.to_owned()
    };
    let stages = (stage_program(gl, glow::VERTEX_SHADER, &vertex)?, stage_program(gl, glow::FRAGMENT_SHADER, fragment)?);
    // SAFETY: the context is current on this thread
    let token = unsafe { gl.create_program()? };
    SEPARATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(Default::default).insert((SHARE_GROUP.get(), token), stages);
    Ok(token)
}

/// makes a program current to draw with: a linked one as it is, a separable
/// one's stages through the GL thread's pipeline for them, made the first
/// time.
pub(super) fn bind_program(gl: &glow::Context, program: glow::Program) {
    let pipeline = if separable() {
        // Only this GL thread creates/deletes its pipelines. delete_program
        // removes the entry before releasing the pair's stage leases, so a
        // cached pipeline already has all the ownership needed for a bind.
        let cached = PIPELINES.with(|pipelines| pipelines.borrow().get(&program).copied());
        cached.or_else(|| {
            #[cfg(test)]
            PAIR_LOOKUPS.set(PAIR_LOOKUPS.get() + 1);
            let (vertex, fragment) = SEPARATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref().and_then(|separate| separate.get(&(SHARE_GROUP.get(), program)).cloned())?;
            // SAFETY: on the GL thread, using linked stages in its share group.
            let pipeline = unsafe {
                let generate: extern "system" fn(i32, *mut u32) = std::mem::transmute(pipeline_fn(1));
                let stages_of: extern "system" fn(u32, u32, u32) = std::mem::transmute(pipeline_fn(3));
                let mut pipeline = 0u32;
                generate(1, &mut pipeline);
                stages_of(pipeline, VERTEX_SHADER_BIT, vertex.program.0.get());
                stages_of(pipeline, FRAGMENT_SHADER_BIT, fragment.program.0.get());
                pipeline
            };
            PIPELINES.with(|pipelines| pipelines.borrow_mut().insert(program, pipeline));
            Some(pipeline)
        })
    } else { None };
    // SAFETY: on the GL thread, binding its pipeline or a shared program.
    unsafe {
        if let Some(pipeline) = pipeline {
            let bind: extern "system" fn(u32) = std::mem::transmute(pipeline_fn(4));
            gl.use_program(None);
            bind(pipeline);
        } else {
            gl.use_program(Some(program));
        }
    }
}

/// deletes a program a draw used, a separable one's stand-in and pipeline,
/// its stages kept for others.
pub(super) fn delete_program(gl: &glow::Context, program: glow::Program) {
    let separate = SEPARATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut().and_then(|separate| separate.remove(&(SHARE_GROUP.get(), program)));
    // SAFETY: on the GL thread, after every draw that used it
    unsafe {
        if separate.is_some() {
            if let Some(pipeline) = PIPELINES.with(|pipelines| pipelines.borrow_mut().remove(&program)) {
                let delete: extern "system" fn(i32, *const u32) = std::mem::transmute(pipeline_fn(2));
                delete(1, &pipeline);
            }
        }
        gl.delete_program(program);
    }
    // Pipelines no longer reference the stages before the pair releases its
    // leases. A compiler may still hold another lease while building a pair.
    drop(separate);
}

/// Prunes cache-only stages on their sharing GL thread. No GL work happens
/// when an Arc drops on a compiler thread. The cache mutex makes reservation
/// lookup and the last-owner decision atomic with respect to one another.
pub(super) fn prune_stages(gl: &glow::Context) {
    let sharing = SHARE_GROUP.get();
    let mut retired = Vec::new();
    {
        let mut stages = STAGE_PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(stages) = stages.as_mut() {
            stages.retain(|&(group, _, _), stage| {
                let keep = group != sharing || Arc::strong_count(stage) != 1;
                if !keep {
                    retired.push(stage.program);
                }
                keep
            });
        }
    }
    if !retired.is_empty() {
        // SAFETY: on this share group's GL thread; the removed cache entry
        // was the only owner. All compiler reservations and pipeline pairs
        // take their own Arc while holding the cache or pair mutex.
        unsafe {
            let bound = gl.get_parameter_i32(glow::CURRENT_PROGRAM) as u32;
            if retired.iter().any(|program| program.0.get() == bound) {
                gl.use_program(None);
            }
            for program in retired {
                gl.delete_program(program);
            }
        }
    }
}

/// what building programs costs, in the log every 10 the driver compiled:
/// the wait for the compiler lock, compiling each stage and linking, and how
/// often a stage's source was one compiled before, which a cache of compiled
/// shaders would save. Tells where a faster compile has to come from.
pub(super) fn count_build(vertex: &str, fragment: &str, waited: std::time::Duration, stages: [std::time::Duration; 3]) {
    struct Counts {
        built: u32,
        waited: std::time::Duration,
        stages: [std::time::Duration; 3],
        slowest: std::time::Duration,
        seen: [rustc_hash::FxHashSet<u64>; 2],
        repeated: [u32; 2],
    }
    static COUNTS: std::sync::Mutex<Option<Counts>> = std::sync::Mutex::new(None);
    let mut counts = COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let counts = counts.get_or_insert_with(|| Counts {
        built: 0,
        waited: std::time::Duration::ZERO,
        stages: [std::time::Duration::ZERO; 3],
        slowest: std::time::Duration::ZERO,
        seen: Default::default(),
        repeated: [0; 2],
    });
    counts.built += 1;
    counts.waited += waited;
    for (total, stage) in counts.stages.iter_mut().zip(stages) {
        *total += stage;
    }
    counts.slowest = counts.slowest.max(stages[1] + stages[2]);
    // a program the driver takes long over is what a player sees stall: the
    // first few each run say what it is made of
    static SLOW_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if stages[1] + stages[2] >= std::time::Duration::from_millis(100) && SLOW_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
        let form = if vertex.contains("switch (pc)") { "a dispatch loop" } else if vertex.contains("void run()") { "translated" } else { "the generic one" };
        log::info!(
            "a slow program: {:.0} ms compiling, {:.0} ms linking; its vertex shader {} lines, {form}, its fragment shader {} lines{}",
            stages[1].as_secs_f64() * 1000.0,
            stages[2].as_secs_f64() * 1000.0,
            vertex.lines().count(),
            fragment.lines().count(),
            if fragment.contains("const uint DYNAMIC = 1u;") { ", a generic stand-in" } else { "" }
        );
    }
    for (index, source) in [vertex, fragment].into_iter().enumerate() {
        if !counts.seen[index].insert(fnv(&[source])) {
            counts.repeated[index] += 1;
        }
    }
    if counts.built.is_multiple_of(10) {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / counts.built as f64;
        log::info!(
            "programs compiled: {}, on average {:.1} ms before compiling (the lock and looking for a saved copy), {:.1} ms until the vertex shader compiled, {:.1} ms until both had, {:.1} ms linking, slowest {:.0} ms; vertex shaders compiled before {} of {}, fragment shaders {}",
            counts.built,
            ms(counts.waited),
            ms(counts.stages[0]),
            ms(counts.stages[1]),
            ms(counts.stages[2]),
            counts.slowest.as_secs_f64() * 1000.0,
            counts.repeated[0],
            counts.built,
            counts.repeated[1]
        );
    }
}

/// stages compiled a shader each on this thread, by stage and source, to
/// link into the next program that has the same one: programs share their
/// vertex stages far more than not, and each was compiled anew. The objects
/// belong to the context, shared with the GL thread's.
pub(super) const KEPT_SHADERS: usize = 512;

thread_local! {
    pub(super) static SHADERS: std::cell::RefCell<rustc_hash::FxHashMap<(u32, u64), glow::Shader>> = std::cell::RefCell::new(Default::default());
}

pub(super) fn compiled_shader(stage: u32, source: &str) -> Option<glow::Shader> {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| crate::switch("ZAKURO_NO_SHADER_REUSE", "every program compiles both its stages")) {
        return None;
    }
    SHADERS.with(|shaders| shaders.borrow().get(&(stage, fnv(&[source]))).copied())
}

/// keeps a stage that compiled for the next program with the same source,
/// or deletes it, when it failed or reuse is off. Past KEPT_SHADERS kept,
/// they all go and the count starts again.
pub(super) fn keep_shader(gl: &glow::Context, stage: u32, source: &str, shader: glow::Shader) {
    // SAFETY: the context is current on this thread, a shader only goes
    // once detached from the program just linked
    unsafe {
        if !gl.get_shader_compile_status(shader) || std::env::var_os("ZAKURO_NO_SHADER_REUSE").is_some() {
            gl.delete_shader(shader);
            return;
        }
        SHADERS.with(|shaders| {
            let mut shaders = shaders.borrow_mut();
            if shaders.len() >= KEPT_SHADERS {
                for (_, old) in shaders.drain() {
                    gl.delete_shader(old);
                }
            }
            if let Some(old) = shaders.insert((stage, fnv(&[source])), shader) {
                gl.delete_shader(old);
            }
            if super::memory::enabled() {
                log::info!("gpu memory compiler snapshot: thread {:?}, retained shader objects {}; source bytes of newest stage {}; driver shader allocation bytes unknown", thread::current().name(), shaders.len(), source.len());
            }
        });
    }
}

pub(super) type ProgramKey = ([u32; FRAGMENT_CONSTANTS], bool, Vec<u32>, u64);

/// compiles and links a program, or loads it from the program binary cache
/// when it was made before, its samplers on the texture units.
/// microseconds spent in build_program, for prewarm's breakdown.
pub(super) static BUILD_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn build_program(gl: &glow::Context, vertex: &str, fragment: &str, cached: Option<&std::path::Path>) -> Result<glow::Program, String> {
    let started = std::time::Instant::now();
    struct Timed(std::time::Instant);
    impl Drop for Timed {
        fn drop(&mut self) {
            BUILD_MICROS.fetch_add(self.0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _timed = Timed(started);
    #[cfg(feature = "shader-attribution")]
    let mut span = attribution::Span::begin(attribution::Kind::Build, Some(gl), cached.is_some() as u64);
    if separable() {
        #[cfg(feature = "shader-attribution")]
        span.set(attribution::SEPARABLE);
        return build_separable(gl, vertex, fragment);
    }
    let _compiling = compiler_lock();
    let _watched = Watched;
    // SAFETY: the context is current on this thread, the shaders are deleted
    // once linked into the program
    unsafe {
        let program = gl.create_program()?;
        let loaded = cached.and_then(|path| observe!(CacheRead, gl, 0, std::fs::read(path).ok())).filter(|bytes| bytes.len() > 4).is_some_and(|bytes| {
            let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            observe!(BinaryLoad, gl, bytes.len() as u64 - 4, gl.program_binary(program, &glow::ProgramBinary { buffer: bytes[4..].to_vec(), format }));
            observe!(BinaryStatus, gl, 0, gl.get_program_link_status(program))
        });
        let waited = started.elapsed();
        if !loaded {
            let mut stages = [std::time::Duration::ZERO; 3];
            let sources = [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)];
            // both stages handed to the driver before waiting on either,
            // so that one compiling in parallel works on them together, and
            // a stage compiled before taken as it is
            let began = std::time::Instant::now();
            let mut shaders = Vec::with_capacity(2);
            for (stage, source) in sources {
                shaders.push(match compiled_shader(stage, source) {
                    Some(shader) => (shader, true),
                    None => {
                        let shader = gl.create_shader(stage)?;
                        gl.shader_source(shader, source);
                        at_compiler([source, ""]);
                        observe!(ShaderCompile, gl, stage as u64, gl.compile_shader(shader));
                        (shader, false)
                    }
                });
            }
            for (index, &(shader, kept)) in shaders.iter().enumerate() {
                at_compiler([sources[index].1, ""]);
                if !kept && !observe!(CompileStatus, gl, sources[index].0 as u64, gl.get_shader_compile_status(shader)) {
                    let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                    for &(shader, kept) in &shaders {
                        if !kept {
                            gl.delete_shader(shader);
                        }
                    }
                    gl.delete_program(program);
                    #[cfg(feature = "shader-attribution")]
                    span.set(attribution::COMPILE_FAILED);
                    return Err(error);
                }
                stages[index] = began.elapsed();
            }
            for &(shader, _) in &shaders {
                gl.attach_shader(program, shader);
            }
            if cached.is_some() {
                gl.program_binary_retrievable_hint(program, true);
            }
            at_compiler([vertex, fragment]);
            let began = std::time::Instant::now();
            observe!(ProgramLink, gl, 0, gl.link_program(program));
            for (&(shader, kept), (stage, source)) in shaders.iter().zip(sources) {
                gl.detach_shader(program, shader);
                if !kept {
                    keep_shader(gl, stage, source, shader);
                }
            }
            let linked = observe!(LinkStatus, gl, 0, gl.get_program_link_status(program));
            stages[2] = began.elapsed();
            count_build(vertex, fragment, waited, stages);
            if !linked {
                let error = format!("could not link a program, {}", gl.get_program_info_log(program));
                gl.delete_program(program);
                #[cfg(feature = "shader-attribution")]
                span.set(attribution::LINK_FAILED);
                return Err(error);
            }
            write_program_binary(gl, program, cached);
        }
        #[cfg(feature = "shader-attribution")]
        span.set(if loaded { attribution::LOADED } else { attribution::COMPILED });
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
pub(super) struct Compile {
    pub(super) key: ProgramKey,
    pub(super) vertex: String,
    pub(super) fragment: String,
    pub(super) cached: Option<std::path::PathBuf>,
}

/// the programs waiting for a compiler thread: those a draw needs now first,
/// the generic stand-ins after them, which the driver takes seconds over
/// each and which draws only use while their own programs build, unless
/// ZAKURO_STAND_INS has draws use them, when they come first. In arrival
/// order otherwise.
pub(super) struct Queue {
    pub(super) jobs: std::sync::Mutex<(std::collections::VecDeque<Compile>, std::collections::VecDeque<Compile>, bool)>,
    pub(super) ready: std::sync::Condvar,
    /// the diagnostic feature's open queue waits, kept under `jobs`' lock.
    #[cfg(feature = "shader-attribution")]
    pub(super) waits: attribution::QueueWaits,
}

impl Queue {
    pub(super) fn new() -> Queue {
        Queue {
            jobs: std::sync::Mutex::new((Default::default(), Default::default(), false)),
            ready: std::sync::Condvar::new(),
            #[cfg(feature = "shader-attribution")]
            waits: attribution::QueueWaits::new(),
        }
    }

    pub(super) fn send(&self, job: Compile) -> Result<(), ()> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if jobs.2 {
            return Err(());
        }
        if job.key.0[29] != 0 {
            BUILDING_STAND_INS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // a stand-in's constants say it is the dynamic program. Draws drawn
        // with stand-ins while their programs build need them first
        if job.key.0[29] != 0 && !crate::raster::stand_ins() {
            #[cfg(feature = "shader-attribution")]
            self.waits.queued(1, true);
            jobs.1.push_back(job);
        } else {
            #[cfg(feature = "shader-attribution")]
            self.waits.queued(0, job.key.0[29] != 0);
            jobs.0.push_back(job);
        }
        self.ready.notify_one();
        Ok(())
    }

    /// the next job, waiting for one, or none once closed.
    pub(super) fn take(&self) -> Option<Compile> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            #[cfg(not(feature = "shader-attribution"))]
            {
                if let Some(job) = jobs.0.pop_front().or_else(|| jobs.1.pop_front()) {
                    return Some(job);
                }
            }
            #[cfg(feature = "shader-attribution")]
            {
                let (first, second, _) = &mut *jobs;
                if let Some(job) = self.waits.take(first, second) {
                    return Some(job);
                }
            }
            if jobs.2 {
                return None;
            }
            jobs = self.ready.wait(jobs).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(super) fn close(&self) {
        self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).2 = true;
        self.ready.notify_all();
    }
}

/// the compiler threads' queue, closed when the compiler goes, which ends
/// them.
pub(super) struct Jobs(std::sync::Arc<Queue>);

impl Jobs {
    pub(super) fn send(&self, job: Compile) -> Result<(), ()> {
        self.0.send(job)
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// a thread compiling new programs in a context that shares the GL thread's
/// objects, so that a draw goes on with a stand-in rather than wait for the
/// driver's compiler.
pub(super) struct Compiler {
    pub(super) jobs: Jobs,
    pub(super) thread: thread::JoinHandle<()>,
    pub(super) done: std::sync::mpsc::Receiver<(ProgramKey, Result<glow::Program, String>)>,
    pub(super) pending: rustc_hash::FxHashSet<ProgramKey>,
    pub(super) failed: rustc_hash::FxHashMap<ProgramKey, String>,
}

impl Compiler {
    pub(super) fn start(egl: &'static Egl) -> Option<Compiler> {
        let synchronous = crate::switch("ZAKURO_SYNC_SHADERS", "shaders compiled when a draw needs them");
        if synchronous {
            return None;
        }
        let received = std::sync::Arc::new(Queue::new());
        let jobs = Jobs(received.clone());
        let (finished, done) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for index in 0..compilers() {
            let (received, finished, ready) = (received.clone(), finished.clone(), ready.clone());
            let spawned = thread::Builder::new()
                .name(if index == 0 { "gles-compiler".into() } else { format!("gles-compiler{index}") })
                .spawn(move || {
                    #[cfg(feature = "shader-attribution")]
                    let _live = attribution::Worker::enter();
                    let Some(gl) = observe!(WorkerContext, none, index as u64, egl.shared_context()) else {
                        let _ = ready.send(false);
                        return;
                    };
                    SHARE_GROUP.set(egl as *const Egl as usize);
                    // SAFETY: lowers the priority of this thread alone, which is
                    // what Linux takes 0 for, the game's threads going first
                    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
                    // glow answers this from the list it read when the context
                    // was made, no GL call
                    #[cfg(feature = "shader-attribution")]
                    attribution::instant(attribution::Kind::ExtensionQuery, Some(&gl), index as u64, gl.supported_extensions().contains("GL_KHR_parallel_shader_compile") as u32);
                    // a driver that can compile in threads of its own may use as
                    // many as it likes for this context's programs
                    if gl.supported_extensions().contains("GL_KHR_parallel_shader_compile") {
                        // SAFETY: the context is current, the extension names it
                        unsafe { observe!(ParallelHint, &gl, u32::MAX as u64, gl.max_shader_compiler_threads(u32::MAX)) };
                        log::info!("the driver compiles this thread's shaders in threads of its own");
                    }
                    let _ = ready.send(true);
                    // one job at a time off the shared queue, the lock let go
                    // before the build
                    while let Some(job) = received.take() {
                        #[cfg(feature = "shader-attribution")]
                        let span = attribution::Span::begin(attribution::Kind::Job, Some(&gl), attribution::taken());
                        let result = build_program(&gl, &job.vertex, &job.fragment, job.cached.as_deref());
                        // SAFETY: the context is current on this thread
                        unsafe { observe!(WorkerFinish, &gl, 0, gl.finish()) };
                        #[cfg(feature = "shader-attribution")]
                        span.end(attribution::Observed::outcome(&result));
                        BUILDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        if job.key.0[29] != 0 {
                            BUILDING_STAND_INS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if finished.send((job.key, result)).is_err() {
                            break;
                        }
                    }
                });
            if let Ok(spawned) = spawned {
                threads.push(spawned);
            }
        }
        drop(ready);
        // every thread that started says whether it has a context, the first
        // that has one is enough to go on
        let mut any = false;
        for _ in 0..threads.len() {
            any |= started.recv().unwrap_or(false);
        }
        let thread = (!threads.is_empty()).then(|| threads.remove(0))?;
        any.then(|| Compiler { jobs, thread, done, pending: Default::default(), failed: Default::default() })
    }
}

/// Diagnostic only, behind the default-off `shader-attribution` feature:
/// what each program build, binary load, compile, link, cache read/write and
/// compiler job cost, as begin/end events on CLOCK_MONOTONIC, enough to
/// tell a warm binary load from a first run's compile and link.
///
/// Bounded: the first [`CAPACITY`] events of the process take a slot of
/// fixed scalars, every later one only counts as dropped. Nothing records
/// shader source, guest data or PCM. Recording takes no lock and makes no
/// heap allocation; it never runs on a draw (binding, deleting and pruning
/// programs are not observed) nor in the crash handler. The clock and the
/// atomics can still move the driver's scheduling, so a capture says where
/// time went, never how fast anything is.
///
/// A snapshot is never final: compiler workers have no join barrier here
/// and may outlive whoever asks, and the GL thread can build later. Open
/// and reserved events are reported as partial, and the completion field
/// is `partial` or `unknown`, never `final`.
#[cfg(feature = "shader-attribution")]
pub mod attribution {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

    /// events kept per process.
    pub const CAPACITY: usize = 256;
    pub const FORMAT: &str = "zakuro-shader-attribution 1";
    /// the most a report renders to: CAPACITY lines of fixed scalars at
    /// their widest, and the header.
    pub const MAX_REPORT_BYTES: usize = 64 * 1024;

    /// an event's outcome when its span ended without one, by an early
    /// return or an unwind.
    pub const NO_OUTCOME: u32 = u32::MAX;
    /// outcomes of build, stage and graphics events.
    pub const LOADED: u32 = 1;
    pub const COMPILED: u32 = 2;
    pub const COMPILE_FAILED: u32 = 3;
    pub const LINK_FAILED: u32 = 4;
    /// build only: handed to the separable stages, their own stage events.
    pub const SEPARABLE: u32 = 5;
    /// stage only, added to LOADED or COMPILED: another thread published
    /// the same stage first and this one was deleted.
    pub const RACED: u32 = 10;

    /// what an event spans. Its aux and outcome mean, by kind:
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u32)]
    pub enum Kind {
        /// build_program; aux 1 with a cache path; outcome LOADED..SEPARABLE
        Build = 1,
        /// stage_program past the in-process stage cache; aux the GL stage
        Stage,
        /// link(), the renderer's own programs; outcome as Build
        Graphics,
        /// reading the program binary file; outcome 1 read, 0 not
        CacheRead,
        /// gl.program_binary, the copy of its bytes included; aux bytes
        BinaryLoad,
        /// gl.get_program_link_status after a binary; outcome linked
        BinaryStatus,
        /// gl.compile_shader; aux the GL stage
        ShaderCompile,
        /// gl.get_shader_compile_status, where a parallel compile is waited
        /// for; aux the GL stage; outcome compiled
        CompileStatus,
        /// gl.link_program
        ProgramLink,
        /// gl.get_program_link_status after a link; outcome linked
        LinkStatus,
        /// gl.get_program_binary; outcome 1 with a binary
        BinaryRetrieve,
        /// the binary file's directory and write; aux bytes; outcome 1 ok
        CacheWrite,
        /// a compiler job from Queue::send to Queue::take; aux bit 0 a
        /// stand-in, bit 1 the stand-ins' later lane; begin and end threads
        /// differ
        QueueWait,
        /// a compiler worker's build_program and gl.finish; aux its
        /// queue_wait's index + 1, 0 unknown; outcome 1 built
        Job,
        /// the gl.finish after a compiler job
        WorkerFinish,
        /// a compiler worker making its shared context; aux its index;
        /// outcome 1 with a context
        WorkerContext,
        /// whether the worker's context lists GL_KHR_parallel_shader_compile,
        /// an instant; aux the worker's index; outcome 1 listed
        ExtensionQuery,
        /// gl.max_shader_compiler_threads; aux the count asked for
        ParallelHint,
    }

    const KINDS: [Kind; 18] = [
        Kind::Build, Kind::Stage, Kind::Graphics, Kind::CacheRead, Kind::BinaryLoad, Kind::BinaryStatus,
        Kind::ShaderCompile, Kind::CompileStatus, Kind::ProgramLink, Kind::LinkStatus, Kind::BinaryRetrieve,
        Kind::CacheWrite, Kind::QueueWait, Kind::Job, Kind::WorkerFinish, Kind::WorkerContext,
        Kind::ExtensionQuery, Kind::ParallelHint,
    ];

    impl Kind {
        pub fn name(self) -> &'static str {
            match self {
                Kind::Build => "build",
                Kind::Stage => "stage",
                Kind::Graphics => "graphics",
                Kind::CacheRead => "cache_read",
                Kind::BinaryLoad => "binary_load",
                Kind::BinaryStatus => "binary_status",
                Kind::ShaderCompile => "shader_compile",
                Kind::CompileStatus => "compile_status",
                Kind::ProgramLink => "program_link",
                Kind::LinkStatus => "link_status",
                Kind::BinaryRetrieve => "binary_retrieve",
                Kind::CacheWrite => "cache_write",
                Kind::QueueWait => "queue_wait",
                Kind::Job => "job",
                Kind::WorkerFinish => "worker_finish",
                Kind::WorkerContext => "worker_context",
                Kind::ExtensionQuery => "extension_query",
                Kind::ParallelHint => "parallel_hint",
            }
        }

        pub fn from_u32(value: u32) -> Option<Kind> {
            KINDS.get((value as usize).checked_sub(1)?).copied()
        }
    }

    /// what a span's value says of how it went.
    pub trait Observed {
        fn outcome(&self) -> u32;
    }
    impl Observed for () {
        fn outcome(&self) -> u32 {
            1
        }
    }
    impl Observed for bool {
        fn outcome(&self) -> u32 {
            *self as u32
        }
    }
    impl<T> Observed for Option<T> {
        fn outcome(&self) -> u32 {
            self.is_some() as u32
        }
    }
    impl<T, E> Observed for Result<T, E> {
        fn outcome(&self) -> u32 {
            self.is_ok() as u32
        }
    }

    const EMPTY: u32 = 0;
    const OPEN: u32 = 1;
    const DONE: u32 = 2;

    /// one event's scalars. The opening thread writes the begin fields
    /// before publishing OPEN, the closing one the end fields before DONE.
    struct Record {
        state: AtomicU32,
        kind: AtomicU32,
        outcome: AtomicU32,
        begin_thread: AtomicU32,
        end_thread: AtomicU32,
        context: AtomicU32,
        share_group: AtomicU32,
        begin_ns: AtomicU64,
        end_ns: AtomicU64,
        aux: AtomicU64,
    }

    impl Record {
        const fn new() -> Record {
            Record {
                state: AtomicU32::new(EMPTY),
                kind: AtomicU32::new(0),
                outcome: AtomicU32::new(NO_OUTCOME),
                begin_thread: AtomicU32::new(0),
                end_thread: AtomicU32::new(0),
                context: AtomicU32::new(0),
                share_group: AtomicU32::new(0),
                begin_ns: AtomicU64::new(0),
                end_ns: AtomicU64::new(0),
                aux: AtomicU64::new(0),
            }
        }
    }

    /// N fixed slots, taken in order, never reused.
    pub struct Recorder<const N: usize> {
        attempted: AtomicU64,
        records: [Record; N],
    }

    impl<const N: usize> Default for Recorder<N> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<const N: usize> Recorder<N> {
        pub const fn new() -> Recorder<N> {
            Recorder { attempted: AtomicU64::new(0), records: [const { Record::new() }; N] }
        }

        /// counts an attempt, and opens a slot for it while there is one.
        pub fn open(&self, kind: Kind, context: u32, share_group: u32, aux: u64, thread: u32, now: u64) -> Option<u32> {
            self.open_lazy(kind, || (context, share_group, aux, thread, now))
        }

        /// Only admitted records gather identity/clock fields. Saturation
        /// counts drops without more clock syscalls or context-table work.
        fn open_lazy(&self, kind: Kind, fields: impl FnOnce() -> (u32, u32, u64, u32, u64)) -> Option<u32> {
            let index = self.attempted.fetch_add(1, Relaxed);
            let record = self.records.get(usize::try_from(index).ok()?)?;
            let (context, share_group, aux, thread, now) = fields();
            record.kind.store(kind as u32, Relaxed);
            record.context.store(context, Relaxed);
            record.share_group.store(share_group, Relaxed);
            record.aux.store(aux, Relaxed);
            record.begin_thread.store(thread, Relaxed);
            record.begin_ns.store(now, Relaxed);
            record.state.store(OPEN, Release);
            Some(index as u32)
        }

        /// ends an open slot, once; a second close is ignored.
        pub fn close(&self, slot: u32, outcome: u32, thread: u32, now: u64) {
            let Some(record) = self.records.get(slot as usize) else { return };
            if record.state.load(Acquire) != OPEN {
                return;
            }
            record.end_ns.store(now, Relaxed);
            record.end_thread.store(thread, Relaxed);
            record.outcome.store(outcome, Relaxed);
            record.state.store(DONE, Release);
        }

        /// what has been recorded so far, with workers_live compiler workers
        /// still running.
        pub fn snapshot(&self, workers_live: u64) -> Snapshot {
            let mut events = Vec::with_capacity(N);
            let (mut completed, mut open) = (0, 0);
            for (index, record) in self.records.iter().enumerate() {
                let state = record.state.load(Acquire);
                if state == EMPTY {
                    continue;
                }
                let done = state == DONE;
                if done { completed += 1 } else { open += 1 }
                events.push(Event {
                    index: index as u32,
                    kind: record.kind.load(Relaxed),
                    done,
                    outcome: if done { record.outcome.load(Relaxed) } else { NO_OUTCOME },
                    begin_thread: record.begin_thread.load(Relaxed),
                    end_thread: if done { record.end_thread.load(Relaxed) } else { 0 },
                    context: record.context.load(Relaxed),
                    share_group: record.share_group.load(Relaxed),
                    begin_ns: record.begin_ns.load(Relaxed),
                    end_ns: if done { Some(record.end_ns.load(Relaxed)) } else { None },
                    aux: record.aux.load(Relaxed),
                });
            }
            // read after the slots, so every slot seen is counted in it
            let attempted = self.attempted.load(Relaxed);
            let recorded = attempted.min(N as u64);
            Snapshot {
                capacity: N,
                attempted,
                recorded,
                completed,
                open,
                // slots taken whose opening thread had not published them yet
                reserved: recorded.saturating_sub(completed + open),
                dropped: attempted - recorded,
                workers_live,
                events,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Event {
        pub index: u32,
        pub kind: u32,
        pub done: bool,
        pub outcome: u32,
        pub begin_thread: u32,
        pub end_thread: u32,
        pub context: u32,
        pub share_group: u32,
        pub begin_ns: u64,
        pub end_ns: Option<u64>,
        pub aux: u64,
    }

    /// attempted = completed + open + reserved + dropped.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Snapshot {
        pub capacity: usize,
        pub attempted: u64,
        pub recorded: u64,
        pub completed: u64,
        pub open: u64,
        pub reserved: u64,
        pub dropped: u64,
        pub workers_live: u64,
        pub events: Vec<Event>,
    }

    impl Snapshot {
        pub fn partial(&self) -> u64 {
            self.open + self.reserved
        }

        /// `partial` with an event unfinished or a worker running, else
        /// `unknown`: nothing here proves no build comes after.
        pub fn completion(&self) -> &'static str {
            if self.partial() > 0 || self.workers_live > 0 { "partial" } else { "unknown" }
        }

        /// the sidecar's text, at most MAX_REPORT_BYTES.
        pub fn render(&self) -> String {
            use std::fmt::Write;
            let mut out = String::with_capacity(512 + self.events.len() * 160);
            let _ = writeln!(out, "{FORMAT}");
            let _ = writeln!(out, "diagnostic-only: instrumentation may perturb driver scheduling; not a timing or speed result");
            let _ = writeln!(out, "clock CLOCK_MONOTONIC ns");
            let _ = writeln!(
                out,
                "capacity {} attempted {} recorded {} completed {} partial {} open {} reserved {} dropped {} workers_live {} completion {}",
                self.capacity, self.attempted, self.recorded, self.completed, self.partial(), self.open, self.reserved, self.dropped, self.workers_live, self.completion()
            );
            let _ = writeln!(out, "columns index kind state outcome begin_thread end_thread context share_group begin_ns end_ns aux");
            for event in &self.events {
                let kind = Kind::from_u32(event.kind).map_or("unknown_kind", Kind::name);
                let state = if event.done { "done" } else { "open" };
                let dash = || "-".to_owned();
                let _ = writeln!(
                    out,
                    "event {} {kind} {state} {} {} {} {} {} {} {} {}",
                    event.index,
                    if event.done && event.outcome != NO_OUTCOME { event.outcome.to_string() } else { dash() },
                    event.begin_thread,
                    if event.done { event.end_thread.to_string() } else { dash() },
                    event.context,
                    event.share_group,
                    event.begin_ns,
                    event.end_ns.map_or_else(dash, |end| end.to_string()),
                    event.aux,
                );
            }
            out.truncate(MAX_REPORT_BYTES);
            out
        }
    }

    /// the process's events.
    pub static RECORDER: Recorder<CAPACITY> = Recorder::new();
    static WORKERS_LIVE: AtomicU64 = AtomicU64::new(0);
    /// contexts and share groups by address, numbered from 1 as first seen;
    /// u32::MAX past the table. An address freed and reused keeps its number.
    static CONTEXTS: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];
    static GROUPS: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];

    thread_local! {
        static THREAD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static TAKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    pub fn now_ns() -> u64 {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid timespec to fill; CLOCK_MONOTONIC always exists
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
        time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
    }

    /// the kernel's id of this thread, as the profiler and the log see it.
    pub fn thread() -> u32 {
        THREAD.with(|thread| {
            if thread.get() == 0 {
                // SAFETY: gettid has no arguments and cannot fail
                thread.set(unsafe { libc::syscall(libc::SYS_gettid) } as u32);
            }
            thread.get()
        })
    }

    pub fn intern(table: &[AtomicUsize], address: usize) -> u32 {
        if address == 0 {
            return 0;
        }
        for (index, entry) in table.iter().enumerate() {
            let mut current = entry.load(Relaxed);
            if current == 0 {
                current = entry.compare_exchange(0, address, Relaxed, Relaxed).unwrap_or_else(|other| other);
                if current == 0 {
                    return index as u32 + 1;
                }
            }
            if current == address {
                return index as u32 + 1;
            }
        }
        u32::MAX
    }

    fn ids(gl: Option<&glow::Context>) -> (u32, u32) {
        let context = gl.map_or(0, |gl| intern(&CONTEXTS, gl as *const glow::Context as usize));
        (context, intern(&GROUPS, super::SHARE_GROUP.get()))
    }

    /// an event of the process's recorder, ended when dropped, with
    /// NO_OUTCOME unless one was set.
    pub struct Span {
        slot: Option<u32>,
        outcome: u32,
    }

    impl Span {
        pub fn begin(kind: Kind, gl: Option<&glow::Context>, aux: u64) -> Span {
            let slot = RECORDER.open_lazy(kind, || {
                let (context, share_group) = ids(gl);
                (context, share_group, aux, thread(), now_ns())
            });
            Span { slot, outcome: NO_OUTCOME }
        }

        pub fn set(&mut self, outcome: u32) {
            self.outcome = outcome;
        }

        pub fn raced(&mut self) {
            if self.outcome != NO_OUTCOME {
                self.outcome += RACED;
            }
        }

        pub fn end(mut self, outcome: u32) {
            self.outcome = outcome;
        }

        pub fn slot(&self) -> Option<u32> {
            self.slot
        }
    }

    impl Drop for Span {
        fn drop(&mut self) {
            if let Some(slot) = self.slot.take() {
                RECORDER.close(slot, self.outcome, thread(), now_ns());
            }
        }
    }

    /// an event with no duration.
    pub fn instant(kind: Kind, gl: Option<&glow::Context>, aux: u64, outcome: u32) {
        Span::begin(kind, gl, aux).end(outcome);
    }

    /// a compiler worker running, from before its context to its end.
    pub struct Worker(());

    impl Worker {
        pub fn enter() -> Worker {
            WORKERS_LIVE.fetch_add(1, Relaxed);
            Worker(())
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            WORKERS_LIVE.fetch_sub(1, Relaxed);
        }
    }

    /// the queue_wait index + 1 of the job this thread took last, 0 for
    /// none or unknown, once.
    pub fn taken() -> u64 {
        TAKEN.replace(0)
    }

    const RING: usize = CAPACITY;

    /// each queued job's open queue_wait slot + 1 (0 none), by lane, in
    /// the lanes' FIFO order. Only touched under the queue's lock; atomics
    /// only to be Sync. A job queued more than RING deep overwrites an
    /// older one's slot, whose event then stays open: partial, not wrong.
    pub struct QueueWaits {
        slots: [[AtomicU32; RING]; 2],
        pushed: [AtomicU64; 2],
        popped: [AtomicU64; 2],
    }

    impl Default for QueueWaits {
        fn default() -> Self {
            Self::new()
        }
    }

    impl QueueWaits {
        pub const fn new() -> QueueWaits {
            QueueWaits {
                slots: [const { [const { AtomicU32::new(0) }; RING] }; 2],
                pushed: [const { AtomicU64::new(0) }; 2],
                popped: [const { AtomicU64::new(0) }; 2],
            }
        }

        pub fn queued(&self, lane: usize, stand_in: bool) {
            let aux = stand_in as u64 | (lane as u64) << 1;
            let slot = RECORDER.open_lazy(Kind::QueueWait, || (0, 0, aux, thread(), now_ns())).map_or(0, |slot| slot + 1);
            let at = self.pushed[lane].fetch_add(1, Relaxed);
            self.slots[lane][at as usize % RING].store(slot, Relaxed);
        }

        pub fn queued_in<const N: usize>(&self, recorder: &Recorder<N>, lane: usize, stand_in: bool, thread: u32, now: u64) {
            let aux = stand_in as u64 | (lane as u64) << 1;
            let slot = recorder.open(Kind::QueueWait, 0, 0, aux, thread, now).map_or(0, |slot| slot + 1);
            let at = self.pushed[lane].fetch_add(1, Relaxed);
            self.slots[lane][at as usize % RING].store(slot, Relaxed);
        }

        /// Queue::take's own pop: the first lane, then the second.
        pub fn take<T>(&self, first: &mut std::collections::VecDeque<T>, second: &mut std::collections::VecDeque<T>) -> Option<T> {
            let (lane, job) = match first.pop_front() {
                Some(job) => (0, job),
                None => (1, second.pop_front()?),
            };
            TAKEN.set(self.taken_lazy(&RECORDER, lane, || (thread(), now_ns())));
            Some(job)
        }

        /// closes the queue_wait of the job just popped from lane, giving
        /// its slot + 1, 0 when it had none or was overwritten.
        pub fn taken_in<const N: usize>(&self, recorder: &Recorder<N>, lane: usize, thread: u32, now: u64) -> u64 {
            self.taken_lazy(recorder, lane, || (thread, now))
        }

        fn taken_lazy<const N: usize>(&self, recorder: &Recorder<N>, lane: usize, fields: impl FnOnce() -> (u32, u64)) -> u64 {
            let at = self.popped[lane].fetch_add(1, Relaxed);
            let depth = self.pushed[lane].load(Relaxed).checked_sub(at);
            if !depth.is_some_and(|depth| (1..=RING as u64).contains(&depth)) {
                return 0;
            }
            let slot = self.slots[lane][at as usize % RING].load(Relaxed);
            if slot != 0 {
                let (thread, now) = fields();
                recorder.close(slot - 1, 1, thread, now);
            }
            slot as u64
        }
    }

    /// the process's events now.
    pub fn snapshot() -> Snapshot {
        RECORDER.snapshot(WORKERS_LIVE.load(Relaxed))
    }

    /// writes snapshot()'s report to a new file at path, refusing one that
    /// exists, and gives the snapshot.
    pub fn write_sidecar(path: &std::path::Path) -> std::io::Result<Snapshot> {
        use std::io::Write;
        let snapshot = snapshot();
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(snapshot.render().as_bytes())?;
        Ok(snapshot)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn an_event_keeps_its_exact_boundaries_and_threads() {
            let recorder = Recorder::<4>::new();
            let slot = recorder.open(Kind::BinaryLoad, 2, 1, 4096, 77, 1_000).unwrap();
            assert_eq!(recorder.snapshot(0).events[0].end_ns, None, "open until closed");
            recorder.close(slot, 1, 78, 1_250);
            recorder.close(slot, 0, 79, 9_999);
            let snapshot = recorder.snapshot(0);
            assert_eq!(snapshot.events, [Event { index: 0, kind: Kind::BinaryLoad as u32, done: true, outcome: 1, begin_thread: 77, end_thread: 78, context: 2, share_group: 1, begin_ns: 1_000, end_ns: Some(1_250), aux: 4096 }], "a second close changes nothing");
            assert_eq!((snapshot.attempted, snapshot.completed, snapshot.partial(), snapshot.dropped), (1, 1, 0, 0));
        }

        #[test]
        fn past_capacity_events_are_dropped_and_counted() {
            let recorder = Recorder::<4>::new();
            let slots: Vec<_> = (0..6).map(|i| recorder.open(Kind::ShaderCompile, 0, 0, i, 1, i * 10)).collect();
            assert_eq!(slots, [Some(0), Some(1), Some(2), Some(3), None, None]);
            for slot in slots.iter().flatten() {
                recorder.close(*slot, 1, 1, 100);
            }
            let snapshot = recorder.snapshot(0);
            assert_eq!((snapshot.attempted, snapshot.recorded, snapshot.completed, snapshot.dropped), (6, 4, 4, 2));
            assert_eq!(snapshot.events.len(), 4);
            assert_eq!(snapshot.attempted, snapshot.completed + snapshot.partial() + snapshot.dropped);
        }

        #[test]
        fn dropped_events_do_not_evaluate_identity_or_clock_fields() {
            let recorder = Recorder::<1>::new();
            recorder.open(Kind::Build, 0, 0, 0, 7, 10).unwrap();
            assert_eq!(recorder.open_lazy(Kind::ProgramLink, || panic!("a dropped event must not read clocks or context ids")), None);
            assert_eq!(recorder.snapshot(0).dropped, 1);
            let waits = QueueWaits::new();
            waits.queued_in(&recorder, 0, false, 7, 20);
            assert_eq!(waits.taken_lazy(&recorder, 0, || panic!("a dropped queue span must not read an end clock")), 0);
        }

        #[test]
        fn unfinished_events_and_live_workers_publish_partial_never_final() {
            let recorder = Recorder::<4>::new();
            let done = recorder.open(Kind::Build, 1, 1, 1, 5, 10).unwrap();
            recorder.close(done, LOADED, 5, 20);
            recorder.open(Kind::Job, 2, 1, 0, 6, 30).unwrap();
            let snapshot = recorder.snapshot(0);
            assert_eq!((snapshot.completed, snapshot.open, snapshot.completion()), (1, 1, "partial"));
            let report = snapshot.render();
            assert!(report.contains("event 1 job open - 6 - 2 1 30 - 0\n"), "{report}");
            assert!(report.contains("completion partial"));
            let quiet = Recorder::<4>::new();
            quiet.close(quiet.open(Kind::Build, 1, 1, 0, 5, 1).unwrap(), COMPILED, 5, 2);
            assert_eq!(quiet.snapshot(1).completion(), "partial", "a live worker can still record");
            assert_eq!(quiet.snapshot(0).completion(), "unknown", "no barrier proves the last build");
            assert!(!quiet.snapshot(0).render().contains("final"));
        }

        #[test]
        fn a_span_ended_without_an_outcome_says_so() {
            let slot = Span::begin(Kind::Graphics, None, 9).slot();
            if let Some(slot) = slot {
                let event = snapshot().events.into_iter().find(|event| event.index == slot).unwrap();
                assert!(event.done);
                assert_eq!((event.outcome, event.aux, event.context), (NO_OUTCOME, 9, 0));
                assert!(event.end_ns.unwrap() >= event.begin_ns);
            }
        }

        #[test]
        fn queue_waits_span_send_to_take_in_lane_order() {
            let (recorder, waits) = (Recorder::<8>::new(), QueueWaits::new());
            waits.queued_in(&recorder, 1, true, 3, 100);
            waits.queued_in(&recorder, 0, false, 3, 110);
            assert_eq!(waits.taken_in(&recorder, 0, 9, 150), 2, "lane 0's job had slot 1");
            assert_eq!(waits.taken_in(&recorder, 1, 9, 170), 1);
            let events = recorder.snapshot(0).events;
            assert_eq!((events[0].begin_ns, events[0].end_ns, events[0].aux, events[0].begin_thread, events[0].end_thread), (100, Some(170), 0b11, 3, 9));
            assert_eq!((events[1].begin_ns, events[1].end_ns, events[1].aux), (110, Some(150), 0));
            assert_eq!(waits.taken_in(&recorder, 0, 9, 200), 0, "a pop with nothing pushed is unknown");
        }

        #[test]
        fn an_overwritten_queue_wait_stays_open_rather_than_misattributed() {
            let (recorder, waits) = (Recorder::<CAPACITY>::new(), QueueWaits::new());
            for i in 0..=RING as u64 {
                waits.queued_in(&recorder, 0, false, 1, i);
            }
            assert_eq!(waits.taken_in(&recorder, 0, 2, 1_000), 0, "its ring entry was overwritten");
            assert_eq!(waits.taken_in(&recorder, 0, 2, 1_001), 2);
            let snapshot = recorder.snapshot(0);
            assert_eq!((snapshot.attempted, snapshot.dropped, snapshot.completed, snapshot.open), (RING as u64 + 1, 1, 1, RING as u64 - 1));
            assert!(!snapshot.events[0].done);
        }

        #[test]
        fn a_full_report_at_its_widest_stays_bounded() {
            let recorder = Recorder::<CAPACITY>::new();
            for _ in 0..CAPACITY + 10 {
                if let Some(slot) = recorder.open(Kind::ExtensionQuery, u32::MAX, u32::MAX, u64::MAX, u32::MAX, u64::MAX) {
                    recorder.close(slot, NO_OUTCOME - 1, u32::MAX, u64::MAX);
                }
            }
            let report = recorder.snapshot(u64::MAX).render();
            assert!(report.len() < MAX_REPORT_BYTES, "{} bytes", report.len());
            assert_eq!(report.lines().filter(|line| line.starts_with("event ")).count(), CAPACITY);
            assert!(report.contains("attempted 266 recorded 256 completed 256 partial 0 open 0 reserved 0 dropped 10"));
        }

        #[test]
        fn sidecar_writer_preserves_existing_files_and_writes_bounded_new_output() {
            let directory = std::env::temp_dir().join(format!("zakuro-shader-attribution-{}-{}", std::process::id(), now_ns()));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("events.txt");
            std::fs::write(&path, b"preserve existing receipt").unwrap();
            assert_eq!(write_sidecar(&path).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&path).unwrap(), b"preserve existing receipt");
            std::fs::remove_file(&path).unwrap();
            let snapshot = write_sidecar(&path).unwrap();
            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(contents.starts_with(FORMAT));
            assert!(contents.len() <= MAX_REPORT_BYTES);
            assert_eq!(snapshot.attempted, snapshot.completed + snapshot.partial() + snapshot.dropped);
            assert_ne!(snapshot.completion(), "final");
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }

        #[test]
        fn contexts_are_numbered_as_first_seen_until_the_table_is_full() {
            let table: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
            assert_eq!([intern(&table, 0), intern(&table, 0x10), intern(&table, 0x20), intern(&table, 0x10), intern(&table, 0x30)], [0, 1, 2, 1, u32::MAX]);
        }

        #[test]
        fn kinds_round_trip_their_numbers() {
            for kind in KINDS {
                assert_eq!(Kind::from_u32(kind as u32), Some(kind));
            }
            assert_eq!(Kind::from_u32(0), None);
            assert_eq!(Kind::from_u32(KINDS.len() as u32 + 1), None);
        }
    }
}

#[cfg(test)]
mod compiler_release_tests {
    use super::*;

    fn red_pixels(gl: &glow::Context, program: glow::Program) -> [u8; 16] {
        // SAFETY: called on the GL worker with its current context.
        unsafe {
            let image = gl.create_renderbuffer().unwrap();
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(image));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, 2, 2);
            let framebuffer = gl.create_framebuffer().unwrap();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(image));
            assert_eq!(gl.check_framebuffer_status(glow::FRAMEBUFFER), glow::FRAMEBUFFER_COMPLETE);
            let vao = gl.create_vertex_array().unwrap();
            gl.bind_vertex_array(Some(vao));
            gl.viewport(0, 0, 2, 2);
            for cap in [glow::DEPTH_TEST, glow::SCISSOR_TEST, glow::CULL_FACE, glow::BLEND] { gl.disable(cap); }
            bind_program(gl, program);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let mut pixels = [0; 16];
            gl.read_pixels(0, 0, 2, 2, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut pixels)));
            gl.use_program(None);
            gl.delete_vertex_array(vao);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(framebuffer);
            gl.delete_renderbuffer(image);
            assert_eq!(gl.get_error(), glow::NO_ERROR);
            pixels
        }
    }

    const BINARY_VERTEX: &str = "#version 310 es\nvoid main() { vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0); }\n";
    const BINARY_FRAGMENT: &str = "#version 310 es\nprecision highp float;\nlayout(location=0) out vec4 color;\nvoid main() { color = vec4(1.0, 0.0, 0.0, 1.0); }\n";

    #[test]
    fn program_binary_none_and_some_paths_preserve_exact_pixels() {
        if separable() { eprintln!("monolithic binary cache fixture skipped in separable mode"); return; }
        let gles = Gles::new().expect("actual EGL context required for binary cache lifecycle test");
        gles.worker.run(|| {
            let gl = GL_CONTEXT.get().unwrap();
            let reads = BINARY_READS.get();
            let none = build_program(gl, BINARY_VERTEX, BINARY_FRAGMENT, None).unwrap();
            assert_eq!(BINARY_READS.get(), reads, "absent path must not request a binary");
            // SAFETY: the newly linked program belongs to this current context.
            unsafe { assert_eq!(gl.get_program_parameter_i32(none, glow::PROGRAM_BINARY_RETRIEVABLE_HINT), 0); }
            let path = std::env::temp_dir().join(format!("zakuro-binary-cache-{}-{}.bin", std::process::id(), none.0.get()));
            let _ = std::fs::remove_file(&path);
            let saved = build_program(gl, BINARY_VERTEX, BINARY_FRAGMENT, Some(&path)).unwrap();
            assert_eq!(BINARY_READS.get(), reads + 1);
            assert!(std::fs::read(&path).unwrap().len() > 4, "a usable driver binary is retained");
            // SAFETY: same live current-context program.
            unsafe { assert_ne!(gl.get_program_parameter_i32(saved, glow::PROGRAM_BINARY_RETRIEVABLE_HINT), 0); }
            let loaded = build_program(gl, BINARY_VERTEX, BINARY_FRAGMENT, Some(&path)).unwrap();
            assert_eq!(BINARY_READS.get(), reads + 1, "loading a good binary does not serialize it again");
            let expected: [u8; 16] = [255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255];
            for program in [none, saved, loaded] {
                assert_eq!(red_pixels(gl, program), expected);
                // SAFETY: all queued readback work completed above.
                unsafe { gl.delete_program(program); }
            }
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn separable_program_binary_none_skips_hint_and_serialization() {
        if std::env::var_os("ZAKURO_NO_PROGRAM_BINARY_CACHE").is_none() {
            eprintln!("separable no-binary-path fixture skipped; requires explicit ZAKURO_NO_PROGRAM_BINARY_CACHE=1");
            return;
        }
        assert!(program_cache_path("test", BINARY_VERTEX, BINARY_FRAGMENT).is_none());
        let mut gles = Gles::new().expect("actual EGL context required for separable no-path test");
        // Finish startup jobs before this isolated fixture can exit and
        // tear down the driver underneath its background compiler.
        while gles.compiler.as_ref().is_some_and(|compiler| !compiler.pending.is_empty()) {
            gles.take_compiled(true);
        }
        gles.worker.run(|| {
            let gl = GL_CONTEXT.get().unwrap();
            assert_ne!(pipeline_fn(0), 0, "actual separable program entry points required");
            let reads = BINARY_READS.get();
            let vertex = stage_program(gl, glow::VERTEX_SHADER, BINARY_VERTEX).unwrap();
            let fragment = stage_program(gl, glow::FRAGMENT_SHADER, BINARY_FRAGMENT).unwrap();
            for stage in [&vertex, &fragment] {
                // SAFETY: stages are linked programs in this current share group.
                unsafe { assert_eq!(gl.get_program_parameter_i32(stage.program, glow::PROGRAM_BINARY_RETRIEVABLE_HINT), 0); }
            }
            let pair = build_separable(gl, BINARY_VERTEX, BINARY_FRAGMENT).unwrap();
            assert_eq!(BINARY_READS.get(), reads, "neither stage requests a binary without a path");
            assert_eq!(red_pixels(gl, pair), [255, 0, 0, 255].repeat(4).as_slice());
            delete_program(gl, pair);
            drop((vertex, fragment));
            prune_stages(gl);
        });
    }

    #[test]
    fn cached_pipeline_binds_keep_pixels_and_skip_pair_lookups() {
        let mut gles = match Gles::new() {
            Ok(gles) => gles,
            Err(error) => { eprintln!("no OpenGL ES here, skipped: {error}"); return; }
        };
        // Complete the fixture's background prewarm before the test process
        // can exit and tear down the driver beneath a compiler thread.
        while gles.compiler.as_ref().is_some_and(|compiler| !compiler.pending.is_empty()) {
            gles.take_compiled(true);
        }
        if !separable() { eprintln!("pipeline cache fixture requires separable mode, skipped"); return; }
        gles.worker.run(|| {
            let gl = GL_CONTEXT.get().unwrap();
            let red = [255, 0, 0, 255].repeat(4);
            let blue = [0, 0, 255, 255].repeat(4);
            let first = build_separable(gl, BINARY_VERTEX, BINARY_FRAGMENT).unwrap();
            let fragment = BINARY_FRAGMENT.replace("1.0, 0.0, 0.0", "0.0, 0.0, 1.0");
            let second = build_separable(gl, BINARY_VERTEX, &fragment).unwrap();
            let before = PAIR_LOOKUPS.get();
            assert_eq!(red_pixels(gl, first).as_slice(), red);
            let pipeline = PIPELINES.with(|pipelines| pipelines.borrow()[&first]);
            for _ in 0..8 {
                assert_eq!(red_pixels(gl, second).as_slice(), blue);
                assert_eq!(red_pixels(gl, first).as_slice(), red);
            }
            assert_eq!(PAIR_LOOKUPS.get(), before + 2, "only first use of each pair looks up global stage leases");
            assert_eq!(PIPELINES.with(|pipelines| pipelines.borrow()[&first]), pipeline);
            delete_program(gl, first);
            assert!(!PIPELINES.with(|pipelines| pipelines.borrow().contains_key(&first)));
            let replacement = build_separable(gl, BINARY_VERTEX, BINARY_FRAGMENT).unwrap();
            assert_eq!(red_pixels(gl, replacement).as_slice(), red);
            assert_eq!(PAIR_LOOKUPS.get(), before + 3, "a new pair cannot inherit a retired pipeline");
            delete_program(gl, replacement);
            delete_program(gl, second);
            prune_stages(gl);
        });
    }

}
