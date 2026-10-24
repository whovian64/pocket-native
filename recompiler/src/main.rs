//! 3dsrecomp, a static recompiler for 3DS titles, whose code runs in any
//! emulator that implements recomp.h, Zakuro being the one it was made for.
//!
//! analyze reports how much of a title's code a generic pass can discover
//! on its own, build turns what it found into C and compiles it into a
//! library the emulator can load, port makes a program of the title with
//! that code linked in, and verify, built with the verify feature, checks
//! the library against Zakuro's interpreter.

use std::path::{Path, PathBuf};
use std::process::exit;

use recomp3ds::build::{self, Event};
use recomp3ds::discover::{self, Analysis, Byte, Mode, Program, Source};
use recomp3ds::rom::Title;
use recomp3ds::{compile, port};
#[cfg(feature = "verify")]
use recomp3ds::{abi, codegen, image, imported, module_files, static_module, verify};

const USAGE: &str = "\
usage, 3dsrecomp analyze <rom>
       3dsrecomp build <rom> [<dir>] [--overrides <file or dir>]
       3dsrecomp port <rom> <dir> [--overrides <file or dir>] [--name <name>] [--zakuro <checkout>]
       3dsrecomp verify <rom> <library> [count]

build without a dir works in the cache and installs the library where
Zakuro finds it on its own.";

/// what the options on the command line ask for.
#[derive(Default)]
struct Options {
    /// C files of functions written by hand, see docs/overrides.md.
    overrides: Option<PathBuf>,
    /// what port calls the program.
    name: Option<String>,
    /// a Zakuro checkout for port to build against.
    zakuro: Option<PathBuf>,
}

fn main() {
    let mut words = Vec::new();
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next().unwrap_or_else(|| {
                eprintln!("{arg} needs a value");
                exit(2);
            })
        };
        match arg.as_str() {
            "--overrides" => options.overrides = Some(value().into()),
            "--name" => options.name = Some(value()),
            "--zakuro" => options.zakuro = Some(value().into()),
            other if other.starts_with("--") => {
                eprintln!("unknown option {other}\n{USAGE}");
                exit(2);
            }
            _ => words.push(arg),
        }
    }
    match words.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["analyze", rom] => analyze(rom),
        ["build", rom] => build(rom, None, &options),
        ["build", rom, dir] => build(rom, Some(Path::new(dir)), &options),
        ["port", rom, dir] => port(rom, Path::new(dir), &options),
        #[cfg(feature = "verify")]
        ["verify", rom, library] => check(rom, Path::new(library), 500),
        #[cfg(feature = "verify")]
        ["verify", rom, library, count] => check(rom, Path::new(library), count.parse().unwrap_or(500)),
        #[cfg(not(feature = "verify"))]
        ["verify", ..] => {
            eprintln!("this build has no verify, build it with --features verify");
            exit(2);
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

/// the title and its programs, the executable first and then its modules.
fn load(path: &str) -> (Title, Vec<(String, Program)>) {
    let title = Title::load(path).unwrap_or_else(|error| {
        eprintln!("could not load {path}, {error}");
        exit(1);
    });
    let programs = recomp3ds::programs(&title).unwrap_or_else(|error| {
        eprintln!("could not read the code, {error}");
        exit(1);
    });
    (title, programs)
}

/// builds the library in dir, or when there is none in the cache, and then
/// installs it where hosts look for it.
fn build(path: &str, given: Option<&Path>, options: &Options) {
    let build_options = build::Options { dir: given, overrides: options.overrides.as_deref(), cancel: None };
    if let Err(error) = build::build(Path::new(path), &build_options, &print_event) {
        eprintln!("{error}");
        exit(1);
    }
}

/// prints what a build does, compiling every twentieth of the way, which a
/// frontend running build can follow.
fn print_event(event: Event) {
    match event {
        Event::Generated { files, bytes, overrides } => {
            println!("wrote {files} files, {} MiB of C, {overrides} overrides", bytes >> 20)
        }
        Event::Note(note) => println!("{note}"),
        Event::Compiled { done, total } => {
            if done == total || done * 20 / total != (done - 1) * 20 / total {
                println!("compiled {done} of {total}");
            }
        }
        Event::Built { library, took } => println!("built {} in {took:.1?}", library.display()),
        Event::Installed(path) => println!("installed {}, which Zakuro runs for the game from now on", path.display()),
    }
}

/// a Cargo project at dir that builds the title into a program of its own.
fn port(path: &str, dir: &Path, options: &Options) {
    let generated = build::generate(Path::new(path), |_| dir.join("code"), options.overrides.as_deref(), &print_event)
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            exit(1);
        });
    let (title, code) = (&generated.title, &generated.dir);
    let start = std::time::Instant::now();
    let progress = |done, total| {
        print_event(Event::Compiled { done, total });
        true
    };
    let archived = compile::objects(code, &generated.sources, &progress)
        .and_then(|objects| compile::archive(&objects, &code.join("librecomp.a")));
    if let Err(error) = archived {
        eprintln!("{error}");
        exit(1);
    }
    println!("built {} in {:.1?}", code.join("librecomp.a").display(), start.elapsed());

    let name = options.name.clone().unwrap_or_else(|| port::package_name(&title.exheader.title));
    let written = port::write_project(dir, &name, &title.exheader.title, title.program_id(), options.zakuro.as_deref());
    if let Err(error) = written {
        eprintln!("{error}");
        exit(1);
    }
    println!("wrote the project for {name}, build it with cargo build --release in {}", dir.display());
    println!("then run target/release/{name} with the path to the game");
}

/// runs up to count of the recompiled functions of each program against
/// the interpreter, the modules one at a time, each loaded at the same place.
#[cfg(feature = "verify")]
fn check(path: &str, library: &Path, count: usize) {
    let (title, programs) = load(path);
    let mut library = abi::Library::open(library).unwrap_or_else(|error| {
        eprintln!("could not open {}, {error}", library.display());
        exit(1);
    });
    let image = image::Image::from_title(&title).unwrap_or_else(|error| {
        eprintln!("could not read the code, {error}");
        exit(1);
    });
    let regions = verify::regions(
        (image.text.base, &image.text.bytes),
        (image.rodata.base, &image.rodata.bytes),
        (image.data.base, &image.data.bytes),
        title.exheader.bss_size,
    );
    let start = std::time::Instant::now();

    let executable = verify::verify(
        "executable",
        &verify::Memory::new(regions.clone()),
        &library,
        &sample(&discover::analyze(&programs[0].1), 0, count),
    );
    print_report("executable", &executable);

    let mut modules = verify::Report::default();
    let files = module_files(&title);
    let crs = static_module(&title);
    for (module, bytes) in &files {
        let Some(index) = library.module_index(&module.name) else { continue };
        let Some(program) = module.program(bytes, &imported(&files, crs.as_ref(), module)) else { continue };
        let mut memory = regions.clone();
        memory.push(verify::Region {
            base: verify::MODULE_BASE,
            bytes: module.image(bytes, verify::MODULE_BASE, verify::IMPORT_STUB),
            writable: true,
        });
        library.place(index, verify::MODULE_BASE);
        let report = verify::verify(
            &module.name,
            &verify::Memory::new(memory),
            &library,
            &sample(&discover::analyze(&program), verify::MODULE_BASE, count),
        );
        library.place(index, 0);
        modules.add(&report);
    }
    print_report("modules", &modules);
    println!("took {:.1?}", start.elapsed());
}

/// up to count of the functions that became C, spread over the program, as
/// addresses at base with bit 0 set for Thumb.
#[cfg(feature = "verify")]
fn sample(analysis: &Analysis, base: u32, count: usize) -> Vec<u32> {
    let functions: Vec<u32> = analysis
        .functions
        .iter()
        .filter(|&(&entry, f)| codegen::recompiles(entry, f))
        .map(|(&entry, f)| (base + entry) | (f.mode == Mode::Thumb) as u32)
        .collect();
    // VERIFY_ONLY=address checks that one function alone
    if let Some(only) = std::env::var("VERIFY_ONLY").ok().and_then(|v| u32::from_str_radix(&v, 16).ok()) {
        return functions.into_iter().filter(|&f| f == only).collect();
    }
    let step = (functions.len() / count.max(1)).max(1);
    functions.into_iter().step_by(step).take(count).collect()
}

#[cfg(feature = "verify")]
fn print_report(name: &str, report: &verify::Report) {
    println!(
        "{name:<11} {} functions, {} returned, {} reached an svc, {} stuck, {} stopped otherwise, {} mismatched",
        report.tested, report.returned, report.svc, report.stuck, report.other, report.mismatched
    );
    println!(
        "{:<11} instructions {} recompiled, {} through the fallback, {} interpreted alone",
        "", report.native, report.fallbacks, report.interpreted
    );
}

fn analyze(path: &str) {
    let (title, mut programs) = load(path);
    let module_count = programs.len() - 1;
    let start = std::time::Instant::now();
    let mut analyses: Vec<Analysis> = programs.iter().map(|(_, program)| discover::analyze(program)).collect();
    let missed = sort_hints(&build::hints(title.program_id()), &programs[0].1, &analyses[0]);
    let (labels, functions) = build::apply_hints(&title, &mut programs, &mut analyses);
    let elapsed = start.elapsed();

    println!("title       {}, {} modules", title.exheader.title, module_count);
    if labels + functions > 0 {
        println!("hints       {labels} ways into functions, {functions} new functions, from where Zakuro interpreted");
    }
    println!();
    println!("                  KiB  functions   code  literals  unreached  indirect  switches    svc  dead ends");
    let executable = Totals::of(&analyses[0]);
    let modules = analyses[1..].iter().map(Totals::of).fold(Totals::default(), Totals::add);
    executable.print("executable");
    modules.print("modules");
    executable.add(modules).print("all");
    println!();

    let functions = || analyses.iter().flat_map(|analysis| analysis.functions.values());
    let from = |source: Source| functions().filter(|f| f.source == source).count();
    println!(
        "found by    {} calls, {} pointers, {} relocations, {} exports, {} imports, {} scanned, {} hinted, {} entry",
        from(Source::Call),
        from(Source::Pointer),
        from(Source::Relocation),
        from(Source::Export),
        from(Source::Import),
        from(Source::Scan),
        from(Source::Hint),
        from(Source::Entry)
    );
    let thumb = functions().filter(|f| f.mode == Mode::Thumb).count();
    let instructions: usize = functions().map(|f| f.instructions.len()).sum();
    println!("thumb       {thumb} functions");
    println!("decoded     {instructions} instructions, shared code once per function");
    println!("analysis    {elapsed:.2?}");
    println!();

    if !missed.is_empty() {
        println!("what the analysis missed of where Zakuro interpreted, and an example of each");
        for (kind, (count, example)) in &missed {
            println!("  {count:>7}  {kind:<58} 0x{example:08X}");
        }
        println!();
    }

    println!("largest unreached runs");
    let mut gaps: Vec<_> = programs
        .iter()
        .zip(&analyses)
        .flat_map(|((name, program), analysis)| {
            analysis.largest_gaps(program.text.base, 8).into_iter().map(move |(start, length)| (name, start, length))
        })
        .collect();
    gaps.sort_by_key(|gap| std::cmp::Reverse(gap.2));
    for (name, start, length) in gaps.into_iter().take(10) {
        println!("  {name:<28} 0x{start:08X}  {length} bytes");
    }
}

/// sorts the places Zakuro interpreted in the executable by what the
/// analysis, without them, made of each and of what comes before it, for
/// telling what discovery misses in a title. each kind with its count and
/// one place, odd for Thumb.
fn sort_hints(hints: &[u32], program: &Program, analysis: &Analysis) -> std::collections::BTreeMap<String, (usize, u32)> {
    use recomp3ds::arm::{self, Flow};
    let text = &program.text;
    let mut owner = std::collections::HashMap::new();
    for function in analysis.functions.values() {
        for &address in &function.instructions {
            owner.entry(address).or_insert(function);
        }
    }
    let pointed: std::collections::HashSet<u32> =
        program.seeds.iter().filter(|(_, source)| *source == Source::Pointer).map(|&(value, _)| value).collect();
    let flow_name = |flow: Flow| match flow {
        Flow::Branch { link: true, .. } | Flow::CallOtherMode { .. } => "a call",
        Flow::Branch { link: false, .. } => "a branch",
        Flow::BranchRegister { link: true, .. } => "a call through a register",
        Flow::BranchRegister { link: false, .. } => "bx to a register",
        Flow::Return => "a return",
        Flow::JumpTable | Flow::AddressTable { .. } => "a switch",
        Flow::IndirectJump => "another jump to a register",
        Flow::LoadPcLiteral { .. } => "ldr pc from a literal",
        Flow::LiteralLoad { .. } | Flow::Other => "an ordinary instruction",
        Flow::Svc => "an svc",
        Flow::Undefined => "something that is not code",
    };
    let before = |address: u32, thumb: bool| -> &'static str {
        if thumb {
            let Some(half) = address.checked_sub(2).and_then(|at| text.read16(at)) else { return "nothing" };
            // the second half of a bl pair, the call starts two before
            if half >> 11 == 0b11111 || half >> 11 == 0b11101 {
                return "a call";
            }
            flow_name(recomp3ds::thumb::decode(half, text.read16(address), address - 2).0.flow)
        } else {
            let Some(word) = address.checked_sub(4).and_then(|at| text.read32(at)) else { return "nothing" };
            // mov lr, pc before the instruction before, the old way of
            // calling through a register
            let linked = address.checked_sub(8).and_then(|at| text.read32(at)).is_some_and(|w| w & 0x0FFF_FFFF == 0x01A0_E00F);
            if linked {
                return "mov lr, pc and a jump, a call";
            }
            flow_name(arm::decode(word, address - 4).flow)
        }
    };
    let mut kinds = std::collections::BTreeMap::new();
    for &hint in hints {
        let (address, thumb) = (hint & !1, hint & 1 != 0);
        if !text.contains(address) {
            continue;
        }
        let mode = if thumb { Mode::Thumb } else { Mode::Arm };
        let kind = match owner.get(&address) {
            Some(function) if function.mode == mode => {
                if function.labels.contains(&address) {
                    "already a way in, run anyway (another reason)".to_owned()
                } else {
                    format!("inside a function, after {}", before(address, thumb))
                }
            }
            Some(_) => format!("decoded as {} code, run as the other", if thumb { "ARM" } else { "Thumb" }),
            None => {
                let byte = analysis.map[(address - text.base) as usize];
                let what = match byte {
                    Byte::Literal => "taken for data",
                    _ => "unreached",
                };
                let starts = if thumb {
                    // push {..., lr}
                    if text.read16(address).is_some_and(|half| half & 0xFF00 == 0xB500) { ", push lr" } else { "" }
                } else if text.read32(address).is_some_and(|word| word & 0xFFFF_4000 == 0xE92D_4000) {
                    ", push lr"
                } else {
                    ""
                };
                let pointer = if pointed.contains(&hint) { ", a pointer in data" } else { "" };
                format!("{} {what}{starts}{pointer}, after {}", if thumb { "Thumb" } else { "ARM" }, before(address, thumb))
            }
        };
        kinds.entry(kind).or_insert((0, hint)).0 += 1;
    }
    kinds
}

/// the numbers the report shows for a program, or for several added up.
#[derive(Default, Clone, Copy)]
struct Totals {
    text: usize,
    functions: usize,
    code: usize,
    literal: usize,
    indirect: usize,
    switches: usize,
    svc: usize,
    dead_ends: usize,
}

impl Totals {
    fn of(analysis: &Analysis) -> Totals {
        Totals {
            text: analysis.map.len(),
            functions: analysis.functions.len(),
            code: analysis.count(Byte::Code),
            literal: analysis.count(Byte::Literal),
            indirect: analysis.indirect_sites,
            switches: analysis.jump_tables,
            svc: analysis.svc_sites,
            dead_ends: analysis.dead_ends,
        }
    }

    fn add(self, other: Totals) -> Totals {
        Totals {
            text: self.text + other.text,
            functions: self.functions + other.functions,
            code: self.code + other.code,
            literal: self.literal + other.literal,
            indirect: self.indirect + other.indirect,
            switches: self.switches + other.switches,
            svc: self.svc + other.svc,
            dead_ends: self.dead_ends + other.dead_ends,
        }
    }

    fn print(&self, name: &str) {
        let percent = |bytes: usize| bytes as f64 * 100.0 / self.text.max(1) as f64;
        let unreached = self.text - self.code - self.literal;
        println!(
            "{name:<12} {:>9} {:>10} {:>5.1}% {:>8.1}% {:>9.1}% {:>9} {:>9} {:>6} {:>10}",
            self.text / 1024,
            self.functions,
            percent(self.code),
            percent(self.literal),
            percent(unreached),
            self.indirect,
            self.switches,
            self.svc,
            self.dead_ends
        );
    }
}
