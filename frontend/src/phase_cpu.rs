//! Ordinary-main-thread diagnostic snapshots, independent of PNPROF and SIGPROF.
//! Task identity is (numeric TID, stat starttime); enumeration is not atomic.
use std::fmt::{self, Write as _};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const TASK_CAP: usize = 256;
const ENTRY_CAP: usize = 512;
const STAT_CAP: u64 = 4096;
const RECORD_CAP: usize = 64 * 1024;
const OUTPUT_CAP: usize = 512 * 1024;
const SNAPSHOT_CAP: usize = 7;
const FRAMES: [u64; 3] = [1500, 4500, 6000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cpu { start: u64, user: u64, system: u64 }

// Field 2 may contain spaces and ')' characters. Indices after its final ')'
// are state=3, utime=14, stime=15, starttime=22; all counts stay in kernel ticks.
fn parse_stat(bytes: &[u8], expected_tid: u32) -> Option<Cpu> {
    let text = std::str::from_utf8(bytes).ok()?;
    let open = text.find('(')?;
    if text[..open].trim().parse::<u32>().ok()? != expected_tid { return None; }
    let close = text.rfind(')')?;
    if close < open { return None; }
    let mut fields = text[close + 1..].split_ascii_whitespace();
    let user = fields.nth(11)?.parse().ok()?;
    let system = fields.next()?.parse().ok()?;
    let start = fields.nth(6)?.parse().ok()?;
    Some(Cpu { start, user, system })
}

struct Task { tid: u32, cpu: Option<Cpu>, status: &'static str, errno: Option<i32> }
struct Scan { tasks: Vec<Task>, directory_complete: bool, errors: usize, cap: &'static str }

fn read_task(tid: u32) -> Task {
    let read = || -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(format!("/proc/self/task/{tid}/stat"))?
            .take(STAT_CAP + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let (cpu, status, errno) = match read() {
        Err(error) => (None, "unreadable", error.raw_os_error()),
        Ok(bytes) if bytes.len() > STAT_CAP as usize => (None, "stat_cap", None),
        Ok(bytes) => match parse_stat(&bytes, tid) {
            Some(cpu) => (Some(cpu), "readable", None),
            None => (None, "parse_unknown", None),
        },
    };
    Task { tid, cpu, status, errno }
}

fn scan_tasks() -> Scan {
    let mut scan = Scan { tasks: Vec::new(), directory_complete: false, errors: 0, cap: "none" };
    let entries = match std::fs::read_dir("/proc/self/task") {
        Ok(entries) => entries,
        Err(_) => { scan.errors = 1; return scan; }
    };
    for (index, entry) in entries.enumerate() {
        if index == ENTRY_CAP { scan.cap = "directory_entries"; return scan; }
        let entry = match entry { Ok(entry) => entry, Err(_) => { scan.errors += 1; continue; } };
        let name = entry.file_name();
        let Some(tid) = name.to_str().and_then(|name| name.parse::<u32>().ok()).filter(|tid| *tid > 0) else { continue; };
        if scan.tasks.len() == TASK_CAP { scan.cap = "tasks"; return scan; }
        scan.tasks.push(read_task(tid));
    }
    // Complete directory iteration is still a non-atomic view of live tasks.
    scan.directory_complete = scan.errors == 0;
    scan
}

fn clock_ns(id: libc::clockid_t) -> Option<u64> {
    let mut value: libc::timespec = unsafe { std::mem::zeroed() };
    // Ordinary code only. Never called from sampled(), or any signal handler.
    if unsafe { libc::clock_gettime(id, &mut value) } != 0 { return None; }
    let nanos = u64::try_from(value.tv_nsec).ok().filter(|nanos| *nanos < 1_000_000_000)?;
    u64::try_from(value.tv_sec).ok()?.checked_mul(1_000_000_000)?.checked_add(nanos)
}

struct Number(Option<u64>);
impl fmt::Display for Number {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 { Some(value) => write!(out, "{value}"), None => out.write_str("-") }
    }
}

fn destination(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(path.file_name().ok_or_else(|| invalid("missing filename"))?))
}
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }
fn create_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}
fn relation(previous: &[(u32, u64)], tid: u32, start: u64) -> &'static str {
    match previous.iter().find(|(old_tid, _)| *old_tid == tid) {
        Some((_, old_start)) if *old_start == start => "observed_again",
        Some(_) => "tid_reused",
        None => "first_observed",
    }
}

struct State {
    file: File,
    bytes: usize,
    snapshots: usize,
    frames_seen: u8,
    completed: u64,
    previous: Vec<(u32, u64)>,
}
impl State {
    fn append(&mut self, text: &str) -> io::Result<()> {
        if text.len() > RECORD_CAP || self.bytes.checked_add(text.len()).filter(|size| *size <= OUTPUT_CAP).is_none() {
            return Err(invalid("CPU sidecar byte cap"));
        }
        self.file.write_all(text.as_bytes())?;
        self.bytes += text.len();
        Ok(())
    }

    fn snapshot(&mut self, phase: &str, attempted: u64) -> io::Result<()> {
        if self.snapshots == SNAPSHOT_CAP { return Err(invalid("CPU snapshot cap")); }
        let mono_before = clock_ns(libc::CLOCK_MONOTONIC);
        let process_before = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
        let thread_before = clock_ns(libc::CLOCK_THREAD_CPUTIME_ID);
        // SAFETY: ordinary call returning this caller's numeric kernel TID.
        let caller_tid = u32::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).ok();
        let mut scan = scan_tasks();
        let thread_after = clock_ns(libc::CLOCK_THREAD_CPUTIME_ID);
        let process_after = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
        let mono_after = clock_ns(libc::CLOCK_MONOTONIC);
        scan.tasks.sort_unstable_by_key(|task| task.tid);
        let duplicate_tids = scan.tasks.windows(2).filter(|pair| pair[0].tid == pair[1].tid).count();
        let mut out = String::new();
        writeln!(out, "snapshot\t{}\tphase={phase}\tattempted={attempted}\tcompleted={}\tcaller_tid={}\tmono_before_ns={}\tmono_after_ns={}\tprocess_before_ns={}\tprocess_after_ns={}\tthread_before_ns={}\tthread_after_ns={}\tdirectory_complete={}\tdirectory_errors={}\tcap={}\tduplicate_tids={duplicate_tids}",
            self.snapshots, self.completed, Number(caller_tid.map(u64::from)), Number(mono_before), Number(mono_after),
            Number(process_before), Number(process_after), Number(thread_before), Number(thread_after),
            u8::from(scan.directory_complete), scan.errors, scan.cap).unwrap();
        let mut current = Vec::new();
        for task in &scan.tasks {
            if let Some(cpu) = task.cpu {
                writeln!(out, "task\t{}\tstart_ticks={}\tuser_ticks={}\tsystem_ticks={}\trelation={}",
                    task.tid, cpu.start, cpu.user, cpu.system, relation(&self.previous, task.tid, cpu.start)).unwrap();
                current.push((task.tid, cpu.start));
            } else {
                writeln!(out, "unknown_task\t{}\tstatus={}\terrno={}", task.tid, task.status, Number(task.errno.and_then(|code| u64::try_from(code).ok()))).unwrap();
            }
        }
        for &(tid, start) in &self.previous {
            if !current.contains(&(tid, start)) {
                // Could be exit, unreadability, cap omission or a TID reuse.
                // Never label this an exited task or fill its final CPU with 0.
                writeln!(out, "not_observed\t{tid}\tstart_ticks={start}").unwrap();
            }
        }
        writeln!(out, "snapshot_end\t{}\tobserved={}\treadable={}", self.snapshots, scan.tasks.len(), current.len()).unwrap();
        self.append(&out)?;
        self.previous = current;
        self.snapshots += 1;
        Ok(())
    }
}

/// Not shared or installed in a static; the frontend main thread owns it.
#[derive(Default)]
pub(super) struct Recorder { state: Option<State> }
impl Recorder {
    pub(super) fn from_env() -> Self {
        let Some(path) = std::env::var_os("ZAKURO_PHASE_CPU_OUT") else { return Self::default(); };
        let create = || -> io::Result<State> {
            let path = PathBuf::from(path);
            if !path.is_absolute() || path.as_os_str().len() > 4096 { return Err(invalid("CPU output must be an absolute bounded path")); }
            let id = std::env::var("ZAKURO_PHASE_CPU_RUN_ID").map_err(|_| invalid("missing CPU run ID"))?;
            if !valid_id(&id) { return Err(invalid("invalid CPU run ID")); }
            let path = destination(&path)?;
            if path == destination(&std::env::temp_dir().join("zakuro-host-profile.txt"))? { return Err(invalid("CPU output aliases legacy profile")); }
            if let Some(other) = std::env::var_os("ZAKURO_HOST_PROFILE_SIDECAR") {
                if path == destination(Path::new(&other))? { return Err(invalid("CPU output aliases PNPROF")); }
            }
            let file = create_file(&path)?;
            let mut state = State { file, bytes: 0, snapshots: 0, frames_seen: 0, completed: 0, previous: Vec::new() };
            // SAFETY: read-only ordinary libc query. Failure stays unknown.
            let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|hz| *hz != 0);
            state.append(&format!("PNCPU\t1\nheader\trun_id={id}\tpid={}\tclk_tck={}\ttask_cap={TASK_CAP}\tentry_cap={ENTRY_CAP}\tstat_cap={STAT_CAP}\trecord_cap={RECORD_CAP}\toutput_cap={OUTPUT_CAP}\tsnapshot_cap={SNAPSHOT_CAP}\tframe_plan=0,1500,4500,6000\tpackage={}\tsemantics=non_atomic_guest_batch_boundaries_not_presentation\n", std::process::id(), Number(hz), env!("CARGO_PKG_VERSION")))?;
            state.snapshot("frontend_start", 0)?;
            Ok(state)
        };
        match create() {
            Ok(state) => Self { state: Some(state) },
            Err(error) => { log::warn!("phase CPU sidecar disabled or incomplete: {error}"); Self::default() }
        }
    }

    fn capture(&mut self, phase: &str, attempted: u64) {
        let Some(state) = self.state.as_mut() else { return };
        if let Err(error) = state.snapshot(phase, attempted) {
            log::warn!("phase CPU sidecar incomplete; no retry: {error}");
            self.state = None;
        }
    }
    pub(super) fn before_batches(&mut self) { self.capture("before_batches", 0); }
    pub(super) fn completed(&mut self, frame: u64) {
        let Some(state) = self.state.as_mut() else { return };
        state.completed = frame;
        let Some(index) = FRAMES.iter().position(|selected| *selected == frame) else { return };
        if state.frames_seen & (1 << index) != 0 { return; }
        state.frames_seen |= 1 << index;
        self.capture("guest_batch_completed", frame);
    }
    pub(super) fn after_loop(&mut self, attempted: u64) { self.capture("after_loop", attempted); }
    pub(super) fn finish(&mut self, attempted: u64) {
        self.capture("after_gpu_stop_before_exit", attempted);
        let Some(mut state) = self.state.take() else { return };
        let finish = state.append(&format!("PNCPU_DONE\t1\tsnapshots={}\tframes_seen_mask={}\tcompleted={}\n", state.snapshots, state.frames_seen, state.completed))
            .and_then(|()| state.file.sync_all());
        if let Err(error) = finish { log::warn!("phase CPU sidecar finalization failed: {error}"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(comm: &str, user: &str, system: &str, start: &str) -> String {
        format!("42 ({comm}) S 0 0 0 0 0 0 0 0 0 0 {user} {system} 0 0 0 0 0 0 {start} 0\n")
    }
    #[test]
    fn stat_uses_numeric_tid_and_final_comm_parenthesis() {
        assert_eq!(parse_stat(stat("name ) with spaces", "17", "9", "301").as_bytes(), 42), Some(Cpu { start: 301, user: 17, system: 9 }));
        assert_eq!(parse_stat(stat("name", "17", "9", "301").as_bytes(), 43), None);
    }
    #[test]
    fn truncated_negative_and_overflow_counts_are_unknown() {
        assert_eq!(parse_stat(b"42 (name) S 1", 42), None);
        assert_eq!(parse_stat(stat("name", "-1", "9", "301").as_bytes(), 42), None);
        assert_eq!(parse_stat(stat("name", "18446744073709551616", "9", "301").as_bytes(), 42), None);
    }
    #[test]
    fn churn_never_joins_reused_numeric_tids() {
        assert_eq!(relation(&[(42, 301)], 42, 301), "observed_again");
        assert_eq!(relation(&[(42, 301)], 42, 302), "tid_reused");
        assert_eq!(relation(&[], 42, 301), "first_observed");
    }
    #[test]
    fn run_ids_cannot_inject_records() {
        assert!(valid_id("capture-03.1"));
        for id in ["", "line\nbreak", "a\tb", "../a", "é"] { assert!(!valid_id(id)); }
        assert!(!valid_id(&"x".repeat(65)));
    }
    #[test]
    fn exclusive_output_and_caps_preserve_existing_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("phase-cpu-source-fixture-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("new.cpu");
        let file = create_file(&path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        let mut state = State { file, bytes: 0, snapshots: 0, frames_seen: 0, completed: 0, previous: Vec::new() };
        state.append("retained\n").unwrap();
        assert!(create_file(&path).is_err());
        assert!(state.append(&"x".repeat(RECORD_CAP + 1)).is_err());
        state.bytes = OUTPUT_CAP;
        assert!(state.append("one more byte").is_err());
        drop(state);
        assert_eq!(std::fs::read(&path).unwrap(), b"retained\n");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn live_phase_snapshots_include_worker_identity_and_completed_boundaries() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("phase-cpu-live-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("live.cpu");
        let (started, identity) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            started.send(tid).unwrap();
            wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        });
        let tid = identity.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let mut state = State { file: create_file(&path).unwrap(), bytes: 0, snapshots: 0,
            frames_seen: 0, completed: 0, previous: Vec::new() };
        state.snapshot("frontend_start", 0).unwrap();
        assert!(state.previous.iter().any(|(observed, start)| *observed == tid && *start > 0));
        let mut recorder = Recorder { state: Some(state) };
        recorder.before_batches();
        recorder.completed(1);
        assert_eq!(recorder.state.as_ref().unwrap().snapshots, 2);
        for frame in FRAMES { recorder.completed(frame); }
        recorder.after_loop(6000);
        recorder.finish(6000);
        release.send(()).unwrap();
        worker.join().unwrap();
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.lines().filter(|line| line.starts_with("snapshot\t")).count(), 7);
        assert!(output.ends_with("PNCPU_DONE\t1\tsnapshots=7\tframes_seen_mask=7\tcompleted=6000\n"));
        assert!(output.contains(&format!("task\t{tid}\tstart_ticks=")));
        assert!(output.contains("mono_before_ns="));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
