//! R52 opt-in Linux64 ALSA raw phase observations, distinct from R40 schema.
//! No device/DAC clock, playback/drain/quality oracle, or pacing decisions.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

type State = unsafe extern "C" fn(*mut u8) -> i32;
type Available = unsafe extern "C" fn(*mut u8) -> libc::c_long;
type Params = unsafe extern "C" fn(*mut u8, *mut libc::c_ulong, *mut libc::c_ulong) -> i32;
type Delay = unsafe extern "C" fn(*mut u8, *mut i64) -> i32;
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PHASES: AtomicU64 = AtomicU64::new(0);
const LINE_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase { Opened, BeforeSoundThreadExit }
impl Phase {
    fn slot(self) -> usize { match self { Self::Opened => 0, Self::BeforeSoundThreadExit => 1 } }
    fn name(self) -> &'static str { match self { Self::Opened => "opened", Self::BeforeSoundThreadExit => "before_sound_thread_exit" } }
}
fn class(raw: i64) -> &'static str {
    if raw == -i64::from(libc::EPIPE) { "EPIPE" }
    else if raw == -i64::from(libc::ESTRPIPE) { "ESTRPIPE" }
    else if raw == -i64::from(libc::EINTR) { "EINTR" }
    else if raw == -i64::from(libc::EAGAIN) { "EAGAIN" }
    else if raw < 0 { "other_negative" }
    else if raw == 0 { "zero" } else { "positive" }
}
fn add(value: &mut u64, amount: u64, overflow: &mut bool) {
    if let Some(next) = value.checked_add(amount) { *value = next; } else { *overflow = true; }
}
fn next_generation(counter: &AtomicU64) -> Option<u64> {
    counter.fetch_update(Relaxed, Relaxed, |value| value.checked_add(1)).ok()?.checked_add(1)
}
fn phase_ticket(counter: &AtomicU64) -> Option<u64> {
    counter.fetch_update(Relaxed, Relaxed, |value| value.checked_add(1).filter(|next| *next <= 2)).ok()?.checked_add(1)
}
#[derive(Default, Debug)]
struct Counts {
    calls: u64, offered: u64, accepted: u64, zero: u64, invalid_positive: u64,
    errors: [u64; 5], recovery_calls: u64, recovery_ok: u64, recovery_failed: u64,
    last_write: Option<i64>, last_recovery_input: Option<i32>, last_recovery_return: Option<i32>,
    overflow: bool,
}
impl Counts {
    fn returned(&mut self, offered: u64, raw: i64) {
        self.last_write = Some(raw);
        add(&mut self.calls, 1, &mut self.overflow); add(&mut self.offered, offered, &mut self.overflow);
        if raw < 0 {
            let slot = match class(raw) { "EPIPE" => 0, "ESTRPIPE" => 1, "EINTR" => 2, "EAGAIN" => 3, _ => 4 };
            add(&mut self.errors[slot], 1, &mut self.overflow);
        } else if raw == 0 { add(&mut self.zero, 1, &mut self.overflow); }
        else if raw as u64 > offered { add(&mut self.invalid_positive, 1, &mut self.overflow); }
        else { add(&mut self.accepted, raw as u64, &mut self.overflow); }
    }
    fn recovered(&mut self, input: i32, raw: i32) {
        self.last_recovery_input = Some(input); self.last_recovery_return = Some(raw);
        add(&mut self.recovery_calls, 1, &mut self.overflow);
        if raw < 0 { add(&mut self.recovery_failed, 1, &mut self.overflow); }
        else { add(&mut self.recovery_ok, 1, &mut self.overflow); }
    }
}
#[derive(Clone, Copy, Debug)]
struct Clock { rc: i32, seconds: i64, nanos: i64, ns: Option<u64> }
impl Clock {
    fn parts(rc: i32, seconds: i64, nanos: i64) -> Self {
        let ns = (rc == 0 && (0..1_000_000_000).contains(&nanos)).then_some(())
            .and_then(|()| u64::try_from(seconds).ok())
            .and_then(|seconds| seconds.checked_mul(1_000_000_000))
            .and_then(|seconds| seconds.checked_add(nanos as u64));
        Self { rc, seconds, nanos, ns }
    }
    fn now() -> Self {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: initialized writable timespec, whose status/parts stay separate.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
        Self::parts(rc, time.tv_sec, time.tv_nsec)
    }
}
#[derive(Clone, Copy, Debug)]
struct Raw {
    begin: Clock, end: Clock, state_before: Option<i32>, state_after: Option<i32>,
    available: Option<i64>, delay_rc: i32, delay_frames: i64,
    params_rc: Option<i32>, buffer: u64, period: u64,
}
impl Raw {
    fn state_valid(state: Option<i32>) -> bool { state.is_some_and(|value| (0..=8).contains(&value)) }
    fn availability_valid(&self) -> bool { self.available.is_some_and(|value| value >= 0) }
    fn delay_valid(&self) -> bool { self.delay_rc == 0 }
    fn params_valid(&self) -> bool { self.params_rc == Some(0) }
    fn clock_valid(&self) -> bool { self.begin.ns.zip(self.end.ns).is_some_and(|(begin, end)| begin <= end) }
}
pub(super) struct Observer {
    run: String, generation: Option<u64>, emitted: [bool; 2], counts: Counts,
    state: Option<State>, available: Option<Available>, params: Option<Params>,
}
impl Observer {
    /// Runtime opt-in is strict; absence/malformed configuration leaves no observer.
    /// Missing optional symbols never change success of the original ALSA open.
    pub(super) fn configured(library: &libloading::Library) -> Option<Self> {
        if std::env::var("ZAKURO_AUDIO_PROGRESS_OBSERVER").ok().as_deref() != Some("1") { return None; }
        let run = std::env::var("ZAKURO_AUDIO_PROGRESS_RUN_ID").ok()?;
        if run.is_empty() || run.len() > 80 || !run.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') { return None; }
        // SAFETY: existing libasound lifetime exceeds Observer; Linux64 ALSA
        // ABI types match its enum/int, signed long frames and unsigned long.
        let (state, available, params) = unsafe {
            (library.get::<State>(b"snd_pcm_state\0").ok().map(|entry| *entry),
             library.get::<Available>(b"snd_pcm_avail_update\0").ok().map(|entry| *entry),
             library.get::<Params>(b"snd_pcm_get_params\0").ok().map(|entry| *entry))
        };
        Some(Self { run, generation: next_generation(&GENERATION), emitted: [false; 2], counts: Counts::default(), state, available, params })
    }
    pub(super) fn returned(&mut self, offered: u64, raw: i64) { self.counts.returned(offered, raw); }
    pub(super) fn recovered(&mut self, input: i32, raw: i32) { self.counts.recovered(input, raw); }
    /// Two ordinary sound-thread phases, no draw/frame/queued sampling hook.
    /// All calls are observations on the same live PCM. None invokes recovery.
    pub(super) unsafe fn observe(&mut self, phase: Phase, pcm: *mut u8, delay: Delay) {
        let slot = phase.slot(); if self.emitted[slot] { return; } self.emitted[slot] = true;
        // Global cap cannot reset on stream recreation. A missing/mismatched
        // generation pair stays UNKNOWN; it does not grant extra observations.
        if phase_ticket(&PHASES).is_none() { return; }
        let begin = Clock::now();
        let state_before = self.state.map(|state| unsafe { state(pcm) });
        let available = self.available.map(|available| unsafe { available(pcm) as i64 });
        let mut delay_frames = 0;
        let delay_rc = unsafe { delay(pcm, &mut delay_frames) };
        let (mut buffer, mut period) = (0, 0);
        let params_rc = self.params.map(|params| unsafe { params(pcm, &mut buffer, &mut period) });
        let state_after = self.state.map(|state| unsafe { state(pcm) });
        let raw = Raw { begin, end: Clock::now(), state_before, state_after, available,
            delay_rc, delay_frames, params_rc, buffer, period };
        let line = self.render(phase, &raw);
        // One bounded log record per named phase, captured by the existing
        // root-pinned runtime log. Missing records are UNKNOWN, never finality.
        if line.len() <= LINE_LIMIT { log::info!("{line}"); }
    }
    fn render(&self, phase: Phase, raw: &Raw) -> String {
        fn n<T: std::fmt::Display>(value: Option<T>) -> String { value.map_or_else(|| "unknown".to_owned(), |value| value.to_string()) }
        fn c(value: Option<i64>) -> &'static str { value.map_or("unknown", class) }
        let counts = &self.counts;
        format!(concat!(
            "r52_alsa_phase_v1 run_id={} pid={} stream_generation={} phase={} ",
            "frame_units=client_pcm_frames configured_pcm_rate_hz={} device_rate_hz=unknown ",
            "begin_clock_rc={} begin_seconds_raw={} begin_nanos_raw={} begin_monotonic_ns={} ",
            "end_clock_rc={} end_seconds_raw={} end_nanos_raw={} end_monotonic_ns={} clock_valid={} ",
            "state_before_raw={} state_before_valid={} state_after_raw={} state_after_valid={} ",
            "available_frames_raw={} available_valid={} available_class={} ",
            "delay_rc={} delay_class={} delay_frames_raw={} delay_valid={} ",
            "params_rc={} params_class={} buffer_frames_raw={} period_frames_raw={} params_valid={} ",
            "write_calls_returned={} frames_offered_returned_calls={} frames_accepted_returned={} zero_returns={} invalid_positive_returns={} ",
            "write_epipe={} write_estrpipe={} write_eintr={} write_eagain={} write_other_negative={} last_write_raw={} last_write_class={} ",
            "recovery_sequence={} recovery_nonnegative={} recovery_negative={} last_recovery_input={} last_recovery_return={} last_recovery_class={} ",
            "counts_overflow={} snapshot_scope=sequential_nonatomic_audio_thread_calls ",
            "available_is_delay=false delay_is_buffer_fill=false endpoint_progress=unknown ",
            "dac_clock=unknown underruns=unknown backend_drain=not_observed shutdown_finality=unknown physical_playback=unknown acoustic_quality=unknown"),
            self.run, std::process::id(), n(self.generation), phase.name(), super::AUDIO_SAMPLE_RATE.round() as u32,
            raw.begin.rc, raw.begin.seconds, raw.begin.nanos, n(raw.begin.ns), raw.end.rc, raw.end.seconds, raw.end.nanos, n(raw.end.ns), raw.clock_valid(),
            n(raw.state_before), Raw::state_valid(raw.state_before), n(raw.state_after), Raw::state_valid(raw.state_after),
            n(raw.available), raw.availability_valid(), c(raw.available), raw.delay_rc, class(i64::from(raw.delay_rc)), raw.delay_frames, raw.delay_valid(),
            n(raw.params_rc), c(raw.params_rc.map(i64::from)), raw.buffer, raw.period, raw.params_valid(), counts.calls, counts.offered, counts.accepted, counts.zero, counts.invalid_positive,
            counts.errors[0], counts.errors[1], counts.errors[2], counts.errors[3], counts.errors[4], n(counts.last_write), c(counts.last_write),
            counts.recovery_calls, counts.recovery_ok, counts.recovery_failed, n(counts.last_recovery_input), n(counts.last_recovery_return), c(counts.last_recovery_return.map(i64::from)), counts.overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observer() -> Observer {
        Observer { run: "synthetic_only".to_owned(), generation: Some(1), emitted: [false; 2],
            counts: Counts::default(), state: None, available: None, params: None }
    }
    fn raw() -> Raw {
        Raw { begin: Clock::parts(0, 1, 1), end: Clock::parts(0, 1, 2),
            state_before: Some(3), state_after: Some(3), available: Some(400),
            delay_rc: 0, delay_frames: 123, params_rc: Some(0), buffer: 512, period: 64 }
    }
    #[test]
    fn equal_negative_totals_keep_epipe_and_eintr_distinct() {
        let mut pipe = Counts::default(); let mut interrupt = Counts::default();
        pipe.returned(64, -i64::from(libc::EPIPE)); interrupt.returned(64, -i64::from(libc::EINTR));
        assert_eq!(pipe.calls, interrupt.calls); assert_eq!(pipe.accepted, interrupt.accepted);
        assert_eq!(pipe.errors, [1, 0, 0, 0, 0]); assert_eq!(interrupt.errors, [0, 0, 1, 0, 0]);
    }
    #[test]
    fn suspend_retry_interrupt_and_unknown_negative_never_become_epipe() {
        let mut counts = Counts::default();
        for value in [-i64::from(libc::ESTRPIPE), -i64::from(libc::EAGAIN), -i64::from(libc::EINTR), i64::MIN] { counts.returned(64, value); }
        assert_eq!(counts.errors, [0, 1, 1, 1, 1]); assert_eq!(class(i64::MIN), "other_negative");
    }
    #[test]
    fn failed_delay_with_unchanged_zero_is_invalid_not_empty() {
        let mut sample = raw(); sample.delay_rc = -libc::EPIPE; sample.delay_frames = 0;
        assert!(!sample.delay_valid());
        let line = observer().render(Phase::BeforeSoundThreadExit, &sample);
        assert!(line.contains("delay_class=EPIPE delay_frames_raw=0 delay_valid=false"));
        assert!(line.contains("endpoint_progress=unknown"));
    }
    #[test]
    fn negative_successful_delay_is_preserved_without_sanitizing() {
        let mut sample = raw(); sample.delay_frames = -7; assert!(sample.delay_valid());
        assert!(observer().render(Phase::Opened, &sample).contains("delay_frames_raw=-7 delay_valid=true"));
    }
    #[test]
    fn available_and_delay_are_distinct_not_a_fill_equation() {
        let sample = raw(); assert_eq!(sample.available, Some(400)); assert_eq!(sample.delay_frames, 123);
        assert!(sample.availability_valid() && sample.delay_valid());
        let line = observer().render(Phase::Opened, &sample);
        assert!(line.contains("available_is_delay=false delay_is_buffer_fill=false"));
    }
    #[test]
    fn missing_symbols_and_failed_params_keep_raw_outputs_invalid() {
        let mut sample = raw(); sample.available = None; sample.state_before = None; sample.params_rc = None;
        sample.buffer = 0; sample.period = 0;
        let line = observer().render(Phase::Opened, &sample);
        assert!(line.contains("state_before_raw=unknown state_before_valid=false"));
        assert!(line.contains("available_frames_raw=unknown available_valid=false available_class=unknown"));
        assert!(line.contains("params_rc=unknown params_class=unknown buffer_frames_raw=0 period_frames_raw=0 params_valid=false"));
        sample.params_rc = Some(-libc::EINTR); assert!(!sample.params_valid());
        assert!(observer().render(Phase::Opened, &sample).contains("params_class=EINTR"));
    }
    #[test]
    fn raw_state_transitions_and_out_of_range_state_are_not_hidden() {
        let mut sample = raw(); sample.state_before = Some(4); sample.state_after = Some(7);
        let line = observer().render(Phase::Opened, &sample);
        assert!(line.contains("state_before_raw=4 state_before_valid=true state_after_raw=7 state_after_valid=true"));
        for state in [None, Some(-1), Some(9), Some(i32::MAX)] { assert!(!Raw::state_valid(state)); }
    }
    #[test]
    fn failed_availability_keeps_exact_errno_and_validity() {
        let mut sample = raw(); sample.available = Some(-i64::from(libc::ESTRPIPE));
        assert!(!sample.availability_valid());
        assert!(observer().render(Phase::Opened, &sample).contains("available_valid=false available_class=ESTRPIPE"));
    }
    #[test]
    fn partial_zero_and_impossible_positive_returns_do_not_invent_acceptance() {
        let mut counts = Counts::default();
        for result in [5, 0, 11, -i64::from(libc::EAGAIN)] { counts.returned(10, result); }
        assert_eq!((counts.calls, counts.offered, counts.accepted, counts.zero, counts.invalid_positive), (4, 40, 5, 1, 1));
    }
    #[test]
    fn recovery_input_and_raw_result_are_separate_from_write_class() {
        let mut observer = observer(); observer.returned(64, -i64::from(libc::EPIPE));
        observer.recovered(-libc::EPIPE, -libc::EAGAIN);
        assert_eq!((observer.counts.recovery_calls, observer.counts.recovery_ok, observer.counts.recovery_failed), (1, 0, 1));
        let line = observer.render(Phase::Opened, &raw());
        assert!(line.contains("last_write_class=EPIPE")); assert!(line.contains("last_recovery_class=EAGAIN"));
        observer.recovered(-libc::EINTR, 0);
        assert_eq!((observer.counts.recovery_calls, observer.counts.recovery_ok, observer.counts.recovery_failed), (2, 1, 1));
    }
    #[test]
    fn accepted_pcm_and_valid_observations_never_promote_endpoint_or_quality() {
        let mut observer = observer(); observer.returned(64, 64);
        let line = observer.render(Phase::BeforeSoundThreadExit, &raw());
        for required in ["frames_accepted_returned=64", "endpoint_progress=unknown", "dac_clock=unknown", "backend_drain=not_observed", "shutdown_finality=unknown", "physical_playback=unknown", "acoustic_quality=unknown", "underruns=unknown"] { assert!(line.contains(required)); }
    }
    #[test]
    fn recreated_stream_and_recovery_sequences_keep_distinct_identity() {
        let counter = AtomicU64::new(0);
        let mut first = observer(); first.generation = next_generation(&counter);
        let mut second = observer(); second.generation = next_generation(&counter);
        first.recovered(-libc::EPIPE, 0);
        assert_ne!(first.generation, second.generation); assert_eq!(second.counts.recovery_calls, 0);
        assert!(first.render(Phase::Opened, &raw()).contains("stream_generation=1"));
        assert!(second.render(Phase::Opened, &raw()).contains("stream_generation=2"));
    }
    #[test]
    fn generation_and_counter_overflow_are_sticky_not_wrapped_identity() {
        let counter = AtomicU64::new(u64::MAX); assert_eq!(next_generation(&counter), None);
        assert_eq!(counter.load(Relaxed), u64::MAX);
        let mut observer = observer(); observer.generation = None; observer.counts.accepted = u64::MAX;
        observer.returned(1, 1); observer.returned(1, 1);
        assert!(observer.counts.overflow); assert_eq!(observer.counts.accepted, u64::MAX);
        let line = observer.render(Phase::Opened, &raw());
        assert!(line.contains("stream_generation=unknown")); assert!(line.contains("counts_overflow=true"));
    }
    #[test]
    fn monotonic_parts_errors_backwards_and_overflow_stay_invalid() {
        assert_eq!(Clock::parts(0, 2, 3).ns, Some(2_000_000_003));
        for parts in [(-1, 2, 3), (0, -1, 0), (0, 1, -1), (0, 1, 1_000_000_000), (0, i64::MAX, 1)] {
            assert_eq!(Clock::parts(parts.0, parts.1, parts.2).ns, None);
        }
        let mut sample = raw(); sample.end = Clock::parts(0, 0, 0); assert!(!sample.clock_valid());
        sample.end = Clock::parts(-1, 0, 0); assert!(!sample.clock_valid());
    }
    #[test]
    fn maximum_values_stay_one_bounded_schema_line() {
        let mut observer = observer(); observer.run = "x".repeat(80); observer.generation = Some(u64::MAX);
        observer.counts.calls = u64::MAX; observer.counts.offered = u64::MAX; observer.counts.accepted = u64::MAX;
        observer.counts.errors = [u64::MAX; 5]; observer.counts.last_write = Some(i64::MIN);
        let line = observer.render(Phase::BeforeSoundThreadExit, &raw());
        assert!(line.starts_with("r52_alsa_phase_v1 ")); assert!(line.len() <= LINE_LIMIT); assert!(!line.contains('\n'));
        assert_eq!(Phase::Opened.slot(), 0); assert_eq!(Phase::BeforeSoundThreadExit.slot(), 1);
    }
    #[test]
    fn process_phase_cap_cannot_reset_with_stream_recreation() {
        let counter = AtomicU64::new(0);
        assert_eq!(phase_ticket(&counter), Some(1)); assert_eq!(phase_ticket(&counter), Some(2));
        assert_eq!(phase_ticket(&counter), None); assert_eq!(counter.load(Relaxed), 2);
        let overflow = AtomicU64::new(u64::MAX); assert_eq!(phase_ticket(&overflow), None);
        assert_eq!(overflow.load(Relaxed), u64::MAX);
    }
}
