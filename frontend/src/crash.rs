//! what ends the game, a panic or a fatal signal, written into the log
//! first: on Android standard error goes nowhere, and a crash would leave
//! nothing to go on.

use std::sync::atomic::{AtomicI32, Ordering};

/// the log's file, which a signal handler can write to as it is.
static LOG: AtomicI32 = AtomicI32::new(2);

const SIGNALS: [libc::c_int; 5] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];

/// the handlers before, Android's among them, which make its own report.
static mut PREVIOUS: [Option<libc::sigaction>; 5] = [None; 5];

/// logs panics, and fatal signals into the file log has, standard error
/// without one.
pub fn watch(log: Option<libc::c_int>) {
    if let Some(log) = log {
        LOG.store(log, Ordering::Relaxed);
    }
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("the {} thread {info}", std::thread::current().name().unwrap_or("unnamed"));
        before(info);
    }));
    // SAFETY: installing handlers, the ones before kept for them to go on to
    unsafe {
        for (slot, &signal) in SIGNALS.iter().enumerate() {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, &action, &mut old) == 0 {
                PREVIOUS[slot] = Some(old);
            }
        }
    }
}

/// a line put together without allocating, as a signal handler must.
struct Line {
    bytes: [u8; 512],
    len: usize,
}

impl Line {
    fn text(&mut self, text: &[u8]) {
        let take = text.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&text[..take]);
        self.len += take;
    }

    fn hex(&mut self, value: u64) {
        let mut digits = [0u8; 18];
        digits[..2].copy_from_slice(b"0x");
        for (i, digit) in digits[2..].iter_mut().enumerate() {
            *digit = b"0123456789abcdef"[(value >> (60 - i * 4)) as usize & 15];
        }
        let first = digits[2..].iter().position(|&digit| digit != b'0').unwrap_or(15);
        self.text(&digits[..2]);
        self.text(&digits[2 + first..]);
    }

    /// SAFETY: text is null or a nul-terminated string
    unsafe fn c_text(&mut self, text: *const libc::c_char) {
        if !text.is_null() {
            self.text(std::ffi::CStr::from_ptr(text).to_bytes());
        }
    }

    /// where an address lies: its library and the offset in it, and the
    /// function's name when the library keeps it.
    /// SAFETY: any address, dladdr only looks it up
    unsafe fn place(&mut self, address: u64) {
        self.hex(address);
        let mut found: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(address as *const libc::c_void, &mut found) != 0 {
            self.text(b" ");
            let name = std::ffi::CStr::from_ptr(found.dli_fname).to_bytes();
            self.text(name.rsplit(|&byte| byte == b'/').next().unwrap_or(name));
            self.text(b"+");
            self.hex(address - found.dli_fbase as u64);
            if !found.dli_sname.is_null() {
                self.text(b" ");
                self.c_text(found.dli_sname);
            }
        }
    }
}

extern "C" fn on_signal(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the kernel hands a valid siginfo and context, and what follows
    // writes and looks up without allocating
    unsafe {
        let mut line = Line { bytes: [0; 512], len: 0 };
        line.text(b"fatal signal ");
        line.c_text(libc::strsignal(signal));
        line.text(b" in the ");
        let mut name = [0u8; 17];
        libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr());
        line.c_text(name.as_ptr() as *const libc::c_char);
        line.text(b" thread");
        if matches!(signal, libc::SIGSEGV | libc::SIGBUS) {
            line.text(b" reading or writing ");
            line.hex((*info).si_addr() as u64);
        }
        let context = &*(context as *const libc::ucontext_t);
        #[cfg(target_arch = "aarch64")]
        let (pc, caller) = (context.uc_mcontext.pc, Some(context.uc_mcontext.regs[30]));
        #[cfg(target_arch = "x86_64")]
        let (pc, caller) = (context.uc_mcontext.gregs[libc::REG_RIP as usize] as u64, None::<u64>);
        line.text(b", at ");
        line.place(pc);
        if let Some(caller) = caller {
            line.text(b", called from ");
            line.place(caller);
        }
        line.text(b"\n");
        libc::write(LOG.load(Ordering::Relaxed), line.bytes.as_ptr() as *const libc::c_void, line.len);
        zakuro_gpu::note_compiler_crash(LOG.load(Ordering::Relaxed));
        // the handler before takes it from here, the signal coming again
        // as the instruction runs again, or as abort raises it once more
        let slot = SIGNALS.iter().position(|&s| s == signal).unwrap_or(0);
        let mut previous = std::ptr::addr_of!(PREVIOUS).read()[slot].unwrap_or_else(|| std::mem::zeroed());
        if previous.sa_sigaction == on_signal as *const () as usize {
            previous.sa_sigaction = libc::SIG_DFL;
        }
        libc::sigaction(signal, &previous, std::ptr::null_mut());
    }
}
