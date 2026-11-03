//! building GL programs: the cache on disk and the manifest of what a
//! game used, the compiler threads and their queue, separable stages, and
//! noting a shader the driver's compiler crashed on.

use super::*;

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
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let dir = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?;
    Some(dir.join("zakuro").join("gles").join(format!("{:016x}.bin", fnv(&[renderer, vertex, fragment]))))
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
    at_compiler([vertex, fragment]);
    // SAFETY: on the GL thread, the shaders are deleted once linked
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (stage, source) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let shader = gl.create_shader(stage)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                shaders.into_iter().chain([shader]).for_each(|shader| gl.delete_shader(shader));
                gl.delete_program(program);
                return Err(error);
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
            let error = format!("could not link a program, {}", gl.get_program_info_log(program));
            gl.delete_program(program);
            return Err(error);
        }
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

pub(super) type StagePrograms = rustc_hash::FxHashMap<(usize, u32, u64), glow::Program>;
pub(super) type Separate = rustc_hash::FxHashMap<(usize, glow::Program), (glow::Program, glow::Program)>;

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
    /// the GL thread's pipelines, which unlike programs are its context's
    /// own, by the stand-in they are for.
    pub(super) static PIPELINES: std::cell::RefCell<rustc_hash::FxHashMap<glow::Program, u32>> = std::cell::RefCell::new(Default::default());
}

/// a stage of a separable program, built or loaded once and kept.
pub(super) fn stage_program(gl: &glow::Context, stage: u32, source: &str) -> Result<glow::Program, String> {
    let key = (SHARE_GROUP.get(), stage, fnv(&[source]));
    if let Some(&found) = STAGE_PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(Default::default).get(&key) {
        return Ok(found);
    }
    let _compiling = compiler_lock();
    let _watched = Watched;
    let started = std::time::Instant::now();
    let cached = RENDERER_NAME.get().and_then(|name| program_cache_path(name, if stage == glow::VERTEX_SHADER { "separable vertex" } else { "separable fragment" }, source));
    // SAFETY: the context is current on this thread, the pipeline calls
    // were loaded from it and take the program's name
    let program = unsafe {
        let program = gl.create_program()?;
        let parameter: extern "system" fn(u32, u32, i32) = std::mem::transmute(pipeline_fn(0));
        parameter(program.0.get(), PROGRAM_SEPARABLE, 1);
        let loaded = cached.as_deref().and_then(|path| std::fs::read(path).ok()).filter(|bytes| bytes.len() > 4).is_some_and(|bytes| {
            let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            gl.program_binary(program, &glow::ProgramBinary { buffer: bytes[4..].to_vec(), format });
            gl.get_program_link_status(program)
        });
        if !loaded {
            let (shader, kept) = match compiled_shader(stage, source) {
                Some(shader) => (shader, true),
                None => {
                    let shader = gl.create_shader(stage)?;
                    gl.shader_source(shader, source);
                    at_compiler([source, ""]);
                    gl.compile_shader(shader);
                    (shader, false)
                }
            };
            if !kept && !gl.get_shader_compile_status(shader) {
                let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                gl.delete_shader(shader);
                gl.delete_program(program);
                return Err(error);
            }
            gl.attach_shader(program, shader);
            gl.program_binary_retrievable_hint(program, true);
            at_compiler([source, ""]);
            gl.link_program(program);
            gl.detach_shader(program, shader);
            if !kept {
                keep_shader(gl, stage, source, shader);
            }
            if !gl.get_program_link_status(program) {
                let error = format!("could not link a shader stage, {}", gl.get_program_info_log(program));
                gl.delete_program(program);
                return Err(error);
            }
            if let (Some(path), Some(binary)) = (cached.as_deref(), gl.get_program_binary(program)) {
                let mut bytes = binary.format.to_le_bytes().to_vec();
                bytes.extend_from_slice(&binary.buffer);
                let _ = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(path, bytes));
            }
        }
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
    if let Some(&found) = stages.get(&key) {
        // SAFETY: the context is current, nothing has used this one
        unsafe { gl.delete_program(program) };
        return Ok(found);
    }
    stages.insert(key, program);
    Ok(program)
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
    let stages = if separable() { SEPARATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().and_then(|separate| separate.get(&(SHARE_GROUP.get(), program)).copied()) } else { None };
    // SAFETY: on the GL thread, the program and stages were made in contexts
    // sharing its objects, the pipeline in its own
    unsafe {
        let Some((vertex, fragment)) = stages else {
            gl.use_program(Some(program));
            return;
        };
        let pipeline = PIPELINES.with(|pipelines| {
            *pipelines.borrow_mut().entry(program).or_insert_with(|| {
                let generate: extern "system" fn(i32, *mut u32) = std::mem::transmute(pipeline_fn(1));
                let stages_of: extern "system" fn(u32, u32, u32) = std::mem::transmute(pipeline_fn(3));
                let mut pipeline = 0u32;
                generate(1, &mut pipeline);
                stages_of(pipeline, VERTEX_SHADER_BIT, vertex.0.get());
                stages_of(pipeline, FRAGMENT_SHADER_BIT, fragment.0.get());
                pipeline
            })
        });
        let bind: extern "system" fn(u32) = std::mem::transmute(pipeline_fn(4));
        gl.use_program(None);
        bind(pipeline);
    }
}

/// deletes a program a draw used, a separable one's stand-in and pipeline,
/// its stages kept for others.
pub(super) fn delete_program(gl: &glow::Context, program: glow::Program) {
    let separate = SEPARATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut().and_then(|separate| separate.remove(&(SHARE_GROUP.get(), program))).is_some();
    // SAFETY: on the GL thread, after every draw that used it
    unsafe {
        if separate {
            if let Some(pipeline) = PIPELINES.with(|pipelines| pipelines.borrow_mut().remove(&program)) {
                let delete: extern "system" fn(i32, *const u32) = std::mem::transmute(pipeline_fn(2));
                delete(1, &pipeline);
            }
        }
        gl.delete_program(program);
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
    if separable() {
        return build_separable(gl, vertex, fragment);
    }
    let _compiling = compiler_lock();
    let _watched = Watched;
    // SAFETY: the context is current on this thread, the shaders are deleted
    // once linked into the program
    unsafe {
        let program = gl.create_program()?;
        let loaded = cached.and_then(|path| std::fs::read(path).ok()).filter(|bytes| bytes.len() > 4).is_some_and(|bytes| {
            let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            gl.program_binary(program, &glow::ProgramBinary { buffer: bytes[4..].to_vec(), format });
            gl.get_program_link_status(program)
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
                        gl.compile_shader(shader);
                        (shader, false)
                    }
                });
            }
            for (index, &(shader, kept)) in shaders.iter().enumerate() {
                at_compiler([sources[index].1, ""]);
                if !kept && !gl.get_shader_compile_status(shader) {
                    let error = format!("could not compile a shader, {}", gl.get_shader_info_log(shader));
                    for &(shader, kept) in &shaders {
                        if !kept {
                            gl.delete_shader(shader);
                        }
                    }
                    gl.delete_program(program);
                    return Err(error);
                }
                stages[index] = began.elapsed();
            }
            for &(shader, _) in &shaders {
                gl.attach_shader(program, shader);
            }
            gl.program_binary_retrievable_hint(program, true);
            at_compiler([vertex, fragment]);
            let began = std::time::Instant::now();
            gl.link_program(program);
            for (&(shader, kept), (stage, source)) in shaders.iter().zip(sources) {
                gl.detach_shader(program, shader);
                if !kept {
                    keep_shader(gl, stage, source, shader);
                }
            }
            let linked = gl.get_program_link_status(program);
            stages[2] = began.elapsed();
            count_build(vertex, fragment, waited, stages);
            if !linked {
                let error = format!("could not link a program, {}", gl.get_program_info_log(program));
                gl.delete_program(program);
                return Err(error);
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
}

impl Queue {
    pub(super) fn new() -> Queue {
        Queue { jobs: std::sync::Mutex::new((Default::default(), Default::default(), false)), ready: std::sync::Condvar::new() }
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
            jobs.1.push_back(job);
        } else {
            jobs.0.push_back(job);
        }
        self.ready.notify_one();
        Ok(())
    }

    /// the next job, waiting for one, or none once closed.
    pub(super) fn take(&self) -> Option<Compile> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(job) = jobs.0.pop_front().or_else(|| jobs.1.pop_front()) {
                return Some(job);
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
        if crate::switch("ZAKURO_SYNC_SHADERS", "shaders compiled when a draw needs them") {
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
                    let Some(gl) = egl.shared_context() else {
                        let _ = ready.send(false);
                        return;
                    };
                    SHARE_GROUP.set(egl as *const Egl as usize);
                    // SAFETY: lowers the priority of this thread alone, which is
                    // what Linux takes 0 for, the game's threads going first
                    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
                    // a driver that can compile in threads of its own may use as
                    // many as it likes for this context's programs
                    if gl.supported_extensions().contains("GL_KHR_parallel_shader_compile") {
                        // SAFETY: the context is current, the extension names it
                        unsafe { gl.max_shader_compiler_threads(u32::MAX) };
                        log::info!("the driver compiles this thread's shaders in threads of its own");
                    }
                    let _ = ready.send(true);
                    // one job at a time off the shared queue, the lock let go
                    // before the build
                    while let Some(job) = received.take() {
                        let result = build_program(&gl, &job.vertex, &job.fragment, job.cached.as_deref());
                        // SAFETY: the context is current on this thread
                        unsafe { gl.finish() };
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
