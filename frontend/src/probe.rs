//! pocket-native --probe <rom> [frames]: runs any game as fast as it goes,
//! without a gamepad, pressing A now and then and Start once in a while, and
//! writes what happened into probe-<title id>/: how far it got and how fast,
//! the warnings and errors it met, which are what the engine still lacks for
//! it, and both screens every so often.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use zakuro_core::kernel::thread::ThreadStatus;
use zakuro_core::services::hid::PadState;
use zakuro_core::{FrameOutcome, System};

/// passes everything on to env_logger and keeps the warnings and errors,
/// numbers left out so that alike ones count together, with how often and
/// the frame each first came at.
struct Recorder {
    inner: env_logger::Logger,
    seen: Mutex<BTreeMap<String, (u64, u64)>>,
}

static FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RECORDER: std::sync::OnceLock<Recorder> = std::sync::OnceLock::new();

impl log::Log for Recorder {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata) || metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if record.level() <= log::Level::Warn {
            let text: String = format!("{} {}", record.level(), record.args()).chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
            let frame = FRAME.load(std::sync::atomic::Ordering::Relaxed);
            self.seen.lock().unwrap().entry(text).or_insert((0, frame)).0 += 1;
        }
        if self.inner.matches(record) {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// logs as main does, keeping the warnings for the report.
pub fn start_logging() {
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let level = inner.filter().max(log::LevelFilter::Warn);
    let recorder = RECORDER.get_or_init(|| Recorder { inner, seen: Mutex::new(BTreeMap::new()) });
    if log::set_logger(recorder).is_ok() {
        log::set_max_level(level);
    }
}

pub struct Probe {
    pub frames: u64,
    out: PathBuf,
    started: Instant,
    shots: Vec<String>,
    /// ZAKURO_PROBE_SPEED, how fast a handheld would run the game, the frames
    /// it would be behind, and the frames left out to catch up.
    speed: Option<f64>,
    late: f64,
    skipped: u64,
    /// for each screen, whether the last two looks found it black, and the
    /// times it went black for one look between pictures.
    black: [[bool; 2]; 2],
    flashes: [u64; 2],
}

impl Probe {
    pub fn new(title_id: u64, frames: u64) -> Probe {
        let out = PathBuf::from(format!("probe-{title_id:016X}"));
        let _ = std::fs::create_dir_all(&out);
        let speed = std::env::var("ZAKURO_PROBE_SPEED").ok().and_then(|speed| speed.parse().ok()).filter(|&speed: &f64| speed > 0.0);
        Probe { frames, out, started: Instant::now(), shots: Vec::new(), speed, late: 0.0, skipped: 0, black: [[false; 2]; 2], flashes: [0; 2] }
    }

    /// whether a handheld running at the probe's speed would be behind, as
    /// the frontend judges it.
    pub fn behind(&self) -> bool {
        self.speed.is_some() && self.late > 0.5
    }

    /// a frame done, drawn or skipped, at the probe's speed.
    pub fn ran(&mut self, skipped: bool) {
        self.skipped += skipped as u64;
        if let Some(speed) = self.speed {
            self.late = (self.late + if skipped { 0.0 } else { 1.0 / speed - 1.0 } - if skipped { 0.5 } else { 0.0 }).clamp(0.0, 4.0);
        }
    }

    /// looks at both screens, counting a black one between two that are not.
    pub fn watch(&mut self, system: &mut System) {
        if std::env::var_os("ZAKURO_PROBE_NO_WATCH").is_some() {
            return;
        }
        for (index, screen) in [zakuro_common::Screen::Top, zakuro_common::Screen::Bottom].into_iter().enumerate() {
            let (pixels, _, _) = system.read_screen_scaled(screen);
            let black = pixels.chunks(4).all(|pixel| pixel[0] < 8 && pixel[1] < 8 && pixel[2] < 8);
            let [before, last] = self.black[index];
            if !before && last && !black {
                self.flashes[index] += 1;
                log::debug!("black flash on screen {index} before frame {}", FRAME.load(std::sync::atomic::Ordering::Relaxed));
            }
            self.black[index] = [last, black];
        }
    }

    /// A for twenty frames out of forty, Start in place of every tenth.
    pub fn buttons(frame: u64) -> PadState {
        FRAME.store(frame, std::sync::atomic::Ordering::Relaxed);
        match (frame / 20 % 2, frame / 20 % 20) {
            (0, 10) => PadState::START,
            (0, _) => PadState::A,
            _ => PadState::empty(),
        }
    }

    /// the frames to keep the screens of, eight through the run.
    pub fn shot_frames(&self) -> HashSet<u64> {
        let every = (self.frames / 8).max(1);
        (1..=8).map(|n| n * every - 1).collect()
    }

    pub fn shoot(&mut self, system: &mut System, frame: u64) {
        let top = system.read_screen_scaled(zakuro_common::Screen::Top);
        let bottom = system.read_screen_scaled(zakuro_common::Screen::Bottom);
        // the two screens stacked, the bottom one centred
        let (width, height) = (top.1.max(bottom.1), top.2 + bottom.2);
        let row = (width as usize * 3).next_multiple_of(4);
        let size = (54 + row * height as usize) as u32;
        let mut bmp = Vec::with_capacity(size as usize);
        bmp.extend(b"BM".iter().chain(&size.to_le_bytes()).chain(&[0; 4]).chain(&54u32.to_le_bytes()));
        bmp.extend(40u32.to_le_bytes().iter().chain(&width.to_le_bytes()).chain(&height.to_le_bytes()).chain(&1u16.to_le_bytes()).chain(&24u16.to_le_bytes()).chain(&[0; 24]));
        for y in (0..height).rev() {
            let start = bmp.len();
            let ((pixels, screen_width, _), line) = if y < top.2 { (&top, y) } else { (&bottom, y - top.2) };
            let left = (width - screen_width) / 2;
            bmp.resize(start + left as usize * 3, 0);
            let at = (line * screen_width) as usize * 4;
            bmp.extend(pixels[at..at + *screen_width as usize * 4].chunks(4).flat_map(|pixel| [pixel[2], pixel[1], pixel[0]]));
            bmp.resize(start + row, 0);
        }
        let file = format!("frame-{frame:05}.bmp");
        let _ = std::fs::write(self.out.join(&file), bmp);
        self.shots.push(file);
    }

    pub fn report(self, system: &mut System, rom: &str, title_id: u64, has_profile: bool, ran: u64, stopped: Option<FrameOutcome>) -> i32 {
        let seconds = self.started.elapsed().as_secs_f64();
        let mut report = format!("# Probe of {title_id:016X}\n\n");
        let _ = writeln!(report, "- ROM: {rom}");
        let library = std::env::var("ZAKURO_RECOMPILED").unwrap_or_else(|_| "none, the code was interpreted".into());
        let _ = writeln!(report, "- Recompiled library: {library}");
        let _ = writeln!(report, "- Profile: {}", if has_profile { "yes" } else { "none yet, titles/<title id>/profile.toml" });
        let _ = writeln!(report, "- Ran {ran} frames in {seconds:.0} s, {:.0}% of the console's speed", ran as f64 / 60.0 / seconds * 100.0);
        let _ = writeln!(report, "- Pictures drawn: {} top, {} bottom", system.top_pictures(), system.bottom_pictures());
        if let Some(speed) = self.speed {
            let _ = writeln!(report, "- At {:.0}% speed: {} of {ran} frames skipped", speed * 100.0, self.skipped);
        }
        let _ = writeln!(report, "- Black flashes: {} top, {} bottom", self.flashes[0], self.flashes[1]);
        // how busy the game keeps the console's CPU, which a lower CPU clock
        // only helps near the top of
        let instructions = (system.interpreted_instructions + system.recompiled_instructions) / ran.max(1);
        let in_place = system.recompiled.as_ref().map_or(0, |library| library.in_place()) / ran.max(1);
        let _ = writeln!(
            report,
            "- CPU: {instructions} instructions a frame, {:.0}% of the console's, {in_place} svcs a frame run in place",
            instructions as f64 / zakuro_core::CYCLES_PER_FRAME as f64 * 100.0
        );
        let _ = writeln!(report, "- {}", stopped.map_or("Still running at the end".to_owned(), |outcome| format!("Stopped at frame {ran}: {outcome:?}")));
        report += "\n## Warnings and errors\n\nNumbers are written as #. Count, the frame it first came at, and the message.\n\n";
        let seen = RECORDER.get().map(|recorder| recorder.seen.lock().unwrap().clone()).unwrap_or_default();
        let mut seen: Vec<_> = seen.into_iter().collect();
        seen.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
        if seen.is_empty() {
            report += "None.\n";
        }
        for (message, (count, first)) in seen {
            let _ = writeln!(report, "- {count} × (from frame {first}) {message}");
        }
        // where each thread is and what it waits for, for a game that hangs
        report += "\n## Threads at the end\n\n";
        let kernel = &system.kernel;
        for thread in kernel.threads.iter().filter(|thread| thread.status != ThreadStatus::Dead) {
            let pc = if kernel.current_thread == Some(thread.id) { system.cpu.current_pc() } else { thread.context.regs[15] };
            let state = if thread.status.is_blocked() { kernel.describe_wait(thread.id) } else { format!("{:?}", thread.status) };
            let _ = writeln!(report, "- {} priority {}, pc 0x{pc:08X} lr 0x{:08X}: {state}", thread.name, thread.priority, thread.context.regs[14]);
        }
        report += "\n## Screens\n\n";
        for file in &self.shots {
            let _ = writeln!(report, "- {file}");
        }
        let path = self.out.join("report.md");
        match std::fs::write(&path, &report) {
            Ok(()) => println!("{}", path.display()),
            Err(error) => eprintln!("could not write {}: {error}", path.display()),
        }
        if matches!(stopped, Some(FrameOutcome::Faulted)) { 1 } else { 0 }
    }
}
