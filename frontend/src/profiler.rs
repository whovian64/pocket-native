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
    let at = NEXT.fetch_add(1, Ordering::Relaxed);
    if at < SAMPLES {
        PCS[at].store(pc, Ordering::Relaxed);
        LRS[at].store(lr, Ordering::Relaxed);
        TIDS[at].store(tid, Ordering::Relaxed);
    }
}

pub fn start() {
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
    for at in 0..count {
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
        out += &format!("{name} {file} {offset:x} {caller} {from:x}\n");
    }
    let _ = std::fs::write(path, out);
    log::info!("wrote {count} host samples to {}", path.display());
}
