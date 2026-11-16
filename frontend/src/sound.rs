//! the title's sound through ALSA, or AAudio on Android, stretched in time
//! to keep it a twentieth to a tenth of a second behind the picture whatever
//! the frame rate, counting all of it not yet heard, and leaving some out to
//! catch up at once when it falls far behind.

use std::sync::mpsc;

use zakuro_core::AUDIO_SAMPLE_RATE;

use crate::settings::FastForwardAudio;

/// where the sound goes: how many frames it holds that have not played yet,
/// and taking more, all of them before it returns.
trait Device {
    /// frames queued, at the title's rate.
    fn queued(&mut self) -> i64;
    fn write(&mut self, frames: &[[i16; 2]]);
    /// how much to keep queued, in seconds, which is about how far the
    /// sound lags the picture.
    fn target(&self) -> f64 {
        0.1
    }
    /// times the device ran dry so far, where it counts them.
    fn underruns(&self) -> Option<i32> {
        None
    }
    /// past how far behind, in seconds, the oldest sound is left out, and
    /// down to how far.
    fn catch_up(&self) -> (f64, f64) {
        (self.target() * 4.0, self.target())
    }
    /// stops while no sound comes, the game paused, or starts again.
    fn idle(&mut self, _idle: bool) {}
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Fast(FastForwardAudio),
}

impl Mode {
    pub fn new(fast: bool, audio: FastForwardAudio) -> Self {
        if fast { Self::Fast(audio) } else { Self::Normal }
    }
}

pub enum Message {
    Samples(Vec<[i16; 2]>),
    /// Drop queued pre-transition audio and the stretcher's history.
    Mode(Mode),
}

pub fn play(samples: mpsc::Receiver<Message>) {
    std::thread::spawn(move || {
        let Some(mut device) = open() else {
            for _ in samples {}
            return;
        };
        let mut pacer = Pacer::new(device.target(), device.catch_up());
        let mut since_log = std::time::Instant::now();
        loop {
            // half a second with nothing, the game paused or the app away,
            // and the device stops until sound comes again
            let first = match samples.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(first) => first,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    device.idle(true);
                    match samples.recv() {
                        Ok(first) => {
                            device.idle(false);
                            first
                        }
                        Err(_) => break,
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // everything waiting, taken at once, so that how far the sound is
            // behind counts it: left in the channel it would pile up unseen
            let input = pacer.collect(std::iter::once(first).chain(samples.try_iter()));
            if input.is_empty() {
                continue;
            }
            let behind = pacer.take(device.as_mut(), &input);
            if since_log.elapsed() >= std::time::Duration::from_secs(30) {
                log::info!(
                    "sound {:.0} output ms behind, stretched {:.2} times, {} guest ms left out to catch up, {} guest ms discarded on speed changes, underruns {}",
                    behind / AUDIO_SAMPLE_RATE * 1000.0,
                    pacer.ratio,
                    (std::mem::take(&mut pacer.dropped) as f64 / AUDIO_SAMPLE_RATE * 1000.0) as u64,
                    (std::mem::take(&mut pacer.transition_dropped) as f64 / AUDIO_SAMPLE_RATE * 1000.0) as u64,
                    device.underruns().map_or_else(|| "not measured".into(), |count| format!("{count} so far"))
                );
                since_log = std::time::Instant::now();
            }
        }
    });
}

/// keeps the sound target seconds behind: stretches it longer while less is
/// queued and shorter while more is, and far behind leaves the oldest out.
struct Pacer {
    mode: Mode,
    stretch: Stretch,
    target: f64,
    /// past how many frames behind the oldest is left out, and down to how
    /// many.
    over: f64,
    to: f64,
    ratio: f64,
    /// Integral baseline for fast audio only, free to follow achieved
    /// throughput rather than assuming the requested 2x was reached.
    nominal: f64,
    chunk: Vec<[i16; 2]>,
    /// Guest frames skipped for catch-up since the last look; transition
    /// clears are counted separately below.
    dropped: usize,
    transition_dropped: usize,
}

impl Pacer {
    fn new(target_seconds: f64, (over, to): (f64, f64)) -> Pacer {
        let frames = |seconds: f64| AUDIO_SAMPLE_RATE * seconds;
        Pacer { mode: Mode::Normal, stretch: Stretch::default(), target: frames(target_seconds), over: frames(over), to: frames(to), ratio: 1.0, nominal: 1.0, chunk: Vec::new(), dropped: 0, transition_dropped: 0 }
    }

    /// Consume transitions in channel order, so no sound from before the
    /// last speed change survives in the batch or the WSOLA crossfade.
    fn collect(&mut self, messages: impl Iterator<Item = Message>) -> Vec<[i16; 2]> {
        let mut input = Vec::new();
        for message in messages {
            match message {
                Message::Samples(samples) => {
                    if input.is_empty() {
                        input = samples;
                    } else {
                        input.extend_from_slice(&samples);
                    }
                }
                Message::Mode(mode) => {
                    self.mode = mode;
                    log::info!("sound playback mode {mode:?}");
                    self.transition_dropped += input.len() + self.stretch.pending();
                    input.clear();
                    self.stretch = Stretch::default();
                    self.chunk.clear();
                    self.ratio = if mode == Mode::Normal { 1.0 } else { 0.5 };
                    self.nominal = self.ratio;
                }
            }
        }
        input
    }

    /// takes the title's newest sound to the device, and says how many of
    /// output frames were queued or pending before it went (at normal
    /// speed, pending input retains the original accounting).
    fn take(&mut self, device: &mut dyn Device, input: &[[i16; 2]]) -> f64 {
        self.stretch.feed(input);
        // the sound not yet heard: in the device, and here, not stretched yet
        // Pending guest samples take fewer output samples during fast
        // playback; count both sides in the device's time units.
        let pending_ratio = if self.mode == Mode::Normal { 1.0 } else { self.ratio };
        let queued = device.queued() as f64;
        let behind = queued + self.stretch.pending() as f64 * pending_ratio;
        // far behind, after a stall or with a device that holds less than it
        // should, the oldest is left out to catch up at once rather than be
        // stretched away over minutes
        if behind > self.over {
            self.dropped += self.stretch.skip(((behind - self.to) / pending_ratio) as usize);
        }
        let wanted = if self.mode == Mode::Normal {
            // Preserve normal-speed control, including its original units.
            (1.0 + (self.target - behind) / self.target).clamp(0.75, 2.5)
        } else {
            // Include input ready to become output, but not the WSOLA
            // lookahead. Counting lookahead steals the device's margin;
            // ignoring ready input winds up when a small device blocks.
            // Re-read pending after any catch-up skip above.
            let lookahead = if self.mode == Mode::Fast(FastForwardAudio::SpedUp) { 1 } else { SEGMENT + SEEK };
            let ready = self.stretch.pending().saturating_sub(lookahead) as f64 * self.ratio;
            // Keep an output-burst safety margin. WSOLA's lookahead makes
            // this approximate: Normal pitch can sit above the target.
            let burst = if self.mode == Mode::Fast(FastForwardAudio::SpedUp) {
                input.len() as f64 * self.ratio
            } else { HOP as f64 };
            let margin = (burst / 2.0).min(self.target / 2.0);
            let error = (self.target + margin - queued - ready) / self.target;
            // Integrate per estimated output second, capped across stalls.
            // The baseline converges to 1 / achieved_speed, removing the
            // proportional-only error at every speed. Clamp for anti-windup.
            let seconds = (input.len() as f64 * self.ratio / AUDIO_SAMPLE_RATE).min(0.1);
            self.nominal = (self.nominal + error * seconds).clamp(0.25, 2.5);
            (self.nominal + error).clamp(0.25, 2.5)
        };
        let smoothing = if self.mode == Mode::Normal { 0.1 } else {
            // Respond within the device's safety buffer when achieved
            // speed drops suddenly, independently of input batch size.
            let seconds = input.len() as f64 * self.ratio / AUDIO_SAMPLE_RATE;
            1.0 - (-seconds / 0.025).exp()
        };
        self.ratio += (wanted - self.ratio) * smoothing;
        self.chunk.clear();
        match self.mode {
            Mode::Fast(FastForwardAudio::SpedUp) => self.stretch.resample(self.ratio, &mut self.chunk),
            _ => self.stretch.stretch(self.ratio, &mut self.chunk),
        }
        device.write(&self.chunk);
        behind
    }
}

#[cfg(not(target_os = "android"))]
fn open() -> Option<Box<dyn Device>> {
    Alsa::open().map(|alsa| Box::new(alsa) as Box<dyn Device>)
}

#[cfg(target_os = "android")]
fn open() -> Option<Box<dyn Device>> {
    AAudio::open().map(|aaudio| Box::new(aaudio) as Box<dyn Device>)
}

/// the sound through ALSA's default device, from libasound when the system
/// has it.
#[cfg(not(target_os = "android"))]
struct Alsa {
    _library: libloading::Library,
    pcm: *mut u8,
    write: unsafe extern "C" fn(*mut u8, *const u8, u64) -> i64,
    recover: unsafe extern "C" fn(*mut u8, i32, i32) -> i32,
    delay: unsafe extern "C" fn(*mut u8, *mut i64) -> i32,
}

#[cfg(not(target_os = "android"))]
impl Alsa {
    fn open() -> Option<Alsa> {
        // SAFETY: plain ALSA calls on the device this opens, with buffers
        // that live as long as each call, and functions kept no longer than
        // the library they came from
        unsafe {
            let Ok(alsa) = libloading::Library::new("libasound.so.2") else {
                log::warn!("no libasound, playing without sound");
                return None;
            };
            let open: libloading::Symbol<unsafe extern "C" fn(*mut *mut u8, *const u8, i32, i32) -> i32> = alsa.get(b"snd_pcm_open\0").ok()?;
            let set: libloading::Symbol<unsafe extern "C" fn(*mut u8, i32, i32, u32, u32, i32, u32) -> i32> = alsa.get(b"snd_pcm_set_params\0").ok()?;
            let write = *alsa.get::<unsafe extern "C" fn(*mut u8, *const u8, u64) -> i64>(b"snd_pcm_writei\0").ok()?;
            let recover = *alsa.get::<unsafe extern "C" fn(*mut u8, i32, i32) -> i32>(b"snd_pcm_recover\0").ok()?;
            let delay = *alsa.get::<unsafe extern "C" fn(*mut u8, *mut i64) -> i32>(b"snd_pcm_delay\0").ok()?;
            let mut pcm = std::ptr::null_mut();
            if open(&mut pcm, c"default".as_ptr().cast(), 0, 0) < 0 || set(pcm, 2, 3, 2, AUDIO_SAMPLE_RATE.round() as u32, 1, 150_000) < 0 {
                log::warn!("could not open the sound device, playing without sound");
                return None;
            }
            if let Ok(get) = alsa.get::<unsafe extern "C" fn(*mut u8, *mut u64, *mut u64) -> i32>(b"snd_pcm_get_params\0") {
                let (mut buffer, mut period) = (0, 0);
                get(pcm, &mut buffer, &mut period);
                log::info!("sound at {} Hz, the device buffering {buffer} samples in periods of {period}", AUDIO_SAMPLE_RATE.round());
            }
            Some(Alsa { _library: alsa, pcm, write, recover, delay })
        }
    }
}

#[cfg(not(target_os = "android"))]
impl Device for Alsa {
    /// a bit more than a tenth of a second, which the RG52 Mini's device
    /// needs to play without gaps, as upstream found.
    fn target(&self) -> f64 {
        0.15
    }

    /// past a second behind, and down to a second, as the RG52 Mini's lag
    /// was fixed with
    fn catch_up(&self) -> (f64, f64) {
        (1.0, 1.0)
    }

    fn queued(&mut self) -> i64 {
        let mut queued = 0i64;
        // SAFETY: the device open made, and a number for ALSA to fill
        if unsafe { (self.delay)(self.pcm, &mut queued) } < 0 {
            queued = 0;
        }
        queued
    }

    fn write(&mut self, frames: &[[i16; 2]]) {
        let mut done = 0;
        while done < frames.len() {
            // SAFETY: the frames left, interleaved stereo as the device was set up
            let written = unsafe { (self.write)(self.pcm, frames[done..].as_ptr().cast(), (frames.len() - done) as u64) };
            if written < 0 {
                // SAFETY: as above
                if unsafe { (self.recover)(self.pcm, written as i32, 1) } < 0 {
                    break;
                }
            } else {
                done += written as usize;
            }
        }
    }
}

/// how much sound AAudio keeps queued, in seconds: a phone's scheduler
/// holds a thread up longer than a handheld's, and less ran the device dry.
#[cfg(target_os = "android")]
const AAUDIO_TARGET: f64 = 0.1;

/// the sound through AAudio at the device's own rate, which the title's is
/// resampled to here: a stream at the title's rate would have the system
/// resample it, and lose the low-latency path for it.
#[cfg(target_os = "android")]
struct AAudio {
    stream: ndk::audio::AudioStream,
    /// the device's frames to one of the title's.
    step: f64,
    /// where the resampler is between the last frame and the next.
    at: f64,
    last: [f32; 2],
    out: Vec<[i16; 2]>,
}

#[cfg(target_os = "android")]
impl AAudio {
    fn open() -> Option<AAudio> {
        use ndk::audio::{AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStreamBuilder};
        let opened = AudioStreamBuilder::new().and_then(|builder| {
            builder
                .direction(AudioDirection::Output)
                .format(AudioFormat::PCM_I16)
                .channel_count(2)
                .sharing_mode(AudioSharingMode::Shared)
                .performance_mode(AudioPerformanceMode::None)
                // room for twice the target at the usual 48 kHz
                .buffer_capacity_in_frames((AAUDIO_TARGET * 2.0 * 48_000.0) as i32)
                .open_stream()
        });
        let stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("could not open AAudio, playing without sound, {error:?}");
                return None;
            }
        };
        // room for twice what play keeps queued, in whole bursts: a buffer
        // smaller than that never holds the target, and play would stretch
        // the sound to fill it for ever
        let burst = stream.frames_per_burst().max(64);
        let rate = stream.sample_rate().max(1) as f64;
        let wanted = ((AAUDIO_TARGET * 2.0 * rate) as u32).div_ceil(burst as u32).max(2) as i32 * burst;
        // the size set comes back as an error from the ndk crate, which takes
        // any result but 0 for one, so it is read back instead
        let _ = stream.set_buffer_size_in_frames(wanted);
        let size = stream.buffer_size_in_frames();
        if let Err(error) = stream.request_start() {
            log::warn!("could not start AAudio, playing without sound, {error:?}");
            return None;
        }
        log::info!("sound through AAudio at {rate} Hz, {burst} frames a burst, a buffer of {size} of {} frames, {:?}", stream.buffer_capacity_in_frames(), stream.performance_mode());
        Some(AAudio { step: AUDIO_SAMPLE_RATE / rate as f64, stream, at: 0.0, last: [0.0; 2], out: Vec::new() })
    }
}

#[cfg(target_os = "android")]
impl Device for AAudio {
    fn queued(&mut self) -> i64 {
        let device = (self.stream.frames_written() - self.stream.frames_read()).max(0);
        (device as f64 * self.step) as i64
    }

    fn target(&self) -> f64 {
        AAUDIO_TARGET
    }

    fn underruns(&self) -> Option<i32> {
        Some(self.stream.x_run_count())
    }

    fn idle(&mut self, idle: bool) {
        let _ = if idle { self.stream.request_pause() } else { self.stream.request_start() };
    }

    fn write(&mut self, frames: &[[i16; 2]]) {
        // linear between the title's frames, at the device's rate
        self.out.clear();
        for frame in frames {
            let next = frame.map(f32::from);
            while self.at < 1.0 {
                let t = self.at as f32;
                self.out.push([0, 1].map(|channel| (self.last[channel] + (next[channel] - self.last[channel]) * t) as i16));
                self.at += self.step;
            }
            self.at -= 1.0;
            self.last = next;
        }
        let mut done = 0;
        while done < self.out.len() {
            // SAFETY: the frames left, interleaved stereo 16-bit as the stream
            // was opened, waiting at most a tenth of a second for room
            match unsafe { self.stream.write(self.out[done..].as_ptr().cast(), (self.out.len() - done) as i32, 100_000_000) } {
                Ok(0) | Err(_) => break,
                Ok(written) => done += written as usize,
            }
        }
    }
}

const SEGMENT: usize = 1024;
const OVERLAP: usize = 256;
const SEEK: usize = 192;
const HOP: usize = SEGMENT - OVERLAP;

/// stretches sound in time without changing its pitch, WSOLA: segments of
/// the input are laid one after another, each moved within a small window to
/// where it best continues the last and crossfaded into it.
#[derive(Default)]
struct Stretch {
    input: Vec<[f32; 2]>,
    at: f64,
    tail: Vec<[f32; 2]>,
}

impl Stretch {
    /// Change pitch along with duration, with a fractional input position
    /// carried across batches so their boundaries cannot add/drop samples.
    fn resample(&mut self, ratio: f64, out: &mut Vec<[i16; 2]>) {
        while (self.at as usize) + 1 < self.input.len() {
            let base = self.at as usize;
            let fraction = (self.at - base as f64) as f32;
            out.push([0, 1].map(|channel| {
                let a = self.input[base][channel];
                let b = self.input[base + 1][channel];
                (a + (b - a) * fraction).clamp(-32768.0, 32767.0) as i16
            }));
            self.at += 1.0 / ratio;
        }
        let used = (self.at as usize).min(self.input.len().saturating_sub(1));
        self.input.drain(..used);
        self.at -= used as f64;
    }

    /// takes more of the title's sound in.
    fn feed(&mut self, samples: &[[i16; 2]]) {
        self.input.extend(samples.iter().map(|&[l, r]| [l as f32, r as f32]));
    }

    /// frames taken in and not yet stretched out.
    fn pending(&self) -> usize {
        self.input.len().saturating_sub(self.at as usize)
    }

    /// leaves out up to frames of the oldest sound not yet stretched, and
    /// says how many it left out; the next segment crossfades over the gap.
    fn skip(&mut self, frames: usize) -> usize {
        let frames = frames.min(self.pending());
        self.at += frames as f64;
        frames
    }

    /// stretches what was taken in by ratio, longer above one, into out.
    fn stretch(&mut self, ratio: f64, out: &mut Vec<[i16; 2]>) {
        let step = HOP as f64 / ratio;
        loop {
            let base = self.at.round() as usize;
            if base + SEEK + SEGMENT > self.input.len() {
                break;
            }
            let mut best = (f32::MIN, base);
            if self.tail.is_empty() {
                best.1 = base;
            } else {
                for start in (base.saturating_sub(SEEK)..=base + SEEK).step_by(4) {
                    let score: f32 = (0..OVERLAP)
                        .step_by(2)
                        .map(|i| {
                            let (a, b) = (self.tail[i], self.input[start + i]);
                            (a[0] + a[1]) * (b[0] + b[1])
                        })
                        .sum();
                    if score > best.0 {
                        best = (score, start);
                    }
                }
            }
            let start = best.1;
            for i in 0..HOP {
                let sample = self.input[start + i];
                let mixed = if i < OVERLAP && !self.tail.is_empty() {
                    let w = i as f32 / OVERLAP as f32;
                    [self.tail[i][0] * (1.0 - w) + sample[0] * w, self.tail[i][1] * (1.0 - w) + sample[1] * w]
                } else {
                    sample
                };
                out.push(mixed.map(|value| value.clamp(-32768.0, 32767.0) as i16));
            }
            self.tail = self.input[start + HOP..start + SEGMENT].to_vec();
            self.at += step;
        }
        // Compression can advance past the newest input. Retain that
        // fractional position while waiting for the following batch.
        let used = (self.at as usize).saturating_sub(SEEK).min(self.input.len());
        if used > 0 {
            self.input.drain(..used);
            self.at -= used as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_changes_discard_old_audio_and_resume_with_only_fresh_samples() {
        let mut pacer = Pacer::new(0.1, (0.4, 0.1));
        pacer.stretch.feed(&vec![[7; 2]; 4000]);
        pacer.stretch.stretch(1.5, &mut pacer.chunk);
        assert!(!pacer.stretch.tail.is_empty());
        let pending_before = pacer.stretch.pending();
        let input = pacer.collect([
            Message::Samples(vec![[8; 2]; 1000]),
            Message::Mode(Mode::Fast(FastForwardAudio::SpedUp)), // enter fast-forward
            Message::Samples(vec![[8; 2]; 500]),
            Message::Mode(Mode::Normal), // leave fast-forward
            Message::Samples(vec![[9; 2]; 500]),
        ].into_iter());
        assert_eq!(input, vec![[9; 2]; 500]);
        assert_eq!(pacer.stretch.pending(), 0);
        assert!(pacer.stretch.tail.is_empty());
        assert!(pacer.chunk.is_empty());
        assert_eq!(pacer.ratio, 1.0);
        assert_eq!(pacer.transition_dropped, pending_before + 1000 + 500);
        assert_eq!(pacer.dropped, 0);
    }

    /// a device that plays at the title's rate in real time and holds at
    /// most capacity frames, its writes waiting for room as a real one's do,
    /// the clock moving only as it plays.
    struct Simulated {
        capacity: usize,
        queued: usize,
        /// frames played, which is the clock.
        played: usize,
        target_seconds: f64,
        /// Measure actual silence and time-weighted device depth, after startup.
        warmup: usize,
        measured: usize,
        starved: usize,
        queue_sum: f64,
        queue_min: usize,
    }

    impl Simulated {
        fn new(capacity: usize, target_seconds: f64) -> Self {
            Self { capacity, queued: 0, played: 0, target_seconds,
                warmup: (20.0 * AUDIO_SAMPLE_RATE) as usize, measured: 0,
                starved: 0, queue_sum: 0.0, queue_min: usize::MAX }
        }

        fn advance(&mut self, frames: usize) {
            let ignored = self.warmup.saturating_sub(self.played).min(frames);
            let measured = frames - ignored;
            let first = self.queued.saturating_sub(ignored);
            let audible = first.min(measured);
            self.starved += measured - audible;
            self.measured += measured;
            // Queue before each output sample is consumed: n + (n-1) + ...
            self.queue_sum += audible as f64 * (2.0 * first as f64 - audible as f64 + 1.0) / 2.0;
            if measured > 0 { self.queue_min = self.queue_min.min(first.saturating_sub(measured)); }
            self.queued = self.queued.saturating_sub(frames);
            self.played += frames;
        }
    }

    impl Device for Simulated {
        fn queued(&mut self) -> i64 {
            self.queued as i64
        }

        fn write(&mut self, frames: &[[i16; 2]]) {
            let mut left = frames.len();
            while left > 0 {
                let room = self.capacity - self.queued;
                if room == 0 {
                    // full: wait while a burst plays
                    let burst = 96.min(self.queued);
                    self.advance(burst);
                    continue;
                }
                let taken = room.min(left);
                self.queued += taken;
                left -= taken;
            }
        }

        fn target(&self) -> f64 {
            self.target_seconds
        }
    }

    /// minutes of a title making its sound on its own thread at speed
    /// times real time, a frame's worth at a time into a channel, while play
    /// takes it to a device of capacity frames: how far behind the sound
    /// ends, in seconds, counting what waits in the channel, in the pacer and
    /// in the device.
    fn run(capacity: usize, speed: f64, minutes: f64, all_at_once: bool, mode: Mode) -> (f64, usize) {
        let mut device = Simulated::new(capacity, 0.05);
        let mut pacer = Pacer::new(device.target(), device.catch_up());
        pacer.collect(std::iter::once(Message::Mode(mode)));
        let per_frame = (AUDIO_SAMPLE_RATE / 60.0) as usize;
        let tone: Vec<[i16; 2]> = (0..per_frame).map(|i| [((i as f32 * 0.07).sin() * 8000.0) as i16; 2]).collect();
        let mut channel: std::collections::VecDeque<Vec<[i16; 2]>> = Default::default();
        let mut made = 0usize;
        // the title, which makes each frame's sound when the clock, the
        // frames the device played, comes to it
        let mut catch_up = |played: usize, channel: &mut std::collections::VecDeque<Vec<[i16; 2]>>| {
            while ((made + per_frame) as f64) <= played as f64 * speed + per_frame as f64 {
                channel.push_back(tone.clone());
                made += per_frame;
            }
        };
        let end = (AUDIO_SAMPLE_RATE * 60.0 * minutes) as usize;
        let mut warmed_up = false;
        while device.played < end {
            if !warmed_up && device.played >= device.warmup {
                pacer.dropped = 0;
                warmed_up = true;
            }
            catch_up(device.played, &mut channel);
            let Some(first) = channel.pop_front() else {
                // nothing waiting: the device plays a burst meanwhile
                // The hardware clock keeps advancing through silence.
                device.advance(96);
                continue;
            };
            let mut input = first;
            if all_at_once {
                input.extend(channel.drain(..).flatten());
            }
            // the write waits for room, and the title goes on meanwhile
            pacer.take(&mut device, &input);
        }
        let waiting: usize = channel.iter().map(Vec::len).sum();
        ((waiting as f64 + pacer.stretch.pending() as f64 + device.queued as f64 / pacer.ratio) / AUDIO_SAMPLE_RATE, pacer.dropped)
    }

    /// a buffer too small to hold the target, as AAudio's was, must not
    /// leave the sound further and further behind.
    #[test]
    fn a_small_buffer_does_not_drift() {
        let (behind, _) = run(192, 1.0, 5.0, true, Mode::Normal);
        assert!(behind < 0.2, "{:.0} ms behind after five minutes", behind * 1000.0);
    }

    #[test]
    fn a_roomy_buffer_stays_near_the_target() {
        let (behind, _) = run(4096, 1.0, 5.0, true, Mode::Normal);
        assert!(behind < 0.2, "{:.0} ms behind after five minutes", behind * 1000.0);
    }

    /// a title running a little fast makes sound faster than it plays, which
    /// is squeezed or left out rather than piled up.
    #[test]
    fn a_fast_title_does_not_drift() {
        let (behind, _) = run(4096, 1.05, 5.0, true, Mode::Normal);
        assert!(behind < 0.25, "{:.0} ms behind after five minutes", behind * 1000.0);
    }

    #[test]
    fn fast_forward_audio_has_bounded_lag_and_no_steady_catchup_drops() {
        for audio in [FastForwardAudio::PreservePitch, FastForwardAudio::SpedUp] {
            for speed in [0.75, 1.0, 1.5, 2.0] {
                for capacity in [192, 4096] {
                    let (behind, dropped) = run(capacity, speed, 5.0, true, Mode::Fast(audio));
                    assert!(behind < 0.3, "{audio:?} at {speed}x: {behind}s behind");
                    assert_eq!(dropped, 0, "{audio:?} at {speed}x left sound out");
                }
            }
        }
    }

    /// Wall-clock input production continues even while writes block or
    /// the device runs dry. Each segment changes achieved speed and mode.
    fn clocked_run(target: f64, capacity_targets: f64, segments: &[(f64, Mode)], seconds: f64) -> (Simulated, usize, f64) {
        let mut device = Simulated::new((target * capacity_targets * AUDIO_SAMPLE_RATE) as usize, target);
        let mut pacer = Pacer::new(target, device.catch_up());
        let per_frame = (AUDIO_SAMPLE_RATE / 60.0) as usize;
        let tone: Vec<_> = (0..per_frame).map(|i| [((i as f32 * 0.07).sin() * 8000.0) as i16; 2]).collect();
        let mut made = 0;
        let mut segment = usize::MAX;
        // Catch-up counts use the same post-startup window as queue/silence.
        // Mode-transition discards stay separate and are not treated as gaps.
        let mut startup_dropped = None;
        while (device.played as f64) < seconds * segments.len() as f64 * AUDIO_SAMPLE_RATE {
            if startup_dropped.is_none() && device.played >= device.warmup { startup_dropped = Some(pacer.dropped); }
            let now = device.played as f64 / AUDIO_SAMPLE_RATE;
            let index = (now / seconds) as usize;
            if index >= segments.len() { break; }
            if index != segment {
                if segment == usize::MAX || segments[index].1 != segments[segment].1 {
                    pacer.collect(std::iter::once(Message::Mode(segments[index].1)));
                }
                segment = index;
            }
            let progress: f64 = segments[..index].iter().map(|(speed, _)| speed * seconds).sum::<f64>()
                + segments[index].0 * (now - index as f64 * seconds);
            let due = (progress * AUDIO_SAMPLE_RATE) as usize + per_frame;
            let mut input = Vec::new();
            while made + per_frame <= due {
                input.extend_from_slice(&tone);
                made += per_frame;
            }
            if input.is_empty() { device.advance(96); }
            else { pacer.take(&mut device, &input); }
        }
        (device, pacer.dropped - startup_dropped.unwrap_or(0), pacer.ratio)
    }

    #[test]
    fn fast_audio_maintains_device_queue_without_starvation() {
        let mut failures = Vec::new();
        for target in [0.05, 0.1, 0.15] {
            for audio in [FastForwardAudio::PreservePitch, FastForwardAudio::SpedUp] {
                for speed in [0.75, 1.0, 1.5, 2.0] {
                    let (device, dropped, ratio) = clocked_run(target, 4.0, &[(speed, Mode::Fast(audio))], 60.0);
                    let mean = device.queue_sum / device.measured as f64 / AUDIO_SAMPLE_RATE;
                    eprintln!("target={target} mode={audio:?} speed={speed} starved={} mean={mean:.4}s min={:.4}s catchup_dropped={dropped} ratio={ratio:.3}", device.starved, device.queue_min as f64 / AUDIO_SAMPLE_RATE);
                    if device.starved != 0 || mean < target * 0.8 || mean > target * 1.5
                        || device.queue_min as f64 / AUDIO_SAMPLE_RATE < target * 0.5 || dropped != 0 {
                        failures.push((target, audio, speed, device.starved, mean, dropped));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "starvation or unsafe device queue: {failures:?}");
    }

    #[test]
    fn simulated_device_counts_silence_and_time_weighted_queue() {
        let mut device = Simulated::new(100, 0.1);
        device.warmup = 5;
        device.queued = 10;
        device.advance(20);
        assert_eq!(device.played, 20);
        assert_eq!(device.measured, 15);
        assert_eq!(device.starved, 10);
        assert_eq!(device.queue_sum, 15.0); // 5 + 4 + 3 + 2 + 1
        assert_eq!(device.queue_min, 0);
    }

    #[test]
    fn fast_audio_tracks_speed_steps_and_mode_changes_without_starvation() {
        for target in [0.1, 0.15] {
            for audio in [FastForwardAudio::PreservePitch, FastForwardAudio::SpedUp] {
                let other = if audio == FastForwardAudio::PreservePitch { FastForwardAudio::SpedUp } else { FastForwardAudio::PreservePitch };
                let schedule = [(2.0, Mode::Fast(audio)), (0.75, Mode::Fast(audio)),
                    (1.5, Mode::Fast(audio)), (2.0, Mode::Fast(other)), (1.0, Mode::Normal)];
                let (device, dropped, _) = clocked_run(target, 4.0, &schedule, 30.0);
                let mean = device.queue_sum / device.measured as f64 / AUDIO_SAMPLE_RATE;
                eprintln!("steps target={target} mode={audio:?} starved={} mean={mean:.4}s min={:.4}s catchup_dropped={dropped}", device.starved, device.queue_min as f64 / AUDIO_SAMPLE_RATE);
                assert_eq!(device.starved, 0, "{target}s, {audio:?}");
                assert!(mean >= target * 0.8 && mean < target * 1.5);
                assert_eq!(dropped, 0);
            }
        }
    }

    #[test]
    fn fast_audio_handles_devices_that_hold_only_the_target() {
        for target in [0.1, 0.15] {
            for audio in [FastForwardAudio::PreservePitch, FastForwardAudio::SpedUp] {
                for speed in [0.75, 1.0, 1.5, 2.0] {
                    let (device, dropped, _) = clocked_run(target, 1.0, &[(speed, Mode::Fast(audio))], 60.0);
                    let mean = device.queue_sum / device.measured as f64 / AUDIO_SAMPLE_RATE;
                    eprintln!("small target={target} mode={audio:?} speed={speed} starved={} mean={mean:.4}s min={:.4}s catchup_dropped={dropped}", device.starved, device.queue_min as f64 / AUDIO_SAMPLE_RATE);
                    assert_eq!(device.starved, 0);
                    // One guest frame at 0.75x spans 22ms; a device capped
                    // at its target necessarily spends part of that lower.
                    assert!(mean > target - 0.025 && mean <= target);
                    assert_eq!(dropped, 0);
                }
            }
        }
    }

    #[test]
    fn normal_pitch_preserves_a_tone_while_sped_up_doubles_its_pitch() {
        let tone: Vec<_> = (0..AUDIO_SAMPLE_RATE as usize).map(|i| {
            [((i as f64 * std::f64::consts::TAU * 440.0 / AUDIO_SAMPLE_RATE).sin() * 8000.0) as i16; 2]
        }).collect();
        for (audio, expected_pitch) in [(FastForwardAudio::PreservePitch, 440.0), (FastForwardAudio::SpedUp, 880.0)] {
            let mut stretch = Stretch::default();
            stretch.feed(&tone);
            let mut out = Vec::new();
            if audio == FastForwardAudio::PreservePitch { stretch.stretch(0.5, &mut out); }
            else { stretch.resample(0.5, &mut out); }
            assert!((0.45..0.55).contains(&(out.len() as f64 / AUDIO_SAMPLE_RATE)));
            let crossings = out.windows(2).filter(|pair| pair[0][0] <= 0 && pair[1][0] > 0).count();
            let pitch = crossings as f64 * AUDIO_SAMPLE_RATE / out.len() as f64;
            assert!((pitch - expected_pitch).abs() < expected_pitch * 0.03, "{audio:?}: {pitch} Hz");
        }
    }

    #[test]
    fn sped_up_resampling_keeps_fractional_position_across_batches() {
        let input: Vec<_> = (0..4000).map(|i| [i; 2]).collect();
        let mut whole = Stretch::default();
        whole.feed(&input);
        let mut expected = Vec::new();
        whole.resample(0.63, &mut expected);
        let mut batched = Stretch::default();
        let mut actual = Vec::new();
        for batch in input.chunks(137) {
            batched.feed(batch);
            batched.resample(0.63, &mut actual);
        }
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected) { assert!((a[0] - b[0]).abs() <= 1); }
    }

    #[test]
    fn skipping_leaves_out_only_what_is_pending() {
        let mut stretch = Stretch::default();
        stretch.feed(&[[0; 2]; 1000]);
        assert_eq!(stretch.pending(), 1000);
        assert_eq!(stretch.skip(400), 400);
        assert_eq!(stretch.pending(), 600);
        assert_eq!(stretch.skip(10_000), 600);
        assert_eq!(stretch.pending(), 0);
    }
}
