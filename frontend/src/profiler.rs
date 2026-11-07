//! a sampling profiler: a CPU time timer interrupts whichever thread runs,
//! which notes its thread and program counter.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const SAMPLES: usize = 1 << 18;
static PCS: [AtomicU64; SAMPLES] = [const { AtomicU64::new(0) }; SAMPLES];
static LRS: [AtomicU64; SAMPLES] = [const { AtomicU64::new(0) }; SAMPLES];
static TIDS: [AtomicU32; SAMPLES] = [const { AtomicU32::new(0) }; SAMPLES];
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// the kernel's profiling timer, which the libc crate leaves out on Android.
const ITIMER_PROF: libc::c_int = 2;
/// whether the timer is going, so that a run ending before its last
/// profiled frame still stops it and writes what it has.
static RUNNING: AtomicBool = AtomicBool::new(false);

// Keep the legacy text row identical, including when the companion is enabled.
macro_rules! legacy_row {
    ($name:expr, $file:expr, $offset:expr, $caller:expr, $from:expr) => {
        format!("{} {} {:x} {} {:x}\n", $name, $file, $offset, $caller, $from)
    };
}

extern "C" fn sampled(_: i32, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the kernel hands the handler the interrupted context
    let (pc, lr) = unsafe {
        // on aarch64 the registers are read where the kernel puts them:
        // the libc crate's Android ucontext_t leaves out the padding that
        // makes bionic's signal mask 128 bytes, as glibc's is, so its
        // uc_mcontext is 128 bytes early and read garbage. the context
        // starts at 176, after flags, link, stack and that mask, rounded to
        // 16, then the fault address, x0 to x30, sp and pc
        #[cfg(target_arch = "aarch64")]
        let pair = {
            const MCONTEXT: usize = 176;
            let word = |at: usize| std::ptr::read_unaligned((context as *const u8).add(at) as *const u64);
            (word(MCONTEXT + 8 + 32 * 8), word(MCONTEXT + 8 + 30 * 8))
        };
        #[cfg(target_arch = "x86_64")]
        let pair = ((*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize] as u64, 0);
        pair
    };
    // SAFETY: gettid is async-signal-safe
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    #[cfg(not(feature = "profile-sidecar"))]
    let at = NEXT.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "profile-sidecar")]
    let at = sidecar::reserve(&NEXT, &sidecar::ATTEMPTS_SATURATED);
    if at < SAMPLES {
        PCS[at].store(pc, Ordering::Relaxed);
        LRS[at].store(lr, Ordering::Relaxed);
        TIDS[at].store(tid, Ordering::Relaxed);
        // Slots are never reused. A stopped timer is NOT a publication fence
        // for handlers on other threads; only this release makes a row ready.
        #[cfg(feature = "profile-sidecar")]
        sidecar::READY[at].store(true, Ordering::Release);
    }
}

pub fn start() {
    #[cfg(feature = "profile-sidecar")]
    sidecar::timer_start();
    // SAFETY: a plain handler and timer, the handler only stores atomics
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = sampled as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigaction(libc::SIGPROF, &action, std::ptr::null_mut());
        let interval = libc::timeval { tv_sec: 0, tv_usec: 1000 };
        let timer = libc::itimerval { it_interval: interval, it_value: interval };
        libc::setitimer(ITIMER_PROF, &timer, std::ptr::null_mut());
    }
    #[cfg(feature = "profile-sidecar")]
    sidecar::event(sidecar::START_RETURNED);
    RUNNING.store(true, Ordering::Relaxed);
    log::info!("profiling the host");
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// stops and writes thread, mapping and offset for each sample.
pub fn dump(path: &std::path::Path) {
    RUNNING.store(false, Ordering::Relaxed);
    // SAFETY: a zero timer stops it
    unsafe {
        let zero: libc::itimerval = std::mem::zeroed();
        libc::setitimer(ITIMER_PROF, &zero, std::ptr::null_mut());
    }
    #[cfg(feature = "profile-sidecar")]
    sidecar::event(sidecar::STOP_RETURNED);
    let maps: Vec<(u64, u64, u64, String)> = std::fs::read_to_string("/proc/self/maps")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (range, _, offset) = (fields.next()?, fields.next()?, fields.next()?);
            let path = fields.nth(2).unwrap_or("").to_owned();
            let (start, end) = range.split_once('-')?;
            Some((u64::from_str_radix(start, 16).ok()?, u64::from_str_radix(end, 16).ok()?, u64::from_str_radix(offset, 16).ok()?, path))
        })
        .collect();
    let mut names = std::collections::HashMap::new();
    let count = NEXT.load(Ordering::Relaxed).min(SAMPLES);
    let mut out = String::new();
    #[cfg(feature = "profile-sidecar")]
    let mut written = 0usize;
    for at in 0..count {
        #[cfg(feature = "profile-sidecar")]
        if !sidecar::READY[at].load(Ordering::Acquire) { continue; }
        let (pc, lr, tid) = (PCS[at].load(Ordering::Relaxed), LRS[at].load(Ordering::Relaxed), TIDS[at].load(Ordering::Relaxed));
        let name = names
            .entry(tid)
            .or_insert_with(|| std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")).unwrap_or_default().trim().to_owned())
            .clone();
        let place = |at: u64| {
            maps.iter()
                .find(|(start, end, ..)| (*start..*end).contains(&at))
                .map_or(("?".to_owned(), at), |(start, _, offset, path)| (path.clone(), at - start + offset))
        };
        let ((file, offset), (caller, from)) = (place(pc), place(lr));
        out += &legacy_row!(name, file, offset, caller, from);
        #[cfg(feature = "profile-sidecar")]
        { written += 1; }
    }
    let _ = std::fs::write(path, out);
    #[cfg(not(feature = "profile-sidecar"))]
    log::info!("wrote {count} host samples to {}", path.display());
    #[cfg(feature = "profile-sidecar")]
    log::info!("wrote {written} complete host samples of {count} reserved slots to {}", path.display());
    #[cfg(feature = "profile-sidecar")]
    sidecar::dump(path);
}

#[cfg(feature = "profile-sidecar")]
pub use sidecar::{configure as configure_sidecar, frame_marker, FRAME_COMPLETED, PROFILE_BOUNDARY, RUN_END};

/// All diagnostic storage and calls disappear without the feature. Rust's
/// supported Atomic* types are lock-free; no clock, allocation, lock, logging,
/// or I/O is used by reserve() or the signal handler's READY publication.
#[cfg(feature = "profile-sidecar")]
mod sidecar {
    use super::*;
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    const MARKERS: usize = 16_384;
    const RECEIPT_CAP: u64 = 4096;
    const HEADER_CAP: usize = 16 * 1024;
    const OUTPUT_CAP: usize = 16 * 1024 * 1024;
    const MAGIC: &[u8; 8] = b"PNPROF\0\x01";
    const COMPLETE: &[u8; 8] = b"PNDONE\0\x01";
    pub const PROFILE_BOUNDARY: u64 = 1;
    const START_CALL: u64 = 2;
    pub(super) const START_RETURNED: u64 = 3;
    pub const FRAME_COMPLETED: u64 = 4;
    pub const RUN_END: u64 = 5;
    pub(super) const STOP_RETURNED: u64 = 6;
    pub(super) static READY: [AtomicBool; SAMPLES] = [const { AtomicBool::new(false) }; SAMPLES];
    pub(super) static ATTEMPTS_SATURATED: AtomicBool = AtomicBool::new(false);
    static MARKER_NEXT: AtomicUsize = AtomicUsize::new(0);
    static MARKERS_SATURATED: AtomicBool = AtomicBool::new(false);
    static MARKER_READY: [AtomicBool; MARKERS] = [const { AtomicBool::new(false) }; MARKERS];
    // frame, next_sample_index, next_after, monotonic_before_ns,
    // monotonic_after_ns, kind | flags<<32. Only the frontend writes markers.
    static MARKER_DATA: [[AtomicU64; 6]; MARKERS] = [const { [const { AtomicU64::new(0) }; 6] }; MARKERS];
    static LAST_FRAME: AtomicU64 = AtomicU64::new(0);
    static STARTS: AtomicUsize = AtomicUsize::new(0);
    static STARTS_SATURATED: AtomicBool = AtomicBool::new(false);
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    static PUBLISHED: AtomicBool = AtomicBool::new(false);

    // This is a root-authored receipt, not runtime discovery of source identity.
    // The exact receipt bytes are embedded; root must bind them to source,
    // toolchain, binary and mapped images in its external capture receipt.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        schema_version: u32,
        feature: String,
        source_revision: String,
        profiler_source_sha256: String,
        profiler_generation: String,
    }

    struct Config {
        path: PathBuf,
        receipt_path: String,
        receipt: String,
        from: u64,
        end: u64,
    }

    fn window(from: u64, length: u64) -> Option<std::ops::Range<u64>> {
        (length != 0).then_some(from..from.checked_add(length)?)
    }

    fn hex(value: &str, length: usize) -> bool {
        value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    pub fn configure(spec: Option<(u64, u64)>) {
        CONFIG.get_or_init(|| {
            let path = PathBuf::from(std::env::var_os("ZAKURO_HOST_PROFILE_SIDECAR")?);
            let load = || -> io::Result<Config> {
                let (from, length) = spec.ok_or_else(|| invalid("missing host profile window"))?;
                let window = window(from, length).ok_or_else(|| invalid("empty or overflowing host profile window"))?;
                let receipt_path = std::env::var("ZAKURO_HOST_PROFILE_SIDECAR_RECEIPT")
                    .map_err(|_| invalid("missing root receipt path"))?;
                if receipt_path.len() > RECEIPT_CAP as usize { return Err(invalid("root receipt path too long")); }
                let mut receipt = String::new();
                std::fs::File::open(&receipt_path)?.take(RECEIPT_CAP + 1).read_to_string(&mut receipt)?;
                if receipt.len() > RECEIPT_CAP as usize { return Err(invalid("root receipt too long")); }
                let parsed: Receipt = toml::from_str(&receipt).map_err(|_| invalid("invalid root receipt TOML"))?;
                if parsed.schema_version != 1 || parsed.feature != "profile-sidecar"
                    || !hex(&parsed.source_revision, 40) || !hex(&parsed.profiler_source_sha256, 64)
                    || parsed.profiler_generation.is_empty() || parsed.profiler_generation.len() > 128 {
                    return Err(invalid("invalid root source/feature/generation receipt"));
                }
                Ok(Config { path, receipt_path, receipt, from: window.start, end: window.end })
            };
            match load() {
                Ok(config) => Some(config),
                Err(error) => { log::warn!("host profile sidecar disabled: {error}"); None }
            }
        });
    }

    fn config() -> Option<&'static Config> { CONFIG.get().and_then(Option::as_ref) }

    /// Saturates rather than wrapping into a live slot. The flag means the
    /// attempted count is a lower bound. CAS retries are lock-free scalar ops.
    pub(super) fn reserve(next: &AtomicUsize, saturated: &AtomicBool) -> usize {
        match next.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |at| at.checked_add(1)) {
            Ok(at) => at,
            Err(_) => { saturated.store(true, Ordering::SeqCst); usize::MAX }
        }
    }

    // Only called outside the handler. Zero + flag records a clock failure.
    fn monotonic_ns() -> Option<u64> {
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 { return None; }
        u64::try_from(time.tv_sec).ok()?.checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(time.tv_nsec).ok()?)
    }

    pub fn frame_marker(frame: u64, kind: u64) {
        LAST_FRAME.store(frame, Ordering::Relaxed);
        let Some(config) = config() else { return };
        // End is included as an endpoint marker, never an included frame.
        if frame < config.from || frame > config.end { return; }
        mark(frame, kind);
    }

    pub(super) fn event(kind: u64) {
        if config().is_some() { mark(LAST_FRAME.load(Ordering::Relaxed), kind); }
    }

    pub(super) fn timer_start() {
        reserve(&STARTS, &STARTS_SATURATED);
        event(START_CALL);
    }

    fn marker_flags(before: Option<u64>, after: Option<u64>, next: u64, next_after: u64, saturated: bool) -> u64 {
        u64::from(before.is_none() || after.is_none())
            | (u64::from(next != next_after) << 1)
            | (u64::from(saturated) << 2)
            | (u64::from(matches!((before, after), (Some(a), Some(b)) if b < a)) << 3)
    }

    fn mark(frame: u64, kind: u64) {
        let at = reserve(&MARKER_NEXT, &MARKERS_SATURATED);
        if at >= MARKERS { return; }
        let before = monotonic_ns();
        let next = NEXT.load(Ordering::SeqCst) as u64;
        let next_after = NEXT.load(Ordering::SeqCst) as u64;
        let after = monotonic_ns();
        let flags = marker_flags(before, after, next, next_after, ATTEMPTS_SATURATED.load(Ordering::SeqCst));
        let values = [frame, next, next_after, before.unwrap_or(0), after.unwrap_or(0), kind | (flags << 32)];
        for (slot, value) in MARKER_DATA[at].iter().zip(values) { slot.store(value, Ordering::Relaxed); }
        MARKER_READY[at].store(true, Ordering::Release);
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Sample { index: u32, ready: bool, tid: u32, pc: u64, lr: u64 }

    fn sample(index: usize, ready: &AtomicBool, pc: &AtomicU64, lr: &AtomicU64, tid: &AtomicU32) -> Sample {
        // An unready row carries no partially published numeric attribution.
        if !ready.load(Ordering::Acquire) {
            return Sample { index: index as u32, ready: false, tid: 0, pc: 0, lr: 0 };
        }
        Sample { index: index as u32, ready: true, tid: tid.load(Ordering::Relaxed),
            pc: pc.load(Ordering::Relaxed), lr: lr.load(Ordering::Relaxed) }
    }

    #[derive(serde::Serialize)]
    struct Header<'a> {
        format_version: u32,
        feature: &'static str,
        package_version: &'static str,
        root_receipt_path: &'a str,
        root_receipt_toml: &'a str,
        requested_start: String,
        requested_length: String,
        requested_end_exclusive: String,
        sample_capacity: usize,
        marker_capacity: usize,
        sample_records: usize,
        marker_records: usize,
        // Unbounded counters are decimal strings: TOML integers are signed,
        // while a saturated usize may exceed i64::MAX on supported hosts.
        attempted_at_snapshot: String,
        stored_in_export: usize,
        incomplete_in_export: usize,
        overflow_at_snapshot: String,
        attempted_after_snapshot: String,
        attempted_saturated: bool,
        marker_attempted: String,
        marker_overflow: String,
        marker_incomplete_in_export: usize,
        ambiguous_markers_in_export: usize,
        marker_attempted_saturated: bool,
        timer_starts: String,
        timer_starts_saturated: bool,
        slot_lifetime: &'static str,
        unknown_tail: bool,
        sample_frame_membership: &'static str,
        sample_timestamps: &'static str,
        marker_order: &'static str,
        timer_syscall_status: &'static str,
    }

    struct Snapshot {
        attempted: usize,
        attempted_after: usize,
        saturated: bool,
        samples: Vec<Sample>,
        marker_attempted: usize,
        marker_saturated: bool,
        markers: Vec<(bool, [u64; 6])>,
    }

    fn snapshot() -> Snapshot {
        let attempted = NEXT.load(Ordering::SeqCst);
        let samples = (0..attempted.min(SAMPLES))
            .map(|at| sample(at, &READY[at], &PCS[at], &LRS[at], &TIDS[at])).collect();
        let marker_attempted = MARKER_NEXT.load(Ordering::SeqCst);
        let markers = (0..marker_attempted.min(MARKERS)).map(|at| {
            let ready = MARKER_READY[at].load(Ordering::Acquire);
            (ready, if ready { std::array::from_fn(|field| MARKER_DATA[at][field].load(Ordering::Relaxed)) } else { [0; 6] })
        }).collect();
        Snapshot { attempted, attempted_after: NEXT.load(Ordering::SeqCst),
            saturated: ATTEMPTS_SATURATED.load(Ordering::SeqCst), samples, marker_attempted,
            marker_saturated: MARKERS_SATURATED.load(Ordering::SeqCst), markers }
    }

    fn header<'a>(config: &'a Config, snapshot: &Snapshot) -> Header<'a> {
        let stored = snapshot.samples.iter().filter(|sample| sample.ready).count();
        Header {
            format_version: 1, feature: "profile-sidecar", package_version: env!("CARGO_PKG_VERSION"),
            root_receipt_path: &config.receipt_path, root_receipt_toml: &config.receipt,
            requested_start: config.from.to_string(), requested_length: (config.end - config.from).to_string(), requested_end_exclusive: config.end.to_string(),
            sample_capacity: SAMPLES, marker_capacity: MARKERS, sample_records: snapshot.samples.len(), marker_records: snapshot.markers.len(),
            attempted_at_snapshot: snapshot.attempted.to_string(), stored_in_export: stored, incomplete_in_export: snapshot.samples.len() - stored,
            overflow_at_snapshot: snapshot.attempted.saturating_sub(SAMPLES).to_string(), attempted_after_snapshot: snapshot.attempted_after.to_string(),
            attempted_saturated: snapshot.saturated, marker_attempted: snapshot.marker_attempted.to_string(),
            marker_overflow: snapshot.marker_attempted.saturating_sub(MARKERS).to_string(), marker_attempted_saturated: snapshot.marker_saturated,
            marker_incomplete_in_export: snapshot.markers.iter().filter(|(ready, _)| !*ready).count(),
            ambiguous_markers_in_export: snapshot.markers.iter().filter(|(ready, values)| *ready && values[5] >> 32 != 0).count(),
            timer_starts: STARTS.load(Ordering::SeqCst).to_string(), timer_starts_saturated: STARTS_SATURATED.load(Ordering::SeqCst),
            slot_lifetime: "process; never reset/reused; rearming retains prior rows; at most one publication attempt",
            unknown_tail: true,
            sample_frame_membership: "unknown; PC read precedes reservation; handler delay is unbounded",
            sample_timestamps: "unknown; marker clocks bound index observations only",
            marker_order: "file order; flag 2 means reservations changed between index loads; equal indices do not resolve PC/frame order",
            timer_syscall_status: "unobserved; call/return markers do not assert success or handler quiescence",
        }
    }

    fn encode(config: &Config, snapshot: &Snapshot) -> io::Result<Vec<u8>> {
        let text = toml::to_string(&header(config, snapshot)).map_err(io::Error::other)?;
        if text.len() > HEADER_CAP || snapshot.samples.len() > SAMPLES || snapshot.markers.len() > MARKERS {
            return Err(invalid("sidecar capacity exceeded"));
        }
        // 8-byte magic, u32 TOML byte count, TOML, fixed 32-byte sample rows,
        // fixed 56-byte marker rows, 8-byte completion footer. Readers MUST
        // check exact length and footer; failed writes can leave partial files.
        // All integers little endian, no padding.
        let size = 20 + text.len() + snapshot.samples.len() * 32 + snapshot.markers.len() * 56;
        if size > OUTPUT_CAP { return Err(invalid("sidecar exceeds 16 MiB")); }
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        for sample in &snapshot.samples {
            for word in [sample.index, u32::from(sample.ready), sample.tid, 0] { out.extend_from_slice(&word.to_le_bytes()); }
            out.extend_from_slice(&sample.pc.to_le_bytes());
            out.extend_from_slice(&sample.lr.to_le_bytes());
        }
        for (index, (ready, values)) in snapshot.markers.iter().enumerate() {
            out.extend_from_slice(&(index as u32).to_le_bytes());
            out.extend_from_slice(&u32::from(*ready).to_le_bytes());
            for value in values { out.extend_from_slice(&value.to_le_bytes()); }
        }
        out.extend_from_slice(COMPLETE);
        Ok(out)
    }

    fn different_destinations(sidecar: &Path, legacy: &Path) -> io::Result<()> {
        if sidecar == legacy { Err(invalid("sidecar destination is the legacy profile")) } else { Ok(()) }
    }

    fn publish_new<W: Write>(sidecar: &Path, legacy: &Path, bytes: &[u8], open_new: impl FnOnce(&Path) -> io::Result<W>) -> io::Result<W> {
        different_destinations(sidecar, legacy)?;
        let mut file = open_new(sidecar)?;
        file.write_all(bytes)?;
        Ok(file)
    }

    // Resolve existing parent aliases even when the leaf does not exist.
    fn destination(path: &Path) -> io::Result<PathBuf> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        Ok(parent.canonicalize()?.join(path.file_name().ok_or_else(|| invalid("missing output filename"))?))
    }

    pub(super) fn dump(legacy: &Path) {
        let Some(config) = config() else { return };
        // Process-lifetime slots are never reset/reused, even across rearming.
        // At most one companion publication is attempted; no retry overwrite.
        if PUBLISHED.swap(true, Ordering::Relaxed) { return; }
        let write = || -> io::Result<()> {
            let path = destination(&config.path)?;
            let legacy = destination(legacy)?;
            let bytes = encode(config, &snapshot())?;
            let file = publish_new(&path, &legacy, &bytes, |path| {
                std::fs::OpenOptions::new().write(true).create_new(true).open(path)
            })?;
            file.sync_all()
        };
        if let Err(error) = write() { log::warn!("host profile sidecar not completed: {error}"); }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn config_fixture() -> Config {
            Config { path: PathBuf::from("companion.bin"), receipt_path: "root-receipt.toml".into(),
                receipt: "fixture only; no runtime source claim".into(), from: 1500, end: 6000 }
        }

        fn snapshot_fixture() -> Snapshot {
            Snapshot { attempted: 2, attempted_after: 3, saturated: false,
                samples: vec![Sample { index: 0, ready: true, tid: 42, pc: 0x1234, lr: 0x5678 },
                    Sample { index: 1, ready: false, tid: 0, pc: 0, lr: 0 }],
                marker_attempted: 1, marker_saturated: false, markers: vec![(true, [1500, 0, 1, 100, 101, 1 | (2 << 32)])] }
        }

        #[test]
        fn start_length_is_half_open_and_checked() {
            let range = window(1500, 4500).unwrap();
            assert_eq!(range, 1500..6000);
            assert!(!range.contains(&1499));
            assert!(range.contains(&1500) && range.contains(&5999));
            assert!(!range.contains(&6000));
            assert!(window(1500, 0).is_none());
            assert!(window(u64::MAX, 1).is_none());
        }

        #[test]
        fn saturation_never_wraps_or_reuses_a_slot() {
            let next = AtomicUsize::new(SAMPLES - 1);
            let saturated = AtomicBool::new(false);
            assert_eq!(reserve(&next, &saturated), SAMPLES - 1);
            assert_eq!(reserve(&next, &saturated), SAMPLES);
            next.store(usize::MAX - 1, Ordering::Relaxed);
            assert_eq!(reserve(&next, &saturated), usize::MAX - 1);
            assert_eq!(reserve(&next, &saturated), usize::MAX);
            assert_eq!(reserve(&next, &saturated), usize::MAX);
            assert_eq!(next.load(Ordering::Relaxed), usize::MAX);
            assert!(saturated.load(Ordering::Relaxed));
        }

        #[test]
        fn unfinished_publication_is_not_a_complete_numeric_sample() {
            let ready = AtomicBool::new(false);
            let pc = AtomicU64::new(0x1234);
            let lr = AtomicU64::new(0x5678);
            let tid = AtomicU32::new(42);
            assert_eq!(sample(7, &ready, &pc, &lr, &tid), Sample { index: 7, ready: false, tid: 0, pc: 0, lr: 0 });
            ready.store(true, Ordering::Release);
            assert_eq!(sample(7, &ready, &pc, &lr, &tid), Sample { index: 7, ready: true, tid: 42, pc: 0x1234, lr: 0x5678 });
        }

        #[test]
        fn marker_uncertainty_is_explicit() {
            assert_eq!(marker_flags(Some(100), Some(101), 5, 5, false), 0);
            assert_eq!(marker_flags(Some(100), Some(101), 5, 6, false), 2);
            assert_eq!(marker_flags(None, Some(101), 5, 5, false), 1);
            assert_eq!(marker_flags(Some(101), Some(100), 5, 5, true), 12);
        }

        #[test]
        fn encoding_retains_raw_identity_and_unknown_rows_with_bounded_size() {
            let snapshot = snapshot_fixture();
            let bytes = encode(&config_fixture(), &snapshot).unwrap();
            assert_eq!(&bytes[..8], MAGIC);
            let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let text = std::str::from_utf8(&bytes[12..12 + length]).unwrap();
            let metadata: toml::Value = toml::from_str(text).unwrap();
            assert_eq!(metadata["requested_start"].as_str(), Some("1500"));
            assert_eq!(metadata["requested_length"].as_str(), Some("4500"));
            assert_eq!(metadata["requested_end_exclusive"].as_str(), Some("6000"));
            assert_eq!(metadata["stored_in_export"].as_integer(), Some(1));
            assert_eq!(metadata["incomplete_in_export"].as_integer(), Some(1));
            assert_eq!(metadata["attempted_after_snapshot"].as_str(), Some("3"));
            assert_eq!(metadata["unknown_tail"].as_bool(), Some(true));
            let rows = &bytes[12 + length..];
            assert_eq!(u32::from_le_bytes(rows[8..12].try_into().unwrap()), 42);
            assert_eq!(u64::from_le_bytes(rows[16..24].try_into().unwrap()), 0x1234);
            assert_eq!(u64::from_le_bytes(rows[24..32].try_into().unwrap()), 0x5678);
            assert_eq!(&rows[36..64], &[0; 28]);
            assert_eq!(&bytes[bytes.len() - 8..], COMPLETE);
            assert_eq!(bytes.len(), 20 + length + 2 * 32 + 56);
            assert!(20 + HEADER_CAP + SAMPLES * 32 + MARKERS * 56 <= OUTPUT_CAP);
        }

        #[test]
        fn overflow_and_oversize_metadata_are_explicit() {
            let mut snapshot = snapshot_fixture();
            snapshot.attempted = SAMPLES + 17;
            snapshot.marker_attempted = MARKERS + 3;
            let config = config_fixture();
            let header = header(&config, &snapshot);
            assert_eq!(header.overflow_at_snapshot, "17");
            assert_eq!(header.marker_overflow, "3");
            let mut config = config_fixture();
            config.receipt = "x".repeat(HEADER_CAP + 1);
            assert!(encode(&config, &snapshot).is_err());
        }

        #[test]
        fn saturated_counters_still_encode_as_explicit_lower_bounds() {
            let mut snapshot = snapshot_fixture();
            snapshot.attempted = usize::MAX;
            snapshot.attempted_after = usize::MAX;
            snapshot.marker_attempted = usize::MAX;
            snapshot.saturated = true;
            snapshot.marker_saturated = true;
            let bytes = encode(&config_fixture(), &snapshot).unwrap();
            let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let text = std::str::from_utf8(&bytes[12..12 + length]).unwrap();
            let metadata: toml::Value = toml::from_str(text).unwrap();
            let expected = usize::MAX.to_string();
            assert_eq!(metadata["attempted_at_snapshot"].as_str(), Some(expected.as_str()));
            assert_eq!(metadata["attempted_saturated"].as_bool(), Some(true));
            assert_eq!(metadata["marker_attempted_saturated"].as_bool(), Some(true));
        }

        #[test]
        fn output_policy_rejects_same_resolved_destination() {
            let legacy = Path::new("/resolved/zakuro-host-profile.txt");
            assert!(different_destinations(legacy, legacy).is_err());
            assert!(different_destinations(Path::new("/resolved/companion.bin"), legacy).is_ok());
            let rejected = publish_new::<Vec<u8>>(legacy, legacy, b"fixture", |_| panic!("must not open legacy output"));
            assert_eq!(rejected.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }

        #[test]
        fn exclusive_create_rejection_never_retries_or_overwrites() {
            let legacy = Path::new("/resolved/zakuro-host-profile.txt");
            let path = Path::new("/resolved/companion.bin");
            let calls = std::cell::Cell::new(0);
            let rejected = publish_new::<Vec<u8>>(path, legacy, b"fixture", |_| {
                calls.set(calls.get() + 1);
                Err(io::Error::new(io::ErrorKind::AlreadyExists, "fixture exists"))
            });
            assert_eq!(rejected.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(calls.get(), 1);
            let written = publish_new(path, legacy, b"fixture", |_| Ok(Vec::<u8>::new())).unwrap();
            assert_eq!(written.as_slice(), b"fixture");
            // Runtime supplies OpenOptions::create_new(true); actual OS
            // path/symlink behavior is deliberately left for root QA.
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_row_is_unchanged_and_has_no_sidecar_fields() {
        assert_eq!(legacy_row!("pocket-native", "/host", 0x1234u64, "?", 0u64), "pocket-native /host 1234 ? 0\n");
        assert_eq!(legacy_row!("", "?", 0u64, "?", 0u64), " ? 0 ? 0\n");
    }
}
