//! pocket-native --probe <rom> [frames]: runs any game as fast as it goes,
//! without a gamepad, pressing A now and then and Start once in a while, and
//! writes what happened into probe-<title id>/: how far it got and how fast,
//! the warnings and errors it met, which are what the engine still lacks for
//! it, and both screens every so often.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use zakuro_core::kernel::thread::ThreadStatus;
use zakuro_core::services::hid::PadState;
use zakuro_core::{FrameOutcome, System};

/// Precise, versioned probe data for external performance comparisons. The
/// human report deliberately rounds times, which is unsuitable for an eval.
#[derive(serde::Serialize)]
struct Metrics<'a> {
    schema_version: u32,
    title_id: String,
    requested_frames: u64,
    attempted_frames: u64,
    completed_frames: u64,
    elapsed_seconds: f64,
    skipped_frames: u64,
    top_pictures: u64,
    bottom_pictures: u64,
    interpreted_instructions: u64,
    recompiled_instructions: u64,
    stop_reason: String,
    screenshots: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_trace_file: Option<&'static str>,
}

/// Optional diagnostics sample guest state without waiting for host workers.
/// Serializing after the run avoids disk writes perturbing each checkpoint.
#[derive(serde::Serialize)]
struct GuestCheckpoint {
    completed_frames: u64,
    guest_cycles: u64,
    registers: [u32; 16],
    #[serde(skip_serializing_if = "Option::is_none")]
    current_thread: Option<u32>,
    interpreted_instructions: u64,
    recompiled_instructions: u64,
}

#[derive(serde::Serialize)]
struct GuestTrace<'a> {
    schema_version: u32,
    sampling_interval_frames: u64,
    dropped_checkpoints: u64,
    checkpoints: &'a [GuestCheckpoint],
}

const MAX_GUEST_CHECKPOINTS: usize = 100_000;

/// Optional performance observer. Buffered until exit; never queries/waits for
/// GPU workers. A batch is one run_frame call, not a unique presented picture.
struct BatchTiming {
    started: Option<(Instant, Option<u64>)>,
    batch_started: Option<(Instant, Option<u64>)>,
    wait_last: u64,
    wait_us: Option<u64>,
    rows: Vec<TimingRow>,
    dropped: u64,
}

struct TimingRow {
    frame: u64,
    wall_ns: u64,
    cpu_ns: Option<u64>,
    batch_wall_ns: Option<u64>,
    batch_cpu_ns: Option<u64>,
    cycles: u64,
    interpreted: u64,
    recompiled: u64,
    gpu_wait_us: Option<u64>,
}

fn process_cpu_ns() -> Option<u64> {
    // SAFETY: getrusage initializes the supplied writable rusage on success.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let micros = |t: libc::timeval| (t.tv_sec as u64) * 1_000_000 + t.tv_usec as u64;
    Some((micros(usage.ru_utime) + micros(usage.ru_stime)) * 1_000)
}

impl BatchTiming {
    fn begin_batch(&mut self) {
        self.batch_started = Some((Instant::now(), process_cpu_ns()));
        self.wait_last = zakuro_core::GPU_WAITED.load(std::sync::atomic::Ordering::Relaxed);
        self.wait_us = Some(0);
    }

    /// Busy::report resets this shared diagnostic counter. Capture its last
    /// segment immediately before that call, then start from zero. No wait or
    /// reset is introduced by this observer itself.
    fn before_wait_reset(&mut self) {
        let now = zakuro_core::GPU_WAITED.load(std::sync::atomic::Ordering::Relaxed);
        self.wait_us = self.wait_us.and_then(|sum| now.checked_sub(self.wait_last)?.checked_add(sum));
        self.wait_last = 0;
    }

    fn begin(&mut self) {
        self.started = Some((Instant::now(), process_cpu_ns()));
    }

    fn finish(&mut self, system: &System, frame: u64) {
        let Some((at, cpu)) = self.started.take() else { return };
        let wall_ns = at.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let cpu_ns = cpu.and_then(|before| process_cpu_ns()?.checked_sub(before));
        if self.rows.len() == MAX_GUEST_CHECKPOINTS {
            self.dropped += 1;
            return;
        }
        self.rows.push(TimingRow { frame, wall_ns, cpu_ns, batch_wall_ns: None, batch_cpu_ns: None, cycles: system.cpu.cycles,
            interpreted: system.interpreted_instructions, recompiled: system.recompiled_instructions, gpu_wait_us: None });
    }

    fn finish_batch(&mut self) {
        let Some((at, cpu)) = self.batch_started.take() else { return };
        if let Some(row) = self.rows.last_mut() {
            row.batch_wall_ns = Some(at.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            row.batch_cpu_ns = cpu.and_then(|before| process_cpu_ns()?.checked_sub(before));
            let now = zakuro_core::GPU_WAITED.load(std::sync::atomic::Ordering::Relaxed);
            row.gpu_wait_us = self.wait_us.and_then(|sum| now.checked_sub(self.wait_last)?.checked_add(sum));
        }
    }

    fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "frame,wall_ns,process_cpu_ns,batch_wall_ns,batch_process_cpu_ns,guest_cycles,interpreted_instructions,recompiled_instructions,gpu_wait_delta_us")?;
        for row in &self.rows {
            writeln!(out, "{},{},{},{},{},{},{},{},{}", row.frame, row.wall_ns,
                row.cpu_ns.map(|n| n.to_string()).unwrap_or_default(),
                row.batch_wall_ns.map(|n| n.to_string()).unwrap_or_default(),
                row.batch_cpu_ns.map(|n| n.to_string()).unwrap_or_default(),
                row.cycles, row.interpreted, row.recompiled,
                row.gpu_wait_us.map(|n| n.to_string()).unwrap_or_default())?;
        }
        out.flush()?;
        if self.dropped != 0 || self.rows.iter().any(|row| row.cpu_ns.is_none() || row.batch_cpu_ns.is_none() || row.batch_wall_ns.is_none() || row.gpu_wait_us.is_none()) {
            return Err(std::io::Error::other("incomplete batch telemetry"));
        }
        Ok(())
    }
}

fn completion(requested: u64, attempted: u64, stopped: Option<FrameOutcome>) -> (u64, String, bool) {
    let completed = attempted.saturating_sub(u64::from(stopped.is_some()));
    let complete = stopped.is_none() && requested > 0 && completed == requested;
    let reason = match stopped {
        Some(outcome) => format!("{outcome:?}"),
        None if complete => "frame_limit".into(),
        None => "interrupted".into(),
    };
    (completed, reason, complete)
}

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
    trace_every: Option<u64>,
    checkpoints: Vec<GuestCheckpoint>,
    dropped_checkpoints: u64,
    timing: Option<BatchTiming>,
    boundary_captures: bool,
}

impl Probe {
    pub fn new(title_id: u64, frames: u64) -> Probe {
        let out = PathBuf::from(format!("probe-{title_id:016X}"));
        let _ = std::fs::create_dir_all(&out);
        let speed = std::env::var("ZAKURO_PROBE_SPEED").ok().and_then(|speed| speed.parse().ok()).filter(|&speed: &f64| speed > 0.0);
        let trace_every = std::env::var("ZAKURO_PROBE_TRACE_EVERY").ok().and_then(|value| value.parse().ok()).filter(|&value| value > 0);
        let timing = (std::env::var("ZAKURO_PROBE_TELEMETRY").as_deref() == Ok("1"))
            .then(|| BatchTiming { started: None, batch_started: None, wait_last: 0, wait_us: Some(0), rows: Vec::with_capacity(frames.min(MAX_GUEST_CHECKPOINTS as u64) as usize), dropped: 0 });
        let boundary_captures = std::env::var("ZAKURO_PROBE_CAPTURE").as_deref() == Ok("boundaries");
        Probe { frames, out, started: Instant::now(), shots: Vec::new(), speed, late: 0.0, skipped: 0, black: [[false; 2]; 2], flashes: [0; 2], trace_every, checkpoints: Vec::new(), dropped_checkpoints: 0, timing, boundary_captures }
    }

    pub fn begin_frame(&mut self) {
        if let Some(timing) = self.timing.as_mut() { timing.begin(); }
    }

    pub fn begin_batch(&mut self) {
        if let Some(timing) = self.timing.as_mut() { timing.begin_batch(); }
    }

    pub fn finish_batch(&mut self) {
        if let Some(timing) = self.timing.as_mut() { timing.finish_batch(); }
    }

    pub fn before_wait_reset(&mut self) {
        if let Some(timing) = self.timing.as_mut() { timing.before_wait_reset(); }
    }

    /// Called immediately after a completed guest frame, before frontend
    /// applet responses. In particular, no picture-count getter is used here:
    /// those wait for the GPU and can change interrupt timing while diagnosing it.
    pub fn checkpoint(&mut self, system: &System, completed_frames: u64) {
        if let Some(timing) = self.timing.as_mut() { timing.finish(system, completed_frames); }
        let Some(every) = self.trace_every else { return };
        if !completed_frames.is_multiple_of(every) {
            return;
        }
        if self.checkpoints.len() == MAX_GUEST_CHECKPOINTS {
            self.dropped_checkpoints += 1;
            return;
        }
        self.checkpoints.push(GuestCheckpoint {
            completed_frames,
            guest_cycles: system.cpu.cycles,
            registers: system.cpu.regs,
            current_thread: system.kernel.current_thread,
            interpreted_instructions: system.interpreted_instructions,
            recompiled_instructions: system.recompiled_instructions,
        });
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
        if self.boundary_captures { return; }
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
        let (completed, stop_reason, complete) = completion(self.frames, ran, stopped);
        let timing_written = self.timing.as_ref().is_none_or(|timing| {
            timing.rows.len() as u64 == completed && timing.write(&self.out.join("batch-timing.csv")).is_ok()
        });
        let trace_written = self.trace_every.is_none_or(|sampling_interval_frames| {
            let trace = GuestTrace { schema_version: 1, sampling_interval_frames, dropped_checkpoints: self.dropped_checkpoints, checkpoints: &self.checkpoints };
            let path = self.out.join("guest-trace.toml");
            match toml::to_string(&trace).map_err(|error| error.to_string()).and_then(|text| std::fs::write(&path, text).map_err(|error| error.to_string())) {
                Ok(()) => true,
                Err(error) => { eprintln!("could not write {}: {error}", path.display()); false }
            }
        });
        let metrics = Metrics {
            schema_version: 1,
            title_id: format!("{title_id:016X}"),
            requested_frames: self.frames,
            attempted_frames: ran,
            completed_frames: completed,
            elapsed_seconds: seconds,
            skipped_frames: self.skipped,
            top_pictures: system.top_pictures(),
            bottom_pictures: system.bottom_pictures(),
            interpreted_instructions: system.interpreted_instructions,
            recompiled_instructions: system.recompiled_instructions,
            stop_reason,
            screenshots: &self.shots,
            guest_trace_file: self.trace_every.map(|_| "guest-trace.toml"),
        };
        let metrics_path = self.out.join("report.toml");
        let metrics_written = match toml::to_string(&metrics) {
            Ok(text) => match std::fs::write(&metrics_path, text) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("could not write {}: {error}", metrics_path.display());
                    false
                }
            },
            Err(error) => {
                eprintln!("could not serialize probe metrics: {error}");
                false
            }
        };
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
        let _ = writeln!(report, "- CPU: {instructions} instructions a frame, {:.0}% of the console's", instructions as f64 / zakuro_core::CYCLES_PER_FRAME as f64 * 100.0);
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
        let report_written = match std::fs::write(&path, &report) {
            Ok(()) => { println!("{}", path.display()); true }
            Err(error) => { eprintln!("could not write {}: {error}", path.display()); false }
        };
        if complete && self.skipped == 0 && metrics_written && report_written && trace_written && timing_written { 0 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_deltas_survive_the_status_report_reset_and_disabled_logging() {
        use std::sync::atomic::Ordering;
        // Bind the observer to the real status logger's reset contract. A
        // change there needs a matching observer update, not silently bad data.
        assert!(include_str!("busy.rs").contains("GPU_WAITED.swap(0, Ordering::Relaxed)"));
        struct Restore(u64);
        impl Drop for Restore {
            fn drop(&mut self) { zakuro_core::GPU_WAITED.store(self.0, Ordering::Relaxed); }
        }
        let _restore = Restore(zakuro_core::GPU_WAITED.load(Ordering::Relaxed));
        // This test creates no System/GL worker; no other frontend test writes
        // this diagnostic counter. Exercise both enabled and filtered logging.
        for reset in [false, true] {
            zakuro_core::GPU_WAITED.store(100, Ordering::Relaxed);
            let mut timing = BatchTiming { started: None, batch_started: None, wait_last: 0, wait_us: Some(0), rows: Vec::new(), dropped: 0 };
            timing.begin_batch();
            timing.rows.push(TimingRow { frame: 1, wall_ns: 1, cpu_ns: Some(0), batch_wall_ns: None, batch_cpu_ns: None,
                cycles: 0, interpreted: 0, recompiled: 1, gpu_wait_us: None });
            zakuro_core::GPU_WAITED.fetch_add(11, Ordering::Relaxed);
            if reset {
                timing.before_wait_reset();
                zakuro_core::GPU_WAITED.swap(0, Ordering::Relaxed);
            }
            zakuro_core::GPU_WAITED.fetch_add(7, Ordering::Relaxed);
            timing.finish_batch();
            assert_eq!(timing.rows[0].gpu_wait_us, Some(18));
        }
    }

    #[test]
    fn only_the_complete_workload_passes() {
        assert_eq!(completion(60, 60, None), (60, "frame_limit".into(), true));
        assert_eq!(completion(60, 20, None), (20, "interrupted".into(), false));
        assert_eq!(completion(60, 60, Some(FrameOutcome::Faulted)), (59, "Faulted".into(), false));
        assert_eq!(completion(60, 20, Some(FrameOutcome::Exited)), (19, "Exited".into(), false));
        assert!(!completion(0, 0, None).2);
    }

    #[test]
    fn guest_trace_preserves_exact_state_and_an_idle_thread() {
        let checkpoints = [GuestCheckpoint {
            completed_frames: 10,
            guest_cycles: 44_685_309,
            registers: std::array::from_fn(|index| 0x1234_0000 + index as u32),
            current_thread: None,
            interpreted_instructions: 100,
            recompiled_instructions: 123_456,
        }];
        let trace = GuestTrace { schema_version: 1, sampling_interval_frames: 10, dropped_checkpoints: 0, checkpoints: &checkpoints };
        let serialized = toml::to_string(&trace).unwrap();
        let value: toml::Value = toml::from_str(&serialized).unwrap();
        let checkpoint = &value["checkpoints"][0];
        assert_eq!(checkpoint["guest_cycles"].as_integer(), Some(44_685_309));
        assert_eq!(checkpoint["registers"].as_array().unwrap().len(), 16);
        assert_eq!(checkpoint["registers"][15].as_integer(), Some(0x1234_000f));
        assert!(checkpoint.get("current_thread").is_none());
        assert_eq!(checkpoint["recompiled_instructions"].as_integer(), Some(123_456));
    }
}
