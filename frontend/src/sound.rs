//! the title's sound through ALSA, or AAudio on Android, stretched in time
//! to keep it a twentieth to a tenth of a second behind the picture whatever
//! the frame rate, counting all of it not yet heard, and leaving some out to
//! catch up at once when it falls far behind.

use std::sync::mpsc;

use zakuro_core::AUDIO_SAMPLE_RATE;

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
}

pub fn play(samples: mpsc::Receiver<Vec<[i16; 2]>>) {
    std::thread::spawn(move || {
        let Some(mut device) = open() else {
            for _ in samples {}
            return;
        };
        let mut pacer = Pacer::new(device.target());
        let mut since_log = std::time::Instant::now();
        while let Ok(first) = samples.recv() {
            // everything waiting, taken at once, so that how far the sound is
            // behind counts it: left in the channel it would pile up unseen
            let mut input = first;
            for more in samples.try_iter() {
                input.extend_from_slice(&more);
            }
            let behind = pacer.take(device.as_mut(), &input);
            if since_log.elapsed() >= std::time::Duration::from_secs(30) {
                log::info!(
                    "sound {:.0} ms behind, stretched {:.2} times, {} ms left out to catch up",
                    behind / AUDIO_SAMPLE_RATE * 1000.0,
                    pacer.ratio,
                    (std::mem::take(&mut pacer.dropped) as f64 / AUDIO_SAMPLE_RATE * 1000.0) as u64
                );
                since_log = std::time::Instant::now();
            }
        }
    });
}

/// keeps the sound target seconds behind: stretches it longer while less is
/// queued and shorter while more is, and past four times the target leaves
/// the oldest out.
struct Pacer {
    stretch: Stretch,
    target: f64,
    ratio: f64,
    chunk: Vec<[i16; 2]>,
    /// frames left out since the last look.
    dropped: usize,
}

impl Pacer {
    fn new(target_seconds: f64) -> Pacer {
        Pacer { stretch: Stretch::default(), target: AUDIO_SAMPLE_RATE * target_seconds, ratio: 1.0, chunk: Vec::new(), dropped: 0 }
    }

    /// takes the title's newest sound to the device, and says how many of
    /// the title's frames were not yet heard before it went.
    fn take(&mut self, device: &mut dyn Device, input: &[[i16; 2]]) -> f64 {
        self.stretch.feed(input);
        // the sound not yet heard: in the device, and here, not stretched yet
        let behind = device.queued() as f64 + self.stretch.pending() as f64;
        // far behind, after a stall or with a device that holds less than it
        // should, the oldest is left out to catch up at once rather than be
        // stretched away over minutes
        if behind > self.target * 4.0 {
            self.dropped += self.stretch.skip((behind - self.target) as usize);
        }
        let wanted = (1.0 + (self.target - behind) / self.target).clamp(0.75, 2.5);
        self.ratio += (wanted - self.ratio) * 0.1;
        self.chunk.clear();
        self.stretch.stretch(self.ratio, &mut self.chunk);
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
            if open(&mut pcm, b"default\0".as_ptr(), 0, 0) < 0 || set(pcm, 2, 3, 2, AUDIO_SAMPLE_RATE.round() as u32, 1, 150_000) < 0 {
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

/// how much sound AAudio keeps queued, in seconds.
#[cfg(target_os = "android")]
const AAUDIO_TARGET: f64 = 0.05;

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
                .performance_mode(AudioPerformanceMode::LowLatency)
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
        let size = stream.set_buffer_size_in_frames(wanted).unwrap_or(0);
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
        let used = (self.at as usize).saturating_sub(SEEK);
        if used > 0 {
            self.input.drain(..used);
            self.at -= used as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a device that plays at the title's rate in real time and holds at
    /// most capacity frames, its writes waiting for room as a real one's do,
    /// the clock moving only as it plays.
    struct Simulated {
        capacity: usize,
        queued: usize,
        /// frames played, which is the clock.
        played: usize,
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
                    self.queued -= burst;
                    self.played += burst;
                    continue;
                }
                let taken = room.min(left);
                self.queued += taken;
                left -= taken;
            }
        }

        fn target(&self) -> f64 {
            0.05
        }
    }

    /// minutes of a title making its sound on its own thread at speed
    /// times real time, a frame's worth at a time into a channel, while play
    /// takes it to a device of capacity frames: how far behind the sound
    /// ends, in seconds, counting what waits in the channel, in the pacer and
    /// in the device.
    fn run(capacity: usize, speed: f64, minutes: f64, all_at_once: bool) -> f64 {
        let mut device = Simulated { capacity, queued: 0, played: 0 };
        let mut pacer = Pacer::new(device.target());
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
        while device.played < end {
            catch_up(device.played, &mut channel);
            let Some(first) = channel.pop_front() else {
                // nothing waiting: the device plays a burst meanwhile
                let burst = 96.min(device.queued).max(1);
                device.queued = device.queued.saturating_sub(burst);
                device.played += burst;
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
        (waiting as f64 + pacer.stretch.pending() as f64 + device.queued as f64 / pacer.ratio) / AUDIO_SAMPLE_RATE
    }

    /// a buffer too small to hold the target, as AAudio's was, must not
    /// leave the sound further and further behind.
    #[test]
    fn a_small_buffer_does_not_drift() {
        let behind = run(192, 1.0, 5.0, true);
        assert!(behind < 0.2, "{:.0} ms behind after five minutes", behind * 1000.0);
    }

    #[test]
    fn a_roomy_buffer_stays_near_the_target() {
        let behind = run(4096, 1.0, 5.0, true);
        assert!(behind < 0.2, "{:.0} ms behind after five minutes", behind * 1000.0);
    }

    /// a title running a little fast makes sound faster than it plays, which
    /// is squeezed or left out rather than piled up.
    #[test]
    fn a_fast_title_does_not_drift() {
        let behind = run(4096, 1.05, 5.0, true);
        assert!(behind < 0.25, "{:.0} ms behind after five minutes", behind * 1000.0);
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
