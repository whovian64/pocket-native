//! the audio DSP's voices, which play their buffers, report on them, and
//! give the mixer what they sound like.

use zakuro_cpu::Bus;

use crate::memory::Memory;

pub const VOICES: usize = 24;
/// samples in one audio frame.
pub const FRAME_SAMPLES: usize = 160;

const CONFIG_SIZE: u32 = 192;
const STATUS_SIZE: u32 = 12;
/// the 16 ADPCM coefficients each voice has.
const COEFFICIENTS_SIZE: u32 = 32;
/// region 1 of the shared memory starts this many bytes after region 0.
pub const REGION_STRIDE: u32 = 0x2_0000;

/// one frame of stereo samples.
pub type Frame = [[i16; 2]; FRAME_SAMPLES];
/// one frame of an intermediate mix, two left and two right channels.
pub type QuadFrame = [[i32; 4]; FRAME_SAMPLES];

/// how far through a frame each sample is, for a gain fading over it.
const RAMP: [f32; FRAME_SAMPLES] = {
    let mut ramp = [0.0; FRAME_SAMPLES];
    let mut i = 0;
    while i < FRAME_SAMPLES {
        ramp[i] = i as f32 / (FRAME_SAMPLES - 1) as f32;
        i += 1;
    }
    ramp
};

/// bits of a configuration's dirty word, which fields the title changed.
mod dirty {
    pub const FORMAT: u32 = 1 << 0;
    pub const MONO_OR_STEREO: u32 = 1 << 1;
    pub const ADPCM_COEFFICIENTS: u32 = 1 << 2;
    pub const PARTIAL_EMBEDDED_BUFFER: u32 = 1 << 3;
    pub const PARTIAL_RESET: u32 = 1 << 4;
    pub const ENABLE: u32 = 1 << 16;
    pub const INTERPOLATION: u32 = 1 << 17;
    pub const RATE_MULTIPLIER: u32 = 1 << 18;
    pub const BUFFER_QUEUE: u32 = 1 << 19;
    pub const PLAY_POSITION: u32 = 1 << 21;
    pub const FILTERS_ENABLED: u32 = 1 << 22;
    pub const SIMPLE_FILTER: u32 = 1 << 23;
    pub const BIQUAD_FILTER: u32 = 1 << 24;
    /// one bit per intermediate mix from here up.
    pub const GAIN: u32 = 1 << 25;
    pub const SYNC_COUNT: u32 = 1 << 28;
    pub const RESET: u32 = 1 << 29;
    pub const EMBEDDED_BUFFER: u32 = 1 << 30;
}

/// offsets into one voice's configuration.
mod config {
    pub const DIRTY: u32 = 0x00;
    pub const GAIN: u32 = 0x04;
    pub const RATE_MULTIPLIER: u32 = 0x34;
    pub const INTERPOLATION: u32 = 0x38;
    pub const FILTERS_ENABLED: u32 = 0x3A;
    pub const SIMPLE_FILTER: u32 = 0x3C;
    pub const BIQUAD_FILTER: u32 = 0x40;
    pub const BUFFERS_DIRTY: u32 = 0x4A;
    pub const BUFFERS: u32 = 0x4C;
    pub const BUFFER_SIZE: u32 = 20;
    pub const ENABLE: u32 = 0xA0;
    pub const SYNC_COUNT: u32 = 0xA2;
    pub const PLAY_POSITION: u32 = 0xA4;
    pub const ADDRESS: u32 = 0xAC;
    pub const LENGTH: u32 = 0xB0;
    pub const FLAGS1: u32 = 0xB4;
    pub const ADPCM_YN: u32 = 0xB8;
    pub const FLAGS2: u32 = 0xBC;
    pub const BUFFER_ID: u32 = 0xBE;
}

/// addresses of the structures the voices use, in region 0.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub configurations: u32,
    pub statuses: u32,
    pub coefficients: u32,
    pub frame_counter: u32,
}

/// where voices read their samples, physical memory.
pub trait SampleMemory {
    fn physical(&mut self, address: u32, length: u32) -> Option<&[u8]>;
}

impl SampleMemory for Memory {
    fn physical(&mut self, address: u32, length: u32) -> Option<&[u8]> {
        self.phys.host_slice_mut(address, length).map(|slice| &*slice)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Pcm8,
    Pcm16,
    Adpcm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interpolation {
    /// the firmware's polyphase filter, played as linear.
    Polyphase,
    Linear,
    None,
}

#[derive(Debug, Clone, Copy)]
struct Buffer {
    address: u32,
    length: u32,
    format: Format,
    stereo: bool,
    looping: bool,
    id: u16,
    /// the ADPCM history to start from, when the title set it.
    adpcm: Option<[i16; 2]>,
    /// queued buffers report when they start, the embedded one does not.
    from_queue: bool,
    play_position: u32,
    has_played: bool,
}

/// the first order filter, y = b0 x + a1 y1, with 15 fraction bits.
#[derive(Debug, Clone, Copy)]
struct SimpleFilter {
    b0: i32,
    a1: i32,
    y1: [i32; 2],
}

impl Default for SimpleFilter {
    fn default() -> Self {
        SimpleFilter { b0: 1 << 15, a1: 0, y1: [0; 2] }
    }
}

/// the second order filter, with 14 fraction bits and its feedback negated.
#[derive(Debug, Clone, Copy)]
struct BiquadFilter {
    b: [i32; 3],
    a: [i32; 2],
    x: [[i32; 2]; 2],
    y: [[i32; 2]; 2],
}

impl Default for BiquadFilter {
    fn default() -> Self {
        BiquadFilter { b: [1 << 14, 0, 0], a: [0; 2], x: [[0; 2]; 2], y: [[0; 2]; 2] }
    }
}

#[derive(Debug, Clone, Default)]
struct Filters {
    simple: SimpleFilter,
    biquad: BiquadFilter,
    /// Coefficient updates do not change which filters the title enabled.
    enabled: u16,
}

impl Filters {
    fn process(&mut self, samples: &mut [[i16; 2]]) {
        for sample in samples {
            for (channel, out) in sample.iter_mut().enumerate() {
                let mut value = *out as i32;
                if self.enabled & 1 != 0 {
                    let f = &mut self.simple;
                    value = ((f.b0 * value + f.a1 * f.y1[channel]) >> 15).clamp(-32768, 32767);
                    f.y1[channel] = value;
                }
                if self.enabled & 2 != 0 {
                    let f = &mut self.biquad;
                    let x0 = value;
                    value = ((f.b[0] * x0 + f.b[1] * f.x[0][channel] + f.b[2] * f.x[1][channel]
                        + f.a[0] * f.y[0][channel]
                        + f.a[1] * f.y[1][channel])
                        >> 14)
                        .clamp(-32768, 32767);
                    f.x[1][channel] = f.x[0][channel];
                    f.x[0][channel] = x0;
                    f.y[1][channel] = f.y[0][channel];
                    f.y[0][channel] = value;
                }
                *out = value as i16;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Voice {
    enabled: bool,
    sync_count: u16,
    rate: f64,
    format: Format,
    stereo: bool,
    interpolation: Interpolation,
    filters: Filters,
    /// how loud the voice is in each channel of each intermediate mix.
    gain: [[f32; 4]; 3],
    /// the gains a mix fades from over the next frame, after they changed.
    ramp: [Option<[f32; 4]>; 3],
    coefficients: [i16; 16],
    /// the last two ADPCM samples, which the next ones follow on from.
    history: [i16; 2],
    queue: Vec<Buffer>,
    /// the buffer being played.
    playing: Option<Buffer>,
    /// an ADPCM buffer, decoded when it started.
    decoded: Vec<i16>,
    /// samples left in the buffer being played, and how long it is.
    remaining: u32,
    length: u32,
    /// input consumed but not yet a whole sample, in 2^-32 of one.
    fraction: u32,
    current_buffer_id: u16,
    last_buffer_id: u16,
    /// set when a new queued buffer starts, reported once.
    buffer_update: bool,
    /// where in its buffer the voice was at the start of the frame.
    position: u32,
    /// what the voice played this frame.
    frame: Frame,
    /// the buffer ran out on the frame's last sample, so the next one
    /// waiting follows on from the next frame's first, as on the console,
    /// rather than after a frame of silence.
    ran_out_at_frame_end: bool,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            enabled: false,
            sync_count: 0,
            rate: 1.0,
            format: Format::Pcm16,
            stereo: false,
            interpolation: Interpolation::Polyphase,
            filters: Filters::default(),
            gain: [[0.0; 4]; 3],
            ramp: [None; 3],
            coefficients: [0; 16],
            history: [0; 2],
            queue: Vec::new(),
            playing: None,
            decoded: Vec::new(),
            remaining: 0,
            length: 0,
            fraction: 0,
            current_buffer_id: 0,
            last_buffer_id: 0,
            buffer_update: false,
            position: 0,
            frame: [[0; 2]; FRAME_SAMPLES],
            ran_out_at_frame_end: false,
        }
    }
}

impl Voice {
    /// starts the next buffer, the lowest id waiting.
    fn dequeue(&mut self, memory: &mut impl SampleMemory) -> bool {
        let Some(index) = (0..self.queue.len()).min_by_key(|&i| self.queue[i].id) else {
            return false;
        };
        let buffer = self.queue[index];
        if buffer.looping {
            self.queue[index].has_played = true;
        } else {
            self.queue.remove(index);
        }

        // only the first time through starts at the play position.
        let start = if buffer.has_played { 0 } else { buffer.play_position.min(buffer.length) };
        self.length = buffer.length;
        self.remaining = buffer.length - start;
        self.position = start;
        self.fraction = 0;
        self.current_buffer_id = buffer.id;
        self.last_buffer_id = 0;
        self.buffer_update = buffer.from_queue && !buffer.has_played;

        if let Some(history) = buffer.adpcm {
            self.history = history;
        }
        self.decoded.clear();
        if buffer.format == Format::Adpcm {
            let bytes = buffer.length.div_ceil(14) * 8;
            match memory.physical(buffer.address & !3, bytes) {
                Some(data) => self.decoded = decode_adpcm(data, buffer.length as usize, &self.coefficients, &mut self.history),
                None => log::debug!("dsp: an ADPCM buffer at 0x{:08X} is not in memory", buffer.address),
            }
        }
        self.playing = Some(buffer);
        true
    }

    /// input sample index of the buffer being played, both channels.
    fn input(&self, memory: &mut impl SampleMemory, index: u32) -> [i16; 2] {
        let Some(buffer) = &self.playing else { return [0; 2] };
        let index = index.min(buffer.length.saturating_sub(1));
        let channels = if buffer.stereo { 2 } else { 1 };
        let at = buffer.address & !3;
        let [left, right] = match buffer.format {
            Format::Adpcm => {
                let sample = self.decoded.get(index as usize).copied().unwrap_or(0);
                [sample, sample]
            }
            Format::Pcm8 => match memory.physical(at + index * channels, channels) {
                Some(data) => [data[0] as i8 as i16 * 256, data[data.len() - 1] as i8 as i16 * 256],
                None => [0; 2],
            },
            Format::Pcm16 => match memory.physical(at + index * channels * 2, channels * 2) {
                Some(data) => [
                    i16::from_le_bytes([data[0], data[1]]),
                    i16::from_le_bytes([data[data.len() - 2], data[data.len() - 1]]),
                ],
                None => [0; 2],
            },
        };
        [left, right]
    }

    /// the playing buffer's input sample index from its bytes, read in one
    /// go, as input reads it.
    fn input_from(&self, data: &[u8], index: u32) -> [i16; 2] {
        let Some(buffer) = &self.playing else { return [0; 2] };
        let index = index.min(buffer.length.saturating_sub(1)) as usize;
        let channels = if buffer.stereo { 2 } else { 1 };
        match buffer.format {
            Format::Adpcm => {
                let sample = self.decoded.get(index).copied().unwrap_or(0);
                [sample, sample]
            }
            Format::Pcm8 => {
                let data = &data[index * channels..(index + 1) * channels];
                [data[0] as i8 as i16 * 256, data[channels - 1] as i8 as i16 * 256]
            }
            Format::Pcm16 => {
                let data = &data[index * channels * 2..(index + 1) * channels * 2];
                [i16::from_le_bytes([data[0], data[1]]), i16::from_le_bytes([data[channels * 2 - 2], data[channels * 2 - 1]])]
            }
        }
    }

    /// the bytes of the buffer playing, how many it takes up.
    fn playing_bytes(&self) -> Option<(u32, u32)> {
        let buffer = self.playing.as_ref()?;
        let width = match buffer.format {
            Format::Adpcm => return Some((0, 0)),
            Format::Pcm8 => 1,
            Format::Pcm16 => 2,
        };
        Some((buffer.address & !3, buffer.length * width * if buffer.stereo { 2 } else { 1 }))
    }

    /// the output sample at the current position, between two inputs.
    fn sample(&self, mut input: impl FnMut(u32) -> [i16; 2]) -> [i16; 2] {
        let at = self.length - self.remaining;
        let x0 = input(at);
        if self.interpolation == Interpolation::None || self.fraction == 0 {
            return x0;
        }
        let x1 = input(at + 1);
        let between = |a: i16, b: i16| (a as i64 + (((b as i64 - a as i64) * self.fraction as i64) >> 32)) as i16;
        [between(x0[0], x1[0]), between(x0[1], x1[1])]
    }

    /// plays one audio frame's worth of input.
    fn play_frame(&mut self, memory: &mut impl SampleMemory) {
        self.frame = [[0; 2]; FRAME_SAMPLES];
        let follows_on = std::mem::take(&mut self.ran_out_at_frame_end) && !self.queue.is_empty();
        if self.remaining == 0 && !follows_on {
            if self.dequeue(memory) {
                return;
            }
            // out of buffers, the voice switches itself off and reports the
            // buffer it finished as the last one, with none current. a title
            // waits on that to know its sound is over.
            self.enabled = false;
            self.buffer_update = true;
            self.last_buffer_id = self.current_buffer_id;
            self.current_buffer_id = 0;
            self.position = 0;
            return;
        }

        self.position = self.length - self.remaining;
        // each output sample consumes rate input samples, counted in 2^-32
        let step = (self.rate * 4_294_967_296.0) as u64;
        let mut output = 0;
        let advance = |voice: &mut Voice| {
            let input = voice.fraction as u64 + step;
            let consumed = ((input >> 32) as u32).min(voice.remaining);
            voice.fraction = (input - ((consumed as u64) << 32)).min(u32::MAX as u64) as u32;
            voice.remaining -= consumed;
        };
        while output < FRAME_SAMPLES {
            if self.remaining == 0 && !self.dequeue(memory) {
                break;
            }
            // the buffer's bytes looked up once for the stretch of it this
            // frame plays, a lookup for every sample cost more than mixing
            let data = match self.playing_bytes() {
                // ADPCM was decoded when the buffer came up
                Some((_, 0)) if self.playing.is_some_and(|buffer| buffer.format == Format::Adpcm) => Some(&[][..]),
                Some((address, length)) if length > 0 => memory.physical(address, length),
                _ => None,
            };
            match data {
                Some(data) => loop {
                    self.frame[output] = self.sample(|index| self.input_from(data, index));
                    output += 1;
                    advance(self);
                    if output == FRAME_SAMPLES || self.remaining == 0 {
                        break;
                    }
                },
                None => {
                    self.frame[output] = self.sample(|index| self.input(memory, index));
                    output += 1;
                    advance(self);
                }
            }
        }
        self.ran_out_at_frame_end = self.remaining == 0 && output == FRAME_SAMPLES;
        self.filters.process(&mut self.frame[..output]);
    }

    /// what mixing needs of what the voice played this frame, which takes
    /// its gains' fades.
    fn to_mix(&mut self) -> Option<VoiceMix> {
        let from = [0, 1, 2].map(|index| self.ramp[index].take());
        (self.enabled && (from.iter().any(Option::is_some) || self.gain.iter().any(|gains| *gains != [0.0; 4])))
            .then(|| VoiceMix { frame: self.frame, gains: self.gain, from })
    }
}

/// what one voice adds to the mixes in a frame, all that mixing needs, so
/// it can happen on a thread of its own as the console's DSP does it.
pub struct VoiceMix {
    frame: Frame,
    gains: [[f32; 4]; 3],
    from: [Option<[f32; 4]>; 3],
}

/// the three intermediate mixes of what the voices played.
pub fn mix(voices: &[VoiceMix]) -> [QuadFrame; 3] {
    let mut mixes = [[[0; 4]; FRAME_SAMPLES]; 3];
    for voice in voices {
        for (index, mix) in mixes.iter_mut().enumerate() {
            mix_frame(&voice.frame, voice.gains[index], voice.from[index], mix);
        }
    }
    mixes
}

/// adds a frame to an intermediate mix at gains, fading to them from
/// others when they just changed.
fn mix_frame(frame: &Frame, gains: [f32; 4], from: Option<[f32; 4]>, mix: &mut QuadFrame) {
    // most voices reach only the first mix, the rest add nothing
    if from.is_none() && gains == [0.0; 4] {
        return;
    }
    // the four channels side by side, which the compiler does at once
    let quad = |sample: &[i16; 2]| [sample[0] as f32, sample[1] as f32, sample[0] as f32, sample[1] as f32];
    let Some(from) = from else {
        for (out, sample) in mix.iter_mut().zip(frame) {
            let sample = quad(sample);
            for c in 0..4 {
                out[c] += (gains[c] * sample[c]) as i32;
            }
        }
        return;
    };
    let span: [f32; 4] = std::array::from_fn(|c| gains[c] - from[c]);
    for ((out, sample), progress) in mix.iter_mut().zip(frame).zip(RAMP) {
        let sample = quad(sample);
        for c in 0..4 {
            out[c] += ((from[c] + span[c] * progress) * sample[c]) as i32;
        }
    }
}

/// GC ADPCM, frames of 8 bytes holding a header and 14 samples of 4 bits,
/// each predicted from the two before it.
fn decode_adpcm(data: &[u8], count: usize, coefficients: &[i16; 16], history: &mut [i16; 2]) -> Vec<i16> {
    let mut samples = Vec::with_capacity(count);
    let [mut yn1, mut yn2] = history.map(|h| h as i32);
    for frame in data.chunks(8) {
        let header = frame[0];
        let scale = 1 << (header & 0xF);
        let predictor = ((header >> 4) & 7) as usize;
        let (c1, c2) = (coefficients[predictor * 2] as i32, coefficients[predictor * 2 + 1] as i32);
        for &byte in &frame[1..] {
            for nibble in [byte >> 4, byte & 0xF] {
                if samples.len() == count {
                    *history = [yn1 as i16, yn2 as i16];
                    return samples;
                }
                // the nibble is signed
                let xn = (((nibble as i32) << 28) >> 28) * scale;
                let value = (((xn << 11) + 0x400 + c1 * yn1 + c2 * yn2) >> 11).clamp(-32768, 32767);
                yn2 = yn1;
                yn1 = value;
                samples.push(value as i16);
            }
        }
    }
    *history = [yn1 as i16, yn2 as i16];
    samples
}

#[derive(Debug, Clone)]
pub struct Voices {
    voices: Vec<Voice>,
}

impl Default for Voices {
    fn default() -> Self {
        Voices {
            voices: vec![Voice::default(); VOICES],
        }
    }
}

impl Voices {
    /// one audio frame, take in what the title changed, play, report, and
    /// return the three intermediate mixes the voices made.
    pub fn tick(&mut self, memory: &mut Memory, layout: Layout) -> Vec<VoiceMix> {
        let read = current_region(memory, layout);
        let write = 1 - read;
        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "dsp: tick, frame counters {} / {}, reading region {read}",
                memory.read16(layout.frame_counter),
                memory.read16(layout.frame_counter + REGION_STRIDE),
            );
        }
        let configurations = layout.configurations + read * REGION_STRIDE;
        let coefficients = layout.coefficients + read * REGION_STRIDE;
        let statuses = layout.statuses + write * REGION_STRIDE;

        let mut mixing = Vec::with_capacity(self.voices.len());
        for (index, voice) in self.voices.iter_mut().enumerate() {
            let base = configurations + index as u32 * CONFIG_SIZE;
            let was_enabled = voice.enabled;
            let queued = voice.queue.len();
            parse_config(voice, memory, base, coefficients + index as u32 * COEFFICIENTS_SIZE);
            if voice.enabled != was_enabled || voice.queue.len() > queued {
                log::debug!(
                    "dsp: voice {index} {} with {} queued (ids {:?}), rate {}, playing {} with {} left, \
                     {:?} {} {:?}, gains {:?}",
                    if voice.enabled { "on" } else { "off" },
                    voice.queue.len(),
                    voice.queue.iter().map(|b| (b.id, b.length, b.looping)).collect::<Vec<_>>(),
                    voice.rate,
                    voice.current_buffer_id,
                    voice.remaining,
                    voice.format,
                    if voice.stereo { "stereo" } else { "mono" },
                    voice.interpolation,
                    voice.gain,
                );
            }
            if voice.enabled {
                voice.play_frame(memory);
            }
            mixing.extend(voice.to_mix());
            write_status(voice, memory, statuses + index as u32 * STATUS_SIZE);
        }
        mixing
    }
}

/// which region the title wrote most recently, allowing for the frame
/// counters wrapping around.
pub fn current_region(memory: &mut Memory, layout: Layout) -> u32 {
    let first = memory.read16(layout.frame_counter);
    let second = memory.read16(layout.frame_counter + REGION_STRIDE);
    if first == 0xFFFF && second != 0xFFFE {
        return 1;
    }
    if second == 0xFFFF && first != 0xFFFE {
        return 0;
    }
    if first > second {
        0
    } else {
        1
    }
}

/// reads a 32-bit value the way the DSP stores it, high half first.
fn read_dsp32(memory: &mut Memory, address: u32) -> u32 {
    ((memory.read16(address) as u32) << 16) | memory.read16(address + 2) as u32
}

fn write_dsp32(memory: &mut Memory, address: u32, value: u32) {
    memory.write16(address, (value >> 16) as u16);
    memory.write16(address + 2, value as u16);
}

/// applies whatever the title changed in a voice's configuration, then
/// clears its dirty flags the way the firmware does.
fn parse_config(voice: &mut Voice, memory: &mut Memory, base: u32, coefficients: u32) {
    let flags = memory.read32(base + config::DIRTY);
    if flags == 0 {
        return;
    }

    if flags & dirty::RESET != 0 {
        *voice = Voice::default();
    }
    if flags & dirty::PARTIAL_RESET != 0 {
        voice.queue.clear();
        voice.ran_out_at_frame_end = false;
    }
    if flags & dirty::ENABLE != 0 {
        voice.enabled = memory.read8(base + config::ENABLE) != 0;
        voice.ran_out_at_frame_end = false;
    }
    if flags & dirty::SYNC_COUNT != 0 {
        voice.sync_count = memory.read16(base + config::SYNC_COUNT);
    }
    if flags & dirty::RATE_MULTIPLIER != 0 {
        let rate = f32::from_bits(memory.read32(base + config::RATE_MULTIPLIER)) as f64;
        voice.rate = if rate > 0.0 && rate.is_finite() { rate } else { 1.0 };
    }
    if flags & dirty::INTERPOLATION != 0 {
        voice.interpolation = match memory.read8(base + config::INTERPOLATION) {
            1 => Interpolation::Linear,
            2 => Interpolation::None,
            _ => Interpolation::Polyphase,
        };
    }
    if flags & dirty::ADPCM_COEFFICIENTS != 0 {
        for (i, coefficient) in voice.coefficients.iter_mut().enumerate() {
            *coefficient = memory.read16(coefficients + i as u32 * 2) as i16;
        }
    }
    for mix in 0..3 {
        if flags & (dirty::GAIN << mix) != 0 {
            voice.ramp[mix] = Some(voice.gain[mix]);
            for channel in 0..4 {
                let at = base + config::GAIN + (mix as u32 * 4 + channel as u32) * 4;
                let gain = f32::from_bits(memory.read32(at));
                voice.gain[mix][channel] = if gain.is_finite() { gain } else { 0.0 };
            }
        }
    }
    if flags & dirty::FILTERS_ENABLED != 0 {
        voice.filters.enabled = memory.read16(base + config::FILTERS_ENABLED);
        // Disabling clears history, but coefficients can be configured while
        // disabled and must survive until the title enables the filter.
        if voice.filters.enabled & 1 == 0 {
            voice.filters.simple.y1 = [0; 2];
        }
        if voice.filters.enabled & 2 == 0 {
            voice.filters.biquad.x = [[0; 2]; 2];
            voice.filters.biquad.y = [[0; 2]; 2];
        }
    }
    let half = |memory: &mut Memory, offset: u32| memory.read16(base + offset) as i16 as i32;
    if flags & dirty::SIMPLE_FILTER != 0 {
        let filter = &mut voice.filters.simple;
        filter.b0 = half(memory, config::SIMPLE_FILTER);
        filter.a1 = half(memory, config::SIMPLE_FILTER + 2);
    }
    if flags & dirty::BIQUAD_FILTER != 0 {
        let filter = &mut voice.filters.biquad;
        let field = |memory: &mut Memory, i: u32| memory.read16(base + config::BIQUAD_FILTER + i * 2) as i16 as i32;
        filter.a = [field(memory, 1), field(memory, 0)];
        filter.b = [field(memory, 4), field(memory, 3), field(memory, 2)];
    }
    // the embedded buffer brings its format along
    let flags1 = memory.read16(base + config::FLAGS1);
    if flags & (dirty::FORMAT | dirty::EMBEDDED_BUFFER) != 0 {
        voice.format = match (flags1 >> 2) & 3 {
            0 => Format::Pcm8,
            2 => Format::Adpcm,
            _ => Format::Pcm16,
        };
    }
    if flags & (dirty::MONO_OR_STEREO | dirty::EMBEDDED_BUFFER) != 0 {
        voice.stereo = flags1 & 3 == 2;
    }

    let play_position = if flags & dirty::PLAY_POSITION != 0 {
        read_dsp32(memory, base + config::PLAY_POSITION)
    } else {
        0
    };

    // the title lengthened the buffer that is already playing.
    if flags & dirty::PARTIAL_EMBEDDED_BUFFER != 0 {
        let length = read_dsp32(memory, base + config::LENGTH);
        let played = voice.length - voice.remaining;
        voice.length = length.max(played);
        voice.remaining = voice.length - played;
        if let Some(playing) = &mut voice.playing {
            playing.length = voice.length;
        }
    }

    if flags & dirty::EMBEDDED_BUFFER != 0 {
        let flags2 = memory.read16(base + config::FLAGS2);
        voice.queue.push(Buffer {
            address: read_dsp32(memory, base + config::ADDRESS),
            length: read_dsp32(memory, base + config::LENGTH),
            format: voice.format,
            stereo: voice.stereo,
            looping: flags2 & 0x2 != 0,
            id: memory.read16(base + config::BUFFER_ID),
            adpcm: (flags2 & 1 != 0).then(|| {
                [
                    memory.read16(base + config::ADPCM_YN) as i16,
                    memory.read16(base + config::ADPCM_YN + 2) as i16,
                ]
            }),
            from_queue: false,
            play_position,
            has_played: false,
        });
    }

    if flags & dirty::BUFFER_QUEUE != 0 {
        let queued = memory.read16(base + config::BUFFERS_DIRTY);
        for slot in 0..4 {
            if queued & (1 << slot) == 0 {
                continue;
            }
            let buffer = base + config::BUFFERS + slot * config::BUFFER_SIZE;
            let length = read_dsp32(memory, buffer + 4);
            if length != 0 {
                voice.queue.push(Buffer {
                    address: read_dsp32(memory, buffer),
                    length,
                    format: voice.format,
                    stereo: voice.stereo,
                    looping: memory.read8(buffer + 15) != 0,
                    id: memory.read16(buffer + 16),
                    adpcm: (memory.read8(buffer + 14) != 0)
                        .then(|| [memory.read16(buffer + 10) as i16, memory.read16(buffer + 12) as i16]),
                    from_queue: true,
                    play_position: 0,
                    has_played: false,
                });
            }
        }
        memory.write16(base + config::BUFFERS_DIRTY, 0);
    }

    memory.write32(base + config::DIRTY, 0);
}

fn write_status(voice: &mut Voice, memory: &mut Memory, base: u32) {
    memory.write8(base, voice.enabled as u8);
    memory.write8(base + 1, voice.buffer_update as u8);
    voice.buffer_update = false;
    memory.write16(base + 2, voice.sync_count);
    write_dsp32(memory, base + 4, voice.position);
    memory.write16(base + 8, voice.current_buffer_id);
    memory.write16(base + 10, voice.last_buffer_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mixing gives what it did before it was laid out four channels side
    /// by side, steady and fading, sample for sample.
    #[test]
    fn mixing_matches_the_plain_loop() {
        let reference = |frame: &Frame, from: Option<[f32; 4]>, gains: [f32; 4]| {
            let mut mix = [[7; 4]; FRAME_SAMPLES];
            for (i, (out, sample)) in mix.iter_mut().zip(frame).enumerate() {
                let progress = i as f32 / (FRAME_SAMPLES - 1) as f32;
                let gain = |c: usize| from.map_or(gains[c], |from| from[c] + (gains[c] - from[c]) * progress);
                out[0] += (gain(0) * sample[0] as f32) as i32;
                out[1] += (gain(1) * sample[1] as f32) as i32;
                out[2] += (gain(2) * sample[0] as f32) as i32;
                out[3] += (gain(3) * sample[1] as f32) as i32;
            }
            mix
        };
        let mut frame = [[0; 2]; FRAME_SAMPLES];
        for (i, sample) in frame.iter_mut().enumerate() {
            *sample = [(i as i32 * 977 % 65536 - 32768) as i16, (i as i32 * -613 % 32768) as i16];
        }
        for (from, gains) in [(None, [0.7, 1.3, 0.25, 0.0]), (Some([0.1, 0.9, 1.0, 0.33]), [0.8, 0.05, 0.6, 1.7])] {
            let mut mix = [[7; 4]; FRAME_SAMPLES];
            mix_frame(&frame, gains, from, &mut mix);
            assert_eq!(mix, reference(&frame, from, gains));
        }
    }

    const BASE: u32 = 0x2000_0000;

    /// physical memory holding one buffer's bytes at BASE.
    struct Samples(Vec<u8>);

    impl SampleMemory for Samples {
        fn physical(&mut self, address: u32, length: u32) -> Option<&[u8]> {
            let start = address.checked_sub(BASE)? as usize;
            self.0.get(start..start + length as usize)
        }
    }

    fn silence() -> Samples {
        Samples(vec![0; 0x10000])
    }

    fn buffer(id: u16, length: u32) -> Buffer {
        Buffer {
            address: BASE,
            length,
            format: Format::Pcm16,
            stereo: false,
            looping: false,
            id,
            adpcm: None,
            from_queue: true,
            play_position: 0,
            has_played: false,
        }
    }

    fn voice_with(buffers: &[(u16, u32)]) -> Voice {
        let mut voice = Voice {
            enabled: true,
            ..Voice::default()
        };
        for &(id, length) in buffers {
            voice.queue.push(buffer(id, length));
        }
        voice
    }

    #[test]
    fn buffers_play_lowest_id_first_and_report_starting() {
        let mut voice = voice_with(&[(7, 320), (6, 320)]);
        voice.play_frame(&mut silence());
        assert_eq!(voice.current_buffer_id, 6);
        assert!(voice.buffer_update, "a queued buffer reports that it started");
    }

    /// at a rate of one, a 320-sample buffer lasts two audio frames.
    #[test]
    fn a_voice_moves_through_its_buffers_in_real_time() {
        let mut memory = silence();
        let mut voice = voice_with(&[(1, 320), (2, 320)]);
        voice.play_frame(&mut memory); // starts buffer 1
        voice.play_frame(&mut memory); // plays 160
        assert_eq!(voice.current_buffer_id, 1);
        assert_eq!(voice.remaining, 160);
        voice.play_frame(&mut memory); // finishes buffer 1
        voice.play_frame(&mut memory); // starts buffer 2 on the way
        assert_eq!(voice.current_buffer_id, 2);
    }

    /// a buffer that ends on a frame's last sample is followed by the next
    /// with no silent frame between, a stream of them would stutter.
    #[test]
    fn queued_buffers_play_on_without_a_gap() {
        let mut memory = Samples(1000i16.to_le_bytes().repeat(0x8000));
        let mut voice = voice_with(&[(1, 320), (2, 320)]);
        voice.play_frame(&mut memory); // starts buffer 1
        for frame in 0..4 {
            voice.play_frame(&mut memory);
            assert!(voice.frame.iter().all(|s| s[0] == 1000), "frame {frame} has a gap");
        }
        assert_eq!((voice.current_buffer_id, voice.remaining), (2, 0));
        voice.play_frame(&mut memory);
        assert!(!voice.enabled, "and still switches off as soon as it runs out");
    }

    #[test]
    fn a_higher_rate_consumes_input_faster() {
        let mut memory = silence();
        let mut voice = voice_with(&[(1, 1000)]);
        voice.rate = 2.0;
        voice.play_frame(&mut memory);
        voice.play_frame(&mut memory);
        assert_eq!(voice.remaining, 1000 - 320);
    }

    #[test]
    fn running_out_of_buffers_switches_the_voice_off() {
        let mut memory = silence();
        let mut voice = voice_with(&[(4, 160)]);
        for _ in 0..3 {
            voice.play_frame(&mut memory);
        }
        assert!(!voice.enabled);
        assert_eq!(voice.last_buffer_id, 4);
        assert_eq!(voice.current_buffer_id, 0, "no buffer is current once it stopped");
    }

    #[test]
    fn a_looping_buffer_keeps_playing() {
        let mut memory = silence();
        let mut voice = voice_with(&[]);
        voice.queue.push(Buffer { length: 160, looping: true, id: 9, ..buffer(9, 160) });
        for _ in 0..10 {
            voice.play_frame(&mut memory);
        }
        assert!(voice.enabled);
        assert_eq!(voice.current_buffer_id, 9);
    }

    /// a ramp of PCM16 samples, played at half speed, comes out with the
    /// samples between them filled in.
    #[test]
    fn pcm16_is_read_and_interpolated() {
        let bytes: Vec<u8> = (0..400i16).flat_map(|i| i.wrapping_mul(100).to_le_bytes()).collect();
        let mut memory = Samples(bytes);
        let mut voice = voice_with(&[(1, 400)]);
        voice.rate = 0.5;
        voice.play_frame(&mut memory); // starts the buffer
        voice.play_frame(&mut memory);
        assert_eq!(&voice.frame[..5], &[[0, 0], [50, 50], [100, 100], [150, 150], [200, 200]]);
    }

    #[test]
    fn stereo_pcm8_keeps_its_channels_apart() {
        let mut memory = Samples([1u8, 0xFF].repeat(200));
        let mut voice = voice_with(&[]);
        voice.queue.push(Buffer { format: Format::Pcm8, stereo: true, ..buffer(1, 200) });
        voice.play_frame(&mut memory);
        voice.play_frame(&mut memory);
        assert_eq!(voice.frame[0], [256, -256]);
    }

    /// the decoder follows the reference implementation's filter, and one
    /// buffer's history carries on into the next.
    #[test]
    fn adpcm_decodes_with_its_coefficients() {
        let mut coefficients = [0i16; 16];
        // predictor 1, y = x + y1
        coefficients[2] = 1 << 11;
        let frame = [0x10, 0x12, 0x30, 0, 0, 0, 0, 0];
        let mut history = [100, 0];
        let samples = decode_adpcm(&frame, 4, &coefficients, &mut history);
        assert_eq!(samples, [101, 103, 106, 106]);
        assert_eq!(history, [106, 106]);
    }

    /// an embedded buffer plays as long as it says, a sound of a second or
    /// two is common.
    #[test]
    fn an_embedded_buffer_keeps_its_length() {
        use crate::memory::{MemoryState, Permission};
        use zakuro_common::memory_map::{DSP_RAM_PADDR, DSP_RAM_SIZE, DSP_RAM_VADDR};
        let mut system = crate::System::new(crate::Config::default());
        let memory = &mut system.memory;
        memory.map(DSP_RAM_VADDR, DSP_RAM_PADDR, DSP_RAM_SIZE, Permission::RW, MemoryState::Static);
        let base = DSP_RAM_VADDR + 0x4_0000;
        write_dsp32(memory, base + config::ADDRESS, 0x2000_0000);
        write_dsp32(memory, base + config::LENGTH, 69_057);
        memory.write32(base + config::DIRTY, dirty::EMBEDDED_BUFFER);
        let mut voice = Voice::default();
        parse_config(&mut voice, memory, base, base + 0x1000);
        assert_eq!(voice.queue[0].length, 69_057);
    }

    fn with_filter_memory(test: impl FnOnce(&mut Memory, u32)) {
        use crate::memory::{MemoryState, Permission};
        use zakuro_common::memory_map::{DSP_RAM_PADDR, DSP_RAM_SIZE, DSP_RAM_VADDR};
        let mut system = crate::System::new(crate::Config::default());
        let memory = &mut system.memory;
        memory.map(DSP_RAM_VADDR, DSP_RAM_PADDR, DSP_RAM_SIZE, Permission::RW, MemoryState::Static);
        test(memory, DSP_RAM_VADDR + 0x4_0000);
    }

    #[test]
    fn reset_with_all_dirty_flags_and_zero_filter_coefficients_preserves_pcm() {
        with_filter_memory(|memory, base| {
            // libctru resets with every parameter dirty, disabled filters,
            // and zero-initialized coefficient arrays.
            memory.write32(base + config::DIRTY, u32::MAX);
            memory.write16(base + config::FILTERS_ENABLED, 0);
            for offset in (config::SIMPLE_FILTER..config::BUFFERS_DIRTY).step_by(2) {
                memory.write16(base + offset, 0);
            }
            let mut voice = Voice::default();
            parse_config(&mut voice, memory, base, base + 0x1000);
            assert_eq!(voice.filters.enabled, 0);

            voice.enabled = true;
            voice.queue.clear();
            voice.queue.push(Buffer { stereo: true, ..buffer(1, 320) });
            let mut samples = Samples([1000i16.to_le_bytes(), (-1000i16).to_le_bytes()].concat().repeat(320));
            voice.play_frame(&mut samples);
            voice.play_frame(&mut samples);
            assert!(voice.frame.iter().all(|sample| *sample == [1000, -1000]));
        });
    }

    #[test]
    fn coefficient_updates_do_not_enable_filters_and_are_kept_for_later_enable() {
        with_filter_memory(|memory, base| {
            let mut voice = Voice::default();
            // Even a stale enable field must not be applied without its dirty bit.
            memory.write16(base + config::FILTERS_ENABLED, 3);
            memory.write16(base + config::SIMPLE_FILTER, 1 << 14);
            memory.write16(base + config::SIMPLE_FILTER + 2, 0);
            for i in 0..5 {
                memory.write16(base + config::BIQUAD_FILTER + i * 2, if i == 4 { 1 << 13 } else { 0 });
            }
            memory.write32(base + config::DIRTY, dirty::SIMPLE_FILTER | dirty::BIQUAD_FILTER);
            parse_config(&mut voice, memory, base, base + 0x1000);
            let mut pcm = [[1000, -1000]; 2];
            voice.filters.process(&mut pcm);
            assert_eq!(pcm, [[1000, -1000]; 2]);
            assert_eq!(voice.filters.enabled, 0);

            // Coefficients written while disabled take effect when enabled,
            // independently for each filter and when both are selected.
            for (enabled, expected) in [(1, 500), (2, 500), (3, 250)] {
                memory.write16(base + config::FILTERS_ENABLED, enabled);
                memory.write32(base + config::DIRTY, dirty::FILTERS_ENABLED);
                parse_config(&mut voice, memory, base, base + 0x1000);
                let mut pcm = [[1000, -1000]; 2];
                voice.filters.process(&mut pcm);
                assert_eq!(pcm, [[expected, -expected]; 2]);
            }

            // Updating an already enabled filter still changes its processing.
            memory.write16(base + config::SIMPLE_FILTER, 0);
            memory.write32(base + config::DIRTY, dirty::SIMPLE_FILTER);
            parse_config(&mut voice, memory, base, base + 0x1000);
            let mut pcm = [[1000, -1000]; 2];
            voice.filters.process(&mut pcm);
            assert_eq!(voice.filters.enabled, 3);
            assert_eq!(pcm, [[0, 0]; 2]);
        });
    }

    #[test]
    fn gains_fade_in_over_a_frame_once_changed() {
        let mut voice = Voice { enabled: true, ..Voice::default() };
        voice.frame = [[1000, 1000]; FRAME_SAMPLES];
        voice.ramp[0] = Some([0.0; 4]);
        voice.gain[0] = [1.0; 4];
        let [mix, ..] = mix(&[voice.to_mix().unwrap()]);
        assert_eq!(mix[0], [0; 4]);
        assert_eq!(mix[FRAME_SAMPLES - 1], [1000; 4]);
        let [again, ..] = super::mix(&[voice.to_mix().unwrap()]);
        assert_eq!(again[0], [1000; 4], "the fade happens once");
    }
}
