//! compiling the generated C, one compiler per core, into a shared library
//! a host loads or a static one a program links in.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// floating point has to round exactly the way the interpreter does, so
/// nothing may be fused into a multiply-add.
const FLAGS: &[&str] = &["-O2", "-ffp-contract=off", "-fno-math-errno", "-w"];
/// what only matters for ELF and Mach-O, and that compilers for Windows
/// refuse or ignore. no stack protector, which would tie the code to one C
/// library: it calls none, so one build loads on glibc and Android alike.
const UNIX_FLAGS: &[&str] = &["-fPIC", "-fvisibility=hidden", "-fno-stack-protector"];

/// what progress hears after each file, how many are done and of how
/// many, answering whether to go on.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) -> bool + Sync);

/// whether a program runs, asked for its version.
fn runs(program: &str) -> bool {
    Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// the first of the tools that runs, or the one named in variable.
fn tool(variable: &str, tools: &[&str]) -> String {
    std::env::var(variable)
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| tools.iter().find(|tool| runs(tool)).map(|tool| tool.to_string()))
        .unwrap_or_else(|| tools[0].to_owned())
}

/// the C compiler, from CC or else the first there is. on Windows MinGW's
/// gcc comes first, it links a DLL with nothing else installed.
fn compiler() -> String {
    let compilers: &[&str] = if cfg!(windows) { &["gcc", "clang", "cc"] } else { &["cc", "gcc", "clang"] };
    tool("CC", compilers)
}

/// whether there is a C compiler to build with.
pub fn check() -> Result<(), String> {
    let compiler = compiler();
    if runs(&compiler) {
        return Ok(());
    }
    let suggestion = if cfg!(windows) {
        "install MinGW-w64's gcc, from MSYS2 or WinLibs, or LLVM's clang"
    } else {
        "install one such as gcc or clang"
    };
    Err(format!("there is no C compiler ({compiler}), {suggestion}, or name it in CC"))
}

/// compiles sources, file names inside dir, and links them into library,
/// telling progress how many of them are done after each one.
pub fn compile(dir: &Path, sources: &[String], library: &Path, progress: Progress) -> Result<(), String> {
    shared(&objects(dir, sources, progress)?, library)
}

/// how many files compile at once: one a core, as many as memory has room
/// for, a file of recompiled code taking gcc up to a gigabyte and Docker on
/// Windows giving its VM half the PC's.
pub fn jobs() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|info| info.lines().find_map(|line| line.strip_prefix("MemAvailable:"))?.split_whitespace().next()?.parse::<usize>().ok());
    available.map_or(cores, |kib| cores.min((kib / (1280 * 1024)).max(1)))
}

/// compiles sources, file names inside dir, each into an object beside it,
/// stopping when progress says so.
pub fn objects(dir: &Path, sources: &[String], progress: Progress) -> Result<Vec<PathBuf>, String> {
    let compiler = compiler();
    let jobs = jobs();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let failures = Mutex::new(Vec::new());
    // more flags for the compiler, RECOMP_CFLAGS=-DRECOMP_FASTMEM for code
    // that reaches memory through the host's view of all of it
    let extra = std::env::var("RECOMP_CFLAGS").unwrap_or_default();
    let extra = extra.as_str();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                while let Some(source) = sources.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let path = dir.join(source);
                    let status = Command::new(&compiler)
                        .args(FLAGS)
                        .args(if cfg!(windows) { &[][..] } else { UNIX_FLAGS })
                        .args(extra.split_whitespace())
                        .arg("-I")
                        .arg(dir)
                        .arg("-c")
                        .arg(&path)
                        .arg("-o")
                        .arg(path.with_extension("o"))
                        .status();
                    if !status.is_ok_and(|s| s.success()) {
                        failures.lock().unwrap().push(source.clone());
                    }
                    if !progress(done.fetch_add(1, Ordering::Relaxed) + 1, sources.len()) {
                        stopped.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    if stopped.into_inner() {
        return Err("stopped".to_owned());
    }
    let failures = failures.into_inner().unwrap();
    if !failures.is_empty() {
        return Err(format!("{} failed to compile, {}", failures.len(), failures.join(" ")));
    }
    Ok(sources.iter().map(|source| dir.join(source).with_extension("o")).collect())
}

/// links objects into a shared library. on Windows gcc's runtime goes in
/// with it, so the DLL needs no other DLL beside it.
pub fn shared(objects: &[PathBuf], library: &Path) -> Result<(), String> {
    let compiler = compiler();
    let mut command = Command::new(&compiler);
    if cfg!(windows) && compiler.contains("gcc") {
        command.arg("-static-libgcc");
    }
    // nothing from a C library, gcc's own helpers linked in after the code
    if !cfg!(windows) {
        command.arg("-nostdlib");
    }
    command.arg("-shared").arg("-o").arg(library).args(objects);
    if !cfg!(windows) && compiler.contains("gcc") {
        command.arg("-lgcc");
    }
    let status = command.status();
    match status {
        Ok(status) if status.success() => Ok(()),
        _ => Err("linking failed".to_owned()),
    }
}

/// puts objects into a static library.
pub fn archive(objects: &[PathBuf], library: &Path) -> Result<(), String> {
    // ar adds to what is there, which could hold objects no longer built
    if library.exists() {
        std::fs::remove_file(library).map_err(|e| format!("could not replace {}, {e}", library.display()))?;
    }
    let ar = tool("AR", &["ar", "llvm-ar"]);
    let status = Command::new(&ar).arg("rcs").arg(library).args(objects).status();
    match status {
        Ok(status) if status.success() => Ok(()),
        _ => Err("archiving failed".to_owned()),
    }
}
