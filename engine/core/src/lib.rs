//! the emulated console, CPU, memory, HLE kernel, HLE services and GPU, and
//! the loop that drives them.

pub mod cro;
pub mod hints;
pub mod kernel;
pub mod loader;
pub mod memory;
pub mod replay;
pub mod recompiled;
pub mod services;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use zakuro_common::memory_map::*;
use zakuro_common::ConsoleModel;
use zakuro_cpu::{Cpu, Exit};
use zakuro_fs::Title;
use zakuro_gpu::{Gpu, GpuMemory, Renderer, SoftwareRenderer};

use kernel::thread::{ThreadId, WaitResult, WaitSyscall, THREAD_EXIT_MAGIC};
use kernel::Kernel;
use memory::{Memory, MemoryState, Permission};
use services::hid::InputState;
use services::ServiceState;

/// console settings the frontend can change.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ConsoleModel,
    pub new3ds: bool,
    /// 0 = Japan, 1 = USA, 2 = Europe.
    pub region: u8,
    pub language: u8,
    pub slider_3d: f32,
    /// a library 3dsrecomp built for the title, or a directory holding one
    /// named after its title id.
    pub recompiled: Option<std::path::PathBuf>,
    /// recompiled code linked into the program, which comes before a
    /// library.
    pub linked: Option<recompiled::Linked>,
    /// with no library given, run the one 3dsrecomp build installed for the
    /// title, if it did.
    pub find_recompiled: bool,
    /// where saves and dumped system files live, the working directory
    /// when there is none.
    pub data_dir: Option<std::path::PathBuf>,
    /// draw on the host's GPU through OpenGL ES, when there is one that can.
    pub hardware_renderer: bool,
    /// with hardware_renderer, the host's GPU only showing the screens, the
    /// software rasterizer drawing them.
    pub software_draws: bool,
    /// how many times the console's resolution the host's GPU draws at.
    pub resolution: u32,
    /// the time the console's clock starts at, in milliseconds since 1900,
    /// the host's when none, a fixed one makes runs repeat exactly.
    pub clock: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: ConsoleModel::Old3ds,
            new3ds: false,
            region: services::cfg::REGION_USA,
            language: services::cfg::LANGUAGE_ENGLISH,
            slider_3d: 0.0,
            recompiled: None,
            linked: None,
            find_recompiled: false,
            data_dir: None,
            hardware_renderer: false,
            software_draws: false,
            resolution: 1,
            clock: None,
        }
    }
}

/// whether ZAKURO_EXACT_EVENTS asks for the interpreter to step up to an
/// event that leaves too little time for a recompiled block, rather than the
/// block running past it.
fn exact_events() -> bool {
    static EXACT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EXACT.get_or_init(|| {
        let exact = std::env::var_os("ZAKURO_EXACT_EVENTS").is_some();
        if exact {
            log::info!("switch: exact event timing, ZAKURO_EXACT_EVENTS");
        }
        exact
    })
}

/// how a frame of emulation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// a full frame's worth of cycles ran.
    Completed,
    /// the title called svcExitProcess.
    Exited,
    /// the title called svcBreak, or hit an instruction we do not have.
    Faulted,
}

/// a screen's picture as RGBA, and its width and height.
pub type Screen = (std::sync::Arc<Vec<u8>>, u32, u32);

pub struct System {
    pub cpu: Cpu,
    pub memory: Memory,
    pub kernel: Kernel,
    pub services: ServiceState,
    pub gpu: Gpu,
    pub renderer: Box<dyn Renderer>,
    pub title: Option<Title>,
    pub config: Config,

    pub exited: bool,
    pub broke: bool,
    /// everything the title wrote with svcOutputDebugString.
    pub debug_output: String,
    pub unimplemented_svcs: BTreeSet<u32>,
    pub services_seen: BTreeSet<String>,
    pub lcd_force_black: bool,
    /// with the GPU on its own thread and the title paced to real time,
    /// time goes on while the GPU works instead of waiting for it.
    pub realtime_gpu: bool,
    /// the scaled picture each screen had at the last presentation, which
    /// goes up at the next, once the GPU had a frame's time to draw it.
    showing: [Option<zakuro_gpu::ScreenRef>; 2],
    /// the panel's layout, cursor, blanking, filtering and overlay at the
    /// last presentation, and the presentations since one was made whether
    /// anything changed or not.
    panel: (Option<([Option<[i32; 4]>; 2], Option<(i32, i32)>, bool, bool, usize)>, u32),
    /// the buffers the screens were given last, as virtual addresses.
    screen_buffers: Vec<u32>,
    /// the dynamic module loader's state.
    pub cro: cro::CroManager,
    /// errors the title reported through err:f, newest last.
    pub fatal_errors: Vec<String>,
    /// undefined instructions we have seen, so the log stays readable.
    undefined_seen: BTreeMap<u32, u32>,
    /// branches into unmapped memory, keyed by target.
    pub prefetch_aborts: BTreeMap<u32, u32>,
    /// the last few instruction addresses, so a fault can say how it got there.
    history: [u32; HISTORY_LENGTH],
    history_index: usize,
    /// where and why the library last handed back, and in which thread, for
    /// telling why a place it has no code for was reached.
    library_left: (u32, &'static str, Option<ThreadId>),
    /// the new hints logged so far, a few hundred tell enough.
    hints_logged: u32,
    /// frames completed since boot.
    pub frames: u64,
    /// the console's clock at boot, in milliseconds since 1900.
    pub(crate) boot_clock: u64,
    /// cycle count at which the next end-of-frame work is due.
    next_frame_boundary: u64,
    /// cycle count at which the DSP next finishes an audio frame.
    next_audio_frame: u64,
    /// cycle count at which the scheduler is next forced to run.
    next_preempt: u64,

    /// sampling profiler, counts how often each thread was found at each PC.
    pub profile: Option<BTreeMap<(String, u32), u64>>,

    /// code recompiled ahead of time, which runs instead of the interpreter
    /// wherever it has something.
    pub recompiled: Option<recompiled::Library>,
    /// where the title ran in the interpreter despite the library.
    pub hints: Option<hints::Hints>,
    gpu_queue: Option<GpuQueue>,
    /// instructions the interpreter ran for want of recompiled code, and
    /// the ones recompiled code ran, to see how much the library covers.
    pub interpreted_instructions: u64,
    pub recompiled_instructions: u64,
}

/// cycles in one 60 Hz frame at the ARM11's clock.
pub const CYCLES_PER_FRAME: u64 = kernel::thread::CPU_CLOCK_HZ / 60;

/// cycles in one audio frame, 160 samples, each exactly 8192 cycles long
/// (the DSP's 32728 Hz is the CPU clock divided by 8192).
pub const CYCLES_PER_AUDIO_FRAME: u64 = 160 * 8192;
/// samples the DSP plays each second, one every 8192 cycles.
pub const AUDIO_SAMPLE_RATE: f64 = kernel::thread::CPU_CLOCK_HZ as f64 / 8192.0;

/// how often the scheduler is forced to run even if no thread yields.
pub const PREEMPT_INTERVAL: u64 = 8192;

/// how many instruction addresses are kept for fault reports.
const HISTORY_LENGTH: usize = 64;

/// what one call to [System::step] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Ran,
    Exited,
    Faulted,
}

impl System {
    pub fn new(config: Config) -> System {
        let boot_clock = config.clock.unwrap_or_else(memory::config::host_clock);
        let app_bytes = 64 * 1024 * 1024;
        let mut services = ServiceState::default();
        if let Some(dir) = &config.data_dir {
            services.fs.user_dir = dir.join("user");
        }
        System {
            cpu: Cpu::new(),
            memory: Memory::new(config.new3ds, app_bytes),
            kernel: Kernel::new(0, memory::MemoryRegion::Application, linear_heap_base(config.new3ds)),
            services,
            gpu: Gpu::new(),
            renderer: Box::new(SoftwareRenderer::default()),
            title: None,
            config,
            exited: false,
            broke: false,
            debug_output: String::new(),
            unimplemented_svcs: BTreeSet::new(),
            services_seen: BTreeSet::new(),
            lcd_force_black: false,
            realtime_gpu: false,
            showing: [None, None],
            panel: (None, 0),
            screen_buffers: Vec::new(),
            cro: cro::CroManager::default(),
            fatal_errors: Vec::new(),
            undefined_seen: BTreeMap::new(),
            prefetch_aborts: BTreeMap::new(),
            history: [0; HISTORY_LENGTH],
            history_index: 0,
            library_left: (0, "", None),
            hints_logged: 0,
            frames: 0,
            boot_clock,
            next_frame_boundary: CYCLES_PER_FRAME,
            next_audio_frame: CYCLES_PER_AUDIO_FRAME,
            next_preempt: PREEMPT_INTERVAL,
            profile: None,
            recompiled: None,
            hints: None,
            gpu_queue: None,
            interpreted_instructions: 0,
            recompiled_instructions: 0,
        }
    }

    // -- process setup ------------------------------------------------------

    /// maps the page holding a thread's TLS block, if it is not mapped yet.
    pub fn map_tls_page(&mut self, thread: ThreadId) {
        let tls = self.kernel.thread(thread).tls;
        let page = tls & !PAGE_MASK;
        if self.memory.mapping_at(page).is_some() {
            return;
        }
        let Some(block) = self
            .memory
            .phys
            .allocate(memory::MemoryRegion::Base, PAGE_SIZE)
        else {
            log::error!("out of memory allocating a TLS page");
            return;
        };
        self.memory.map(
            page,
            block.addr,
            PAGE_SIZE,
            Permission::RW,
            MemoryState::Locked,
        );
        // TLS must start zeroed, the IPC command buffer lives in it.
        self.memory.zero_physical(block.addr, PAGE_SIZE);
    }

    // -- the run loop -------------------------------------------------------

    /// runs one frame's worth of emulation.
    pub fn run_frame(&mut self) -> FrameOutcome {
        // HID samples its inputs on its own, whether or not anyone pressed
        // anything
        let input = self.services.hid.input;
        services::hid::update(self, input);
        // run up to the boundary step() would end the frame at, and end it
        // here, once, instead of again when the next frame's first step
        // finds the boundary passed
        let deadline = self.next_frame_boundary;
        while self.cpu.cycles < deadline {
            match self.step(Some(deadline)) {
                StepOutcome::Ran => {}
                StepOutcome::Exited => return FrameOutcome::Exited,
                StepOutcome::Faulted => return FrameOutcome::Faulted,
            }
        }
        self.next_frame_boundary = self.cpu.cycles + CYCLES_PER_FRAME;
        self.end_frame();
        FrameOutcome::Completed
    }

    /// advances the machine by one instruction, doing whatever scheduling is
    /// due first.
    pub fn step(&mut self, deadline: Option<u64>) -> StepOutcome {
        if self.exited {
            return StepOutcome::Exited;
        }
        if self.broke {
            return StepOutcome::Faulted;
        }

        // fire the end-of-frame work on schedule even when a tool is stepping,
        // or a title waiting on vertical blank would never be woken.
        if self.cpu.cycles >= self.next_frame_boundary {
            self.next_frame_boundary = self.cpu.cycles + CYCLES_PER_FRAME;
            self.end_frame();
        }
        if self.cpu.cycles >= self.next_audio_frame {
            self.next_audio_frame = self.cpu.cycles + CYCLES_PER_AUDIO_FRAME;
            self.audio_frame();
        }

        if self.kernel.reschedule_pending || self.kernel.current_thread.is_none() {
            self.kernel.reschedule_pending = false;
            let tick = self.cpu.cycles;
            if self.kernel.schedule(&mut self.cpu, tick) {
                self.apply_wait_result();
            }
        }

        if self.kernel.current_thread.is_none() && self.gpu_busy() {
            if !self.realtime_gpu {
                self.wait_gpu();
                self.kernel.reschedule_pending = true;
                return StepOutcome::Ran;
            }
            if self.gpu_interrupts() {
                self.kernel.reschedule_pending = true;
                return StepOutcome::Ran;
            }
        }
        if self.kernel.current_thread.is_none() {
            // everything is blocked.
            let mut limit = deadline.unwrap_or(self.cpu.cycles + CYCLES_PER_FRAME);
            // an audio thread waits on the DSP's interrupt, which only fires
            // at an audio frame boundary, skipping past one would lose it.
            if self.services.dsp.running {
                limit = limit.min(self.next_audio_frame);
            }
            match self.next_wakeup() {
                Some(tick) if tick > self.cpu.cycles => self.cpu.cycles = tick.min(limit),
                _ => self.cpu.cycles = limit,
            }
            self.kernel.reschedule_pending = true;
            return StepOutcome::Ran;
        }

        // a thread whose entry point returned branches to this address.
        if self.cpu.regs[15] == THREAD_EXIT_MAGIC {
            if let Some(id) = self.kernel.current_thread {
                self.kernel.end_thread(id);
            }
            return StepOutcome::Ran;
        }

        // a branch through an uninitialized function pointer lands on the null
        // page, whose zero words decode as harmless no-ops.
        let pc = self.cpu.regs[15];
        if !self.memory.is_executable(pc) {
            let count = self.prefetch_aborts.entry(pc).or_insert(0);
            *count += 1;
            if *count == 1 {
                let thread = self
                    .kernel
                    .current()
                    .map_or("?".to_owned(), |t| t.name.clone());
                log::error!(
                    "prefetch abort: {thread} branched to unmapped 0x{pc:08X} from 0x{:08X}",
                    self.cpu.regs[14]
                );
                log::error!("{:?}", self.cpu);
                log::error!("the instructions leading here were:");
                let recent = self.recent_instructions();
                for &entry in recent.iter().rev().take(8).rev() {
                    let address = entry & !1;
                    let mut bytes = [0u8; 8];
                    self.memory.read_bytes(address & !3, &mut bytes);
                    log::error!(
                        "  0x{address:08X} {} [{:02X?}]",
                        if entry & 1 != 0 { "T" } else { "A" },
                        bytes
                    );
                }
                self.fatal_errors.push(format!(
                    "branch to unmapped 0x{pc:08X} (lr 0x{:08X})",
                    self.cpu.regs[14]
                ));
            }
            // stop the thread rather than the machine, the rest of the title
            // may still make progress, and the report says what happened.
            if let Some(id) = self.kernel.current_thread {
                self.kernel.end_thread(id);
            }
            return StepOutcome::Ran;
        }

        self.history[self.history_index] = pc | self.cpu.cpsr.thumb as u32;
        self.history_index = (self.history_index + 1) % HISTORY_LENGTH;

        if self.profile.is_some() {
            self.sample();
        }

        let exit = match self.run_recompiled(deadline) {
            Some(exit) => {
                if let Some(hints) = &mut self.hints {
                    hints.library_ran();
                }
                exit
            }
            None => {
                self.interpreted_instructions += 1;
                interpreted_at(pc | self.cpu.cpsr.thumb as u32);
                let thumb = self.cpu.cpsr.thumb;
                if let (Some(hints), Some(library)) = (&mut self.hints, &self.recompiled) {
                    if !library.has_code(pc | thumb as u32) && hints.interpreted(pc, thumb) && self.hints_logged < 300 {
                        self.hints_logged += 1;
                        let before = self.history[(self.history_index + HISTORY_LENGTH - 2) % HISTORY_LENGTH];
                        let (left, why, thread) = self.library_left;
                        log::info!(
                            "new hint {:08X}: the step before was at {before:08X}, the library last stopped at {left:08X} ({why}) in thread {thread:?}, now thread {:?}",
                            pc | thumb as u32,
                            self.kernel.current_thread
                        );
                    }
                }
                self.cpu.step(&mut self.memory)
            }
        };
        match exit {
            None => {}
            Some(Exit::Supervisor(number)) => {
                // the GPU's interrupts go into a queue the title pops with
                // exclusive loads and stores, they land between its
                // instructions only at a system call, as on the console
                self.gpu_interrupts();
                kernel::svc::dispatch(self, number);
            }
            Some(Exit::Undefined { pc, opcode }) => {
                let count = self.undefined_seen.entry(pc).or_insert(0);
                *count += 1;
                if *count == 1 {
                    log::error!("undefined instruction 0x{opcode:08X} at 0x{pc:08X}");
                    log::error!("{:?}", self.cpu);
                }
                // skip it and keep going, one bad decode should not end the
                // session while the CPU is still being filled in.
                let step = if self.cpu.cpsr.thumb { 2 } else { 4 };
                self.cpu.regs[15] = pc.wrapping_add(step);
            }
            Some(Exit::Breakpoint { pc, imm }) => {
                log::warn!("bkpt #{imm} at 0x{pc:08X}");
                let step = if self.cpu.cpsr.thumb { 2 } else { 4 };
                self.cpu.regs[15] = pc.wrapping_add(step);
            }
            Some(Exit::Halted) => {
                self.cpu.resume();
                self.kernel.reschedule_pending = true;
            }
            Some(Exit::Timeout) => {}
        }
        if let Some(address) = self.memory.new_fault.take() {
            let thread = self.kernel.current().map_or("?".to_owned(), |t| t.name.clone());
            log::warn!("access to unmapped guest address 0x{address:08X} by {thread} at 0x{pc:08X}");
        }

        // give the scheduler a chance on a regular cadence so that a thread
        // which never makes a syscall cannot monopolise the core.
        if self.cpu.cycles >= self.next_preempt {
            self.next_preempt = self.cpu.cycles + PREEMPT_INTERVAL;
            self.kernel.reschedule_pending = true;
        }

        if self.exited {
            StepOutcome::Exited
        } else if self.broke {
            StepOutcome::Faulted
        } else {
            StepOutcome::Ran
        }
    }

    /// runs recompiled code from the pc up to the next thing the scheduler
    /// has to look at, when the library has code there. what it returns
    /// stands in for what one interpreted step would.
    fn run_recompiled(&mut self, deadline: Option<u64>) -> Option<Option<Exit>> {
        const OVERRUN_BUDGET: u64 = 1024;
        let library = self.recompiled.as_ref()?;
        if !library.has_code(self.cpu.regs[15] | self.cpu.cpsr.thumb as u32) {
            return None;
        }
        let mut limit = self.next_preempt.min(self.next_frame_boundary).min(self.next_audio_frame);
        if let Some(deadline) = deadline {
            limit = limit.min(deadline);
        }
        let budget = limit.saturating_sub(self.cpu.cycles).max(1);
        let (mut ran, mut stop) = library.run(&mut self.cpu, &mut self.memory, budget);
        if !exact_events() && ran == 0 && matches!(stop, recompiled::Stop::Left) {
            // not enough budget left for a whole block, which the interpreter
            // would step through one instruction at a time, overrunning the
            // event by a block costs far less
            (ran, stop) = library.run(&mut self.cpu, &mut self.memory, budget.max(OVERRUN_BUDGET));
        }
        self.recompiled_instructions += ran;
        let why = match stop {
            recompiled::Stop::Svc(_) => "an svc",
            recompiled::Stop::Exit(_) => "an instruction it interpreted stopped",
            recompiled::Stop::Left if ran == 0 => "no budget for a block",
            recompiled::Stop::Left => "out of budget or out of its code",
        };
        self.library_left = (self.cpu.regs[15] | self.cpu.cpsr.thumb as u32, why, self.kernel.current_thread);
        if ran == 0 && matches!(stop, recompiled::Stop::Left) {
            return None;
        }
        Some(match stop {
            recompiled::Stop::Svc(number) => Some(Exit::Supervisor(number)),
            recompiled::Stop::Exit(exit) => Some(exit),
            recompiled::Stop::Left => None,
        })
    }

    /// the instruction addresses executed most recently, oldest first.
    pub fn recent_instructions(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(HISTORY_LENGTH);
        for offset in 0..HISTORY_LENGTH {
            let entry = self.history[(self.history_index + offset) % HISTORY_LENGTH];
            if entry != 0 {
                out.push(entry);
            }
        }
        out
    }

    /// starts collecting PC samples.
    pub fn enable_profiler(&mut self) {
        self.profile = Some(BTreeMap::new());
    }

    /// the hottest sampled locations, most frequent first.
    pub fn hot_spots(&self, count: usize) -> Vec<(String, u32, u64)> {
        let Some(profile) = &self.profile else {
            return Vec::new();
        };
        let mut entries: Vec<(String, u32, u64)> = profile
            .iter()
            .map(|((thread, pc), hits)| (thread.clone(), *pc, *hits))
            .collect();
        entries.sort_by_key(|&(_, _, hits)| std::cmp::Reverse(hits));
        entries.truncate(count);
        entries
    }

    fn sample(&mut self) {
        let Some(id) = self.kernel.current_thread else {
            return;
        };
        let name = self.kernel.thread(id).name.clone();
        let pc = self.cpu.regs[15];
        if let Some(profile) = &mut self.profile {
            *profile.entry((name, pc)).or_insert(0) += 1;
        }
    }

    /// writes back the result registers of a syscall that had blocked.
    fn apply_wait_result(&mut self) {
        let Some(id) = self.kernel.current_thread else {
            return;
        };
        let thread = self.kernel.thread_mut(id);
        let (Some(result), Some(syscall)) = (thread.wait_result.take(), thread.wait_syscall.take())
        else {
            return;
        };

        match (syscall, result) {
            (WaitSyscall::WaitSynchronization1, WaitResult::Signaled(_)) => {
                self.cpu.regs[0] = 0;
            }
            (WaitSyscall::WaitSynchronizationN, WaitResult::Signaled(index)) => {
                self.cpu.regs[0] = 0;
                self.cpu.regs[1] = index as u32;
            }
            (WaitSyscall::ArbitrateAddress, WaitResult::Signaled(_)) => {
                self.cpu.regs[0] = 0;
            }
            (_, WaitResult::TimedOut) => {
                self.cpu.regs[0] = zakuro_common::result::errors::TIMEOUT.0;
            }
            (WaitSyscall::SleepThread, _) => {
                self.cpu.regs[0] = 0;
            }
        }
    }

    /// the earliest tick any blocked thread or armed timer wants to wake at.
    fn next_wakeup(&self) -> Option<u64> {
        let threads = self.kernel.next_thread_wakeup();
        let timers = self.kernel.objects.iter().filter_map(|(_, object)| {
            match object {
                kernel::object::KObject::Timer(timer) => timer.fire_at,
                _ => None,
            }
        });
        threads.into_iter().chain(timers).min()
    }

    /// everything that happens between frames, vertical blank, input, clock.
    fn end_frame(&mut self) {
        self.frames += 1;
        if self.frames.is_multiple_of(hints::SAVE_EVERY) {
            if let Some(hints) = &mut self.hints {
                hints.save();
            }
        }

        // refresh the kernel's shared page so the guest's clock advances.
        let tick = self.cpu.cycles;
        memory::config::update_datetime(self.memory.phys.shared_page_mut(), self.boot_clock, tick);

        // fire the expired timers.
        let mut signalled = Vec::new();
        for (id, object) in self.kernel.objects.iter() {
            if let kernel::object::KObject::Timer(timer) = object {
                if timer.fire_at.is_some_and(|at| tick >= at) {
                    signalled.push(id);
                }
            }
        }
        for id in signalled {
            if let Some(kernel::object::KObject::Timer(timer)) = self.kernel.objects.get_mut(id) {
                timer.signaled = true;
                timer.fire_at = if timer.interval > 0 {
                    Some(tick + timer.interval)
                } else {
                    None
                };
            }
            self.kernel.reschedule_pending = true;
        }

        self.gpu_work(&[], |_, renderer, _| renderer.end_frame());

        // pick up any buffer swap the game queued directly in GSP shared
        // memory before the LCDs latch whatever is currently configured.
        services::gsp::apply_framebuffer_updates(self);

        // both LCDs finish scanning out, in that order.
        services::gsp::signal_interrupt(self, services::gsp::InterruptId::Pdc0);
        services::gsp::signal_interrupt(self, services::gsp::InterruptId::Pdc1);
        services::cam::frame(self);

    }

    /// the DSP finishing an audio frame, it plays its voices one frame on and
    /// interrupts the title, whose sound library does one update per interrupt.
    fn audio_frame(&mut self) {
        // the whole opening froze until this ran every 4.9 ms. goddamn audio timing
        if self.services.dsp.running {
            services::dsp::advance(self);
            services::dsp::signal_semaphore(self);
        }
    }

    /// the stereo samples the console played since the last call, at
    /// AUDIO_SAMPLE_RATE.
    pub fn take_audio(&mut self) -> Vec<[i16; 2]> {
        std::mem::take(&mut self.services.dsp.output)
    }

    /// what the software keyboard asks for, while a title waits on it.
    pub fn keyboard_request(&self) -> Option<&services::keyboard::Request> {
        services::keyboard::request(self)
    }

    /// closes the software keyboard with the text typed and the button
    /// pressed, zero being the leftmost and the last one confirming.
    pub fn answer_keyboard(&mut self, text: &str, button: usize) {
        services::keyboard::answer(self, text, button);
    }

    /// what the buttons, circle pad and touch screen are doing, which HID
    /// reports from the next frame on.
    pub fn set_input(&mut self, input: InputState) {
        self.services.hid.input = input;
    }

    // -- GPU plumbing -------------------------------------------------------

    /// runs the GPU's work on a thread of its own from here on, the title
    /// going on while it draws and hearing that the work finished once it
    /// did, as on the console.
    pub fn enable_async_gpu(&mut self) {
        if self.gpu_queue.is_some() {
            return;
        }
        let (jobs, receiver) = std::sync::mpsc::channel::<GpuJob>();
        let spawned = std::thread::Builder::new().name("gpu".into()).spawn(move || {
            for job in receiver {
                job();
            }
        });
        if let Err(error) = spawned {
            log::warn!("no GPU thread, drawing on the CPU's, {error}");
            return;
        }
        self.gpu_queue = Some(GpuQueue { jobs, pending: Arc::default(), done: Arc::default(), finished: Arc::default(), presents: Arc::default() });
    }

    /// does GPU work, on the GPU thread when there is one, then raises its
    /// interrupts.
    fn gpu_work(&mut self, interrupts: &[services::gsp::InterruptId], work: impl FnOnce(&mut Gpu, &mut dyn Renderer, &mut GuestMemory) + 'static) {
        let linear_base = self.kernel.linear_base;
        let Some(queue) = &self.gpu_queue else {
            let mut guest = GuestMemory { linear_base, memory: &mut self.memory };
            work(&mut self.gpu, self.renderer.as_mut(), &mut guest);
            for &interrupt in interrupts {
                services::gsp::signal_interrupt(self, interrupt);
            }
            return;
        };
        let parts = Shared((&mut self.gpu as *mut Gpu, self.renderer.as_mut() as *mut dyn Renderer, &mut self.memory as *mut Memory));
        let interrupts = interrupts.to_vec();
        let (pending, done, finished) = (queue.pending.clone(), queue.done.clone(), queue.finished.clone());
        pending.fetch_add(1, Ordering::AcqRel);
        let work = Shared(work);
        let job = move || {
            let (parts, work) = (parts, work);
            let (gpu, renderer, memory) = parts.0;
            // SAFETY: the GPU and renderer are only touched here until the
            // queue is empty, wait_gpu sees to that, and the memory is the
            // console's, which the title shares with the GPU as on hardware
            unsafe {
                let mut guest = GuestMemory { linear_base, memory: &mut *memory };
                (work.0)(&mut *gpu, &mut *renderer, &mut guest);
            }
            if !interrupts.is_empty() {
                done.lock().unwrap().extend(interrupts);
                finished.fetch_add(1, Ordering::AcqRel);
            }
            pending.fetch_sub(1, Ordering::AcqRel);
        };
        let _ = queue.jobs.send(Box::new(job));
    }

    /// waits for the GPU thread to finish everything given to it, so the
    /// GPU's state can be looked at, and raises what it finished.
    pub fn wait_gpu(&mut self) {
        let Some(queue) = &self.gpu_queue else { return };
        if queue.pending.load(Ordering::Acquire) > 0 {
            let start = std::time::Instant::now();
            let mut tries = 0;
            while queue.pending.load(Ordering::Acquire) > 0 {
                pause(&mut tries);
            }
            GPU_WAITED.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        self.gpu_interrupts();
    }

    /// raises the interrupts of the GPU work that finished.
    fn gpu_interrupts(&mut self) -> bool {
        let Some(queue) = &self.gpu_queue else { return false };
        if queue.finished.load(Ordering::Acquire) == 0 {
            return false;
        }
        queue.finished.store(0, Ordering::Release);
        let finished = std::mem::take(&mut *queue.done.lock().unwrap());
        for interrupt in finished {
            services::gsp::signal_interrupt(self, interrupt);
        }
        true
    }

    fn gpu_busy(&self) -> bool {
        self.gpu_queue
            .as_ref()
            .is_some_and(|queue| queue.pending.load(Ordering::Acquire) > 0 || queue.finished.load(Ordering::Acquire) > 0)
    }

    /// skips the frame about to run when the frontend is behind, or when
    /// it will not be shown and the title makes a picture every frame.
    pub fn skip_frames(&mut self, behind: bool, unshown: bool) {
        self.gpu_work(&[], move |gpu, _, _| gpu.next_frame(behind, unshown));
    }

    /// draws the bottom screen with every picture while it changes, rather
    /// than every other one.
    pub fn set_bottom_full_rate(&mut self, full: bool) {
        self.gpu_work(&[], move |gpu, _, _| gpu.bottom_full_rate = full);
    }

    pub fn set_skip_bottom(&mut self, skip: bool) {
        self.gpu_work(&[], move |gpu, _, _| gpu.skip_bottom = skip);
    }

    pub fn set_bottom_relearn(&mut self, relearn: bool) {
        self.gpu_work(&[], move |gpu, _, _| gpu.bottom_relearn = relearn);
    }

    pub fn set_skip_right_eye(&mut self, skip: bool) {
        self.gpu_work(&[], move |gpu, _, _| gpu.skip_right_eye = skip);
    }

    /// how many display transfers the GPU did, which grows with each new
    /// picture.
    pub fn transfers(&mut self) -> u64 {
        self.wait_gpu();
        self.gpu.transfers
    }

    /// how many pictures the title finished for the top screen.
    pub fn top_pictures(&mut self) -> u64 {
        self.wait_gpu();
        self.gpu.top_pictures
    }

    /// how many bottom screen pictures were drawn.
    pub fn bottom_pictures(&mut self) -> u64 {
        self.wait_gpu();
        self.gpu.bottom_pictures
    }

    /// how many times the panel showed the screens.
    pub fn shown(&mut self) -> u64 {
        self.wait_gpu();
        self.gpu.shown
    }

    pub fn gpu_read_register(&mut self, offset: u32) -> u32 {
        self.wait_gpu();
        self.gpu.read_external(offset)
    }

    pub fn gpu_write_register(&mut self, offset: u32, value: u32) {
        self.wait_gpu();
        self.gpu.write_external(offset, value);
    }

    pub fn set_framebuffer(
        &mut self,
        screen: u32,
        active: u32,
        left: u32,
        right: u32,
        stride: u32,
        format: u32,
    ) {
        for buffer in [left, right].map(|buffer| services::gsp::physical_to_virtual(self, buffer)) {
            if buffer != 0 && !self.screen_buffers.contains(&buffer) {
                if self.screen_buffers.len() == 8 {
                    self.screen_buffers.remove(0);
                }
                self.screen_buffers.push(buffer);
            }
        }
        self.gpu_work(&[], move |gpu, _, _| gpu.set_framebuffer(screen, active, left, right, stride, format));
    }

    pub fn submit_command_list(&mut self, paddr: u32, size: u32) {
        self.gpu_work(&[services::gsp::InterruptId::P3d], move |gpu, renderer, guest| {
            gpu.process_command_list(guest, renderer, paddr, size);
        });
    }

    /// makes guest memory hold what the host GPU drew over a range, before
    /// something other than a draw reads or writes it.
    pub fn sync_gpu(&mut self, addr: u32, len: u32) {
        self.sync_gpu_from(addr, len, "other");
    }

    /// sync_gpu, counted by what asked for it, for the log: a sync that has
    /// to read a drawing back stalls the GPU.
    pub fn sync_gpu_from(&mut self, addr: u32, len: u32, from: &'static str) {
        count_sync(from);
        self.wait_gpu();
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu.sync_memory_for(&mut guest, addr, len, from);
    }

    /// lets the GPU know a range is about to be written whole: what it drew
    /// there is dropped rather than read back.
    pub fn overwrite_gpu(&mut self, addr: u32, len: u32, from: &'static str) {
        count_sync(from);
        self.wait_gpu();
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory { linear_base, memory: &mut self.memory };
        self.gpu.before_overwrite(&mut guest, addr, len);
    }

    /// copies guest memory once the GPU finished what it was given, raising
    /// the DMA interrupt.
    pub fn dma(&mut self, source: u32, dest: u32, size: u32) {
        self.sync_gpu_from(source, size, "dma");
        self.overwrite_gpu(dest, size, "dma into");
        let mut data = vec![0u8; size as usize];
        self.memory.read_bytes(source, &mut data);
        self.memory.write_bytes(dest, &data);
        // a title may copy its picture into a screen's buffer this way
        self.gpu.pictures += 1;
        services::gsp::signal_interrupt(self, services::gsp::InterruptId::Dma);
    }

    pub fn memory_fill(&mut self, start: u32, end: u32, value: u32, width: u32, interrupt: services::gsp::InterruptId) {
        self.gpu_work(&[interrupt], move |gpu, _, guest| gpu.memory_fill(guest, start, end, value, width));
    }

    pub fn display_transfer(
        &mut self,
        input: u32,
        output: u32,
        input_dimensions: u32,
        output_dimensions: u32,
        flags: u32,
    ) {
        // a picture for a screen is done as far as the title can tell once
        // the GPU has it: Smash sends the top screen's only when it hears the
        // bottom screen's went, which from the GPU thread comes once the
        // frame before is drawn, too late for it
        let to_screen = self.gpu_queue.is_some() && {
            let output = services::gsp::physical_to_virtual(self, output);
            self.screen_buffers.iter().any(|&buffer| output.abs_diff(buffer) < 0x1000)
        };
        let interrupts: &[_] = if to_screen { &[] } else { &[services::gsp::InterruptId::Ppf] };
        self.gpu_work(interrupts, move |gpu, _, guest| {
            gpu.display_transfer(guest, input, output, input_dimensions, output_dimensions, flags);
        });
        if to_screen {
            services::gsp::signal_interrupt(self, services::gsp::InterruptId::Ppf);
        }
    }

    pub fn texture_copy(
        &mut self,
        input: u32,
        output: u32,
        size: u32,
        input_gap: u32,
        output_gap: u32,
    ) {
        self.gpu_work(&[services::gsp::InterruptId::Ppf], move |gpu, _, guest| {
            gpu.texture_copy(guest, input, output, size, input_gap, output_gap);
        });
    }

    /// the nine words gsp::ImportDisplayCaptureInfo returns.
    pub fn display_capture_info(&mut self) -> [u32; 9] {
        self.wait_gpu();
        let top = self.gpu.framebuffers[0];
        let bottom = self.gpu.framebuffers[1];
        [
            top.address_left(),
            top.address_right(),
            top.format,
            top.stride,
            bottom.address_left(),
            bottom.address_right(),
            bottom.format,
            bottom.stride,
            0,
        ]
    }

    /// a screen as RGBA at the resolution the host's GPU draws at, and its
    /// width and height. a picture the GPU did not draw, or the CPU changed
    /// since, is the console's own grown to it, so the size stays the same
    /// from frame to frame.
    pub fn read_screen_scaled(&mut self, screen: zakuro_common::Screen) -> Screen {
        self.wait_gpu();
        let (width, height) = (screen.width(), screen.height());
        let scale = self.gpu.scale();
        if let Some(scaled) = self.scaled_screen(screen) {
            return scaled;
        }
        let native = self.read_screen(screen);
        if scale == 1 {
            return (std::sync::Arc::new(native), width, height);
        }
        (std::sync::Arc::new(grow(&native, width, scale)), width * scale, height * scale)
    }

    /// the picture the host's GPU drew scaled for a screen, when it is still
    /// what the screen shows.
    fn scaled_screen(&mut self, screen: zakuro_common::Screen) -> Option<Screen> {
        let (width, height) = (screen.width(), screen.height());
        let index = match screen {
            zakuro_common::Screen::Top => 0,
            zakuro_common::Screen::Bottom => 1,
        };
        let Some(now) = self.screen_ref(screen) else {
            self.showing[index] = None;
            return None;
        };
        // what the screen had a presentation ago goes up now, always that
        // one, so each picture stays up as long as the title kept it
        let show = self.showing[index].replace(now).unwrap_or(now);
        let (image, scale) = self.gpu.scaled_picture(show).or_else(|| self.gpu.scaled_picture(now))?;
        Some((image, width * scale, height * scale))
    }

    /// the picture the host's GPU drew for a screen, when it is still what
    /// the screen shows.
    fn screen_ref(&mut self, screen: zakuro_common::Screen) -> Option<zakuro_gpu::ScreenRef> {
        let mut guest = GuestMemory { linear_base: self.kernel.linear_base, memory: &mut self.memory };
        screen_ref(&mut self.gpu, &mut guest, screen, self.lcd_force_black)
    }

    /// shows the screens on the panel the host GPU drives itself, each in
    /// its area, left, top, width and height in the panel's pixels, none
    /// for a screen left off, the overlay over them and the top screen
    /// filtered when smooth. the GPU thread shows them once it drew them,
    /// the title going on with the next frame meanwhile.
    pub fn present_panel(&mut self, areas: [Option<[i32; 4]>; 2], cursor: Option<(i32, i32)>, overlay: Option<std::sync::Arc<zakuro_gpu::Overlay>>, smooth: bool, interpolate: bool) {
        let black = self.lcd_force_black;
        // the screens go up again when a picture reached one, the buffers
        // shown, the layout or the overlay changed, and every sixth time
        // anyway for a screen the CPU writes itself
        let drawn_over = overlay.as_ref().map_or(0, |overlay| std::sync::Arc::as_ptr(overlay) as usize);
        let layout = Some((areas, cursor, black, smooth, drawn_over));
        let forced = self.panel.0 != layout || self.panel.1 >= 5;
        self.panel = (layout, if forced { 0 } else { self.panel.1 + 1 });
        let presents = self.gpu_queue.as_ref().map(|queue| queue.presents.clone());
        if let Some(presents) = &presents {
            let mut tries = 0;
            while presents.load(Ordering::Acquire) > 0 {
                pause(&mut tries);
            }
            presents.fetch_add(1, Ordering::AcqRel);
        }
        self.gpu_work(&[], move |gpu, _, guest| {
            let state = Some((gpu.pictures, gpu.shown_address(0), gpu.shown_address(1)));
            // a halfway picture went up for something new, which goes up next
            let new = gpu.presented != state;
            let again = interpolate && std::mem::replace(&mut gpu.presented_new, new);
            if !forced && !new && !again {
                if let Some(presents) = presents {
                    presents.fetch_sub(1, Ordering::AcqRel);
                }
                return;
            }
            gpu.presented = state;
            gpu.shown += 1;
            let mut shown = [None, None];
            for (index, screen) in [zakuro_common::Screen::Top, zakuro_common::Screen::Bottom].into_iter().enumerate() {
                let Some(area) = areas[index] else { continue };
                let image = match screen_ref(gpu, guest, screen, black) {
                    Some(found) => zakuro_gpu::PanelImage::Gpu(found),
                    None => zakuro_gpu::PanelImage::Pixels(screen_pixels(gpu, guest, screen, black), screen.width(), screen.height()),
                };
                shown[index] = Some((image, area));
            }
            gpu.present_panel(shown, cursor, overlay, smooth, interpolate);
            if let Some(presents) = presents {
                presents.fetch_sub(1, Ordering::AcqRel);
            }
        });
    }

    /// reads one screen into a straight RGBA8 buffer for presentation.
    pub fn read_screen(&mut self, screen: zakuro_common::Screen) -> Vec<u8> {
        self.wait_gpu();
        let mut guest = GuestMemory { linear_base: self.kernel.linear_base, memory: &mut self.memory };
        screen_pixels(&mut self.gpu, &mut guest, screen, self.lcd_force_black)
    }

    /// a one-line summary for the window title and the log.
    pub fn status_line(&self) -> String {
        let mut line = format!(
            "frame {} | {} threads | {} modules | {} draws | {} transfers | {} fills | gpu {:.1?}",
            self.frames,
            self.kernel.live_thread_count(),
            self.cro.len(),
            self.gpu.draw_calls,
            self.gpu.transfers,
            self.gpu.fills,
            self.gpu.busy,
        );
        if let Some(library) = &self.recompiled {
            // the fallbacks ran inside recompiled code, which counted them
            let interpreted = self.interpreted_instructions + library.fallbacks();
            let total = (self.interpreted_instructions + self.recompiled_instructions).max(1);
            line += &format!(" | interpreted {:.2}%", interpreted as f64 * 100.0 / total as f64);
        }
        line
    }
}

/// adapter letting the GPU reach guest memory by physical address.
struct GuestMemory<'a> {
    memory: &'a mut Memory,
    linear_base: u32,
}

impl GpuMemory for GuestMemory<'_> {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        self.memory.read_bytes_quiet(addr, out);
    }

    fn write(&mut self, addr: u32, data: &[u8]) {
        self.memory.write_bytes_quiet(addr, data);
    }

    fn read_u32(&mut self, addr: u32) -> u32 {
        let mut word = [0u8; 4];
        self.memory.read_bytes_quiet(addr, &mut word);
        u32::from_le_bytes(word)
    }

    fn slice(&mut self, addr: u32, len: usize) -> Option<&[u8]> {
        self.slice_mut(addr, len).map(|slice| &*slice)
    }

    fn slice_mut(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        // the linear heap and VRAM are physical memory in order, so what
        // translate made of a physical address leads back to it
        let physical = if (VRAM_VADDR..VRAM_VADDR + VRAM_SIZE).contains(&addr) {
            VRAM_PADDR + (addr - VRAM_VADDR)
        } else if addr >= self.linear_base && addr - self.linear_base < FCRAM_SIZE_NEW3DS {
            FCRAM_PADDR + (addr - self.linear_base)
        } else {
            return None;
        };
        self.memory.phys.host_slice_mut(physical, u32::try_from(len).ok()?)
    }

    fn translate(&self, paddr: u32) -> u32 {
        if paddr >= FCRAM_PADDR {
            self.linear_base + (paddr - FCRAM_PADDR)
        } else if (VRAM_PADDR..VRAM_PADDR + VRAM_SIZE).contains(&paddr) {
            VRAM_VADDR + (paddr - VRAM_PADDR)
        } else {
            paddr
        }
    }
}

/// with ZAKURO_INTERP_STATS, how often each address ran in the interpreter,
/// the busiest printed when the system goes away.
static INTERPRETED: std::sync::Mutex<Option<std::collections::HashMap<u32, u64>>> = std::sync::Mutex::new(None);

fn interpreted_at(address: u32) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("ZAKURO_INTERP_STATS").is_some()) {
        *INTERPRETED.lock().unwrap().get_or_insert_with(Default::default).entry(address).or_default() += 1;
    }
}

impl Drop for System {
    /// what the interpreter ran lasts past the run.
    fn drop(&mut self) {
        if let Some(counts) = INTERPRETED.lock().unwrap().take() {
            let mut busiest: Vec<_> = counts.into_iter().collect();
            busiest.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
            for (address, n) in busiest.into_iter().take(40) {
                log::info!("interpreted {n:9} at {address:08X}");
            }
        }
        self.wait_gpu();
        if let Some(hints) = &mut self.hints {
            hints.save();
        }
    }
}

/// an RGBA picture of a given width grown by a whole factor, each pixel
/// repeated.
fn grow(pixels: &[u8], width: u32, scale: u32) -> Vec<u8> {
    let (width, scale) = (width as usize, scale as usize);
    let mut out = Vec::with_capacity(pixels.len() * scale * scale);
    for row in pixels.chunks_exact(width * 4) {
        let mut grown = Vec::with_capacity(row.len() * scale);
        for pixel in row.as_chunks::<4>().0 {
            for _ in 0..scale {
                grown.extend_from_slice(pixel);
            }
        }
        for _ in 0..scale {
            out.extend_from_slice(&grown);
        }
    }
    out
}

/// the picture the host's GPU drew for a screen, when it is still what the
/// screen shows.
fn screen_ref(gpu: &mut Gpu, guest: &mut GuestMemory, screen: zakuro_common::Screen, black: bool) -> Option<zakuro_gpu::ScreenRef> {
    let (width, height) = (screen.width(), screen.height());
    let index = match screen {
        zakuro_common::Screen::Top => 0,
        zakuro_common::Screen::Bottom => 1,
    };
    let config = gpu.framebuffers[index];
    let address = gpu.shown_address(index);
    let format = config.color_format();
    let bpp = format.bytes_per_pixel() as u32;
    // plain buffers a row per screen column, as display transfers leave
    // them, rows can be longer than the screen
    let stride = if config.stride == 0 { height * bpp } else { config.stride };
    if black || address == 0 || !stride.is_multiple_of(bpp) || stride < height * bpp {
        return None;
    }
    // no sync first, that would wait for the GPU to finish the picture.
    // read in place where guest memory is mapped whole
    let base = guest.translate(address);
    let len = (width * stride) as usize;
    let copied = guest.slice(base, len).is_none().then(|| {
        let mut bytes = vec![0u8; len];
        guest.read(base, &mut bytes);
        bytes
    });
    let bytes = match &copied {
        Some(bytes) => bytes.as_slice(),
        None => guest.slice(base, len)?,
    };
    gpu.scaled_screen(base, (height, width), stride / bpp, format, bytes)
}

/// one screen as a straight RGBA8 buffer.
fn screen_pixels(gpu: &mut Gpu, guest: &mut GuestMemory, screen: zakuro_common::Screen, black: bool) -> Vec<u8> {
    let index = match screen {
        zakuro_common::Screen::Top => 0,
        zakuro_common::Screen::Bottom => 1,
    };
    let config = gpu.framebuffers[index];
    let width = screen.width();
    let height = screen.height();
    let mut out = vec![0u8; (width * height * 4) as usize];

    let address = gpu.shown_address(index);
    if black || address == 0 {
        return out;
    }

    let format = config.color_format();
    let bpp = format.bytes_per_pixel();
    let stride = if config.stride == 0 {
        height * bpp as u32
    } else {
        config.stride
    };

    let base = guest.translate(address);
    let source_len = (stride * width) as usize;
    gpu.sync_memory_for(guest, base, source_len as u32, "sync to show a screen");
    let mut source = vec![0u8; source_len];
    guest.read(base, &mut source);

    for x in 0..width {
        for y in 0..height {
            // column-major in memory, and the panel scans bottom to top.
            let offset = (x * stride + (height - 1 - y) * bpp as u32) as usize;
            if offset + bpp > source.len() {
                continue;
            }
            let pixel = format.decode(&source[offset..offset + bpp]);
            let dst = ((y * width + x) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&pixel);
        }
    }
    out
}

/// a wait for the GPU thread, which yields a while and then sleeps, so a
/// long one does not keep a core busy, and hot, for nothing.
fn pause(tries: &mut u32) {
    *tries += 1;
    if *tries < 64 {
        std::thread::yield_now();
    } else {
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
}

type GpuJob = Box<dyn FnOnce() + Send>;

/// microseconds the CPU's thread spent waiting on the GPU thread.
pub static GPU_WAITED: AtomicU64 = AtomicU64::new(0);

/// how many times each thing asked for the GPU's drawings to be in memory,
/// logged every few hundred, for telling what stalls the GPU in a game.
fn count_sync(from: &'static str) {
    static COUNTS: std::sync::Mutex<Vec<(&'static str, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut counts = COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let total = match counts.iter_mut().find(|(name, _)| *name == from) {
        Some((_, count)) => {
            *count += 1;
            *count
        }
        None => {
            counts.push((from, 1));
            1
        }
    };
    if total == 1 || total.is_multiple_of(500) {
        log::info!("gpu syncs so far: {counts:?}");
    }
}

/// the GPU thread's queue, what it has not finished and the interrupts of
/// what it did.
struct GpuQueue {
    jobs: std::sync::mpsc::Sender<GpuJob>,
    pending: Arc<AtomicUsize>,
    done: Arc<std::sync::Mutex<Vec<services::gsp::InterruptId>>>,
    finished: Arc<AtomicUsize>,
    presents: Arc<AtomicUsize>,
}

/// what the GPU thread works on, handed over while the queue holds it.
struct Shared<T>(T);

// SAFETY: the queue's owner leaves what is shared alone until the GPU
// thread is done with it
unsafe impl<T> Send for Shared<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::object::KObject;
    use kernel::thread::ThreadStatus;
    use zakuro_cpu::Bus;

    fn mutex_state(system: &System, handle: u32) -> (Option<ThreadId>, u32) {
        let object = system.kernel.handles.resolve(handle).expect("a mutex handle");
        match system.kernel.objects.get(object) {
            Some(KObject::Mutex(mutex)) => (mutex.owner, mutex.lock_count),
            _ => panic!("not a mutex"),
        }
    }

    /// a holder that took a mutex twice, through svcCreateMutex locked and
    /// svcWaitSynchronization1, and a waiter blocked on it, with the holder
    /// back on the core.
    fn holder_and_waiter(system: &mut System) -> (ThreadId, ThreadId, u32) {
        let holder = system.kernel.create_thread("holder", 0x0010_0000, 0x1000_0000, 0, 0x30, 0);
        let waiter = system.kernel.create_thread("waiter", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(holder);
        system.map_tls_page(waiter);
        system.kernel.schedule(&mut system.cpu, 0);
        assert_eq!(system.kernel.current_thread, Some(holder));
        system.cpu.regs[1] = 1;
        kernel::svc::dispatch(system, 0x13);
        let handle = system.cpu.regs[1];
        let wait = |system: &mut System| {
            system.cpu.regs[0] = handle;
            system.cpu.regs[2] = u32::MAX;
            system.cpu.regs[3] = u32::MAX;
            kernel::svc::dispatch(system, 0x24);
        };
        wait(system);
        assert_eq!(mutex_state(system, handle), (Some(holder), 2));
        system.kernel.current_thread = Some(waiter);
        system.kernel.thread_mut(waiter).status = ThreadStatus::Running;
        system.kernel.thread_mut(holder).status = ThreadStatus::Ready;
        wait(system);
        assert_eq!(system.kernel.thread(waiter).status, ThreadStatus::WaitSync);
        system.kernel.current_thread = Some(holder);
        system.kernel.thread_mut(holder).status = ThreadStatus::Running;
        system.kernel.reschedule_pending = false;
        (holder, waiter, handle)
    }

    /// the holder ended with the mutex all given back, and the waiter gets it
    /// once at the next schedule.
    fn waiter_gets_it(system: &mut System, (holder, waiter, handle): (ThreadId, ThreadId, u32)) {
        assert_eq!(system.kernel.thread(holder).status, ThreadStatus::Dead);
        assert_eq!(mutex_state(system, handle), (None, 0));
        let tick = system.cpu.cycles;
        system.kernel.schedule(&mut system.cpu, tick);
        assert_eq!(system.kernel.current_thread, Some(waiter), "{}", system.kernel.describe_wait(waiter));
        assert!(matches!(system.kernel.thread(waiter).wait_result, Some(WaitResult::Signaled(0))));
        assert_eq!(mutex_state(system, handle), (Some(waiter), 1));
    }

    #[test]
    fn svc_exit_thread_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        kernel::svc::dispatch(&mut system, 0x09);
        waiter_gets_it(&mut system, threads);
    }

    #[test]
    fn returning_from_the_entry_point_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        system.cpu.regs[15] = THREAD_EXIT_MAGIC;
        assert_eq!(system.step(None), StepOutcome::Ran);
        waiter_gets_it(&mut system, threads);
    }

    #[test]
    fn a_branch_to_unmapped_memory_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        system.cpu.regs[15] = 0x0000_1000;
        assert!(!system.memory.is_executable(0x0000_1000));
        assert_eq!(system.step(None), StepOutcome::Ran);
        waiter_gets_it(&mut system, threads);
    }

    /// a thread whose timed arbiter wait ran out, and that then ended, left
    /// its entry on the arbiter. a signal for one waiter on the address
    /// passes over it to the thread really waiting there, and the dead one
    /// stays dead.
    #[test]
    fn an_arbiter_signal_wakes_only_threads_still_waiting() {
        let mut system = System::new(Config::default());
        let ended = system.kernel.create_thread("ended", 0x0010_0000, 0x1000_0000, 0, 0x30, 0);
        let signaller = system.kernel.create_thread("signaller", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        let parked = system.kernel.create_thread("parked", 0x0010_0000, 0x0FE0_0000, 0, 0x30, 0);
        for id in [ended, signaller, parked] {
            system.map_tls_page(id);
        }
        system.kernel.schedule(&mut system.cpu, 0);
        assert_eq!(system.kernel.current_thread, Some(ended));
        kernel::svc::dispatch(&mut system, 0x21);
        let arbiter = system.cpu.regs[1];
        let address = system.kernel.thread(ended).tls + 0x100;
        system.memory.write32(address, 0);
        let arbitrate = |system: &mut System, who: ThreadId, kind: u32| {
            system.kernel.current_thread = Some(who);
            system.kernel.thread_mut(who).status = ThreadStatus::Running;
            system.cpu.regs[0] = arbiter;
            system.cpu.regs[1] = address;
            system.cpu.regs[2] = kind;
            system.cpu.regs[3] = 1;
            system.cpu.regs[4] = 1000;
            kernel::svc::dispatch(system, 0x22);
        };
        // waits if less than 1, for a microsecond, and times out
        arbitrate(&mut system, ended, 3);
        system.kernel.current_thread = None;
        system.kernel.schedule(&mut system.cpu, 1_000_000);
        assert!(matches!(system.kernel.thread(ended).wait_result, Some(WaitResult::TimedOut)));
        // then ends
        system.kernel.current_thread = Some(ended);
        system.kernel.thread_mut(ended).status = ThreadStatus::Running;
        kernel::svc::dispatch(&mut system, 0x09);
        // another waits for good, and a third signals one waiter
        arbitrate(&mut system, parked, 1);
        assert_eq!(system.kernel.thread(parked).status, ThreadStatus::WaitArbiter);
        arbitrate(&mut system, signaller, 0);
        assert_eq!(system.kernel.thread(ended).status, ThreadStatus::Dead);
        assert_eq!(system.kernel.thread(parked).status, ThreadStatus::Ready);
    }

    #[test]
    fn timed_out_arbiter_waits_leave_one_entry_a_thread() {
        let mut system = System::new(Config::default());
        let waiter = system.kernel.create_thread("waiter", 0x0010_0000, 0x1000_0000, 0, 0x30, 0);
        system.map_tls_page(waiter);
        system.kernel.schedule(&mut system.cpu, 0);
        kernel::svc::dispatch(&mut system, 0x21);
        let arbiter = system.cpu.regs[1];
        let address = system.kernel.thread(waiter).tls + 0x100;
        system.memory.write32(address, 0);
        for round in 1..=50 {
            system.kernel.current_thread = Some(waiter);
            system.kernel.thread_mut(waiter).status = ThreadStatus::Running;
            system.cpu.regs[..5].copy_from_slice(&[arbiter, address, 3, 1, 1000]);
            kernel::svc::dispatch(&mut system, 0x22);
            system.kernel.current_thread = None;
            system.kernel.schedule(&mut system.cpu, round * 1_000_000);
        }
        let Some(KObject::AddressArbiter(arbiter)) = system.kernel.resolve(arbiter).and_then(|object| system.kernel.objects.get(object)) else {
            panic!("no arbiter");
        };
        assert_eq!(arbiter.waiters.len(), 1);
    }
}
