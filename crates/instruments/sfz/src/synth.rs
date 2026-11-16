//! The player: MIDI in, stereo audio out, for one loaded [`Instrument`].
//!
//! Real-time safe: nothing here allocates, locks or does I/O once a `Synth` is
//! made. Voices come from a fixed pool; regions and samples are read through the
//! shared, immutable instrument.

use crate::envelope::Envelope;
use crate::instrument::{CC_COUNT, Instrument};
use crate::region::{Generator, LoopMode, OffMode, Region, Source, Target, Trigger};
use crate::sample::Sample;
use std::f32::consts::{FRAC_PI_4, SQRT_2};
use std::sync::Arc;

/// Fade for voices cut by `off_mode=fast` (and stolen ones), in seconds.
const FAST_OFF: f32 = 0.006;
const SUSTAIN_PEDAL: u16 = 64;

#[derive(Clone)]
struct Voice {
    active: bool,
    /// Start order, for picking the oldest.
    id: u64,
    region: u32,
    /// `None` for controller-triggered regions.
    key: Option<u8>,
    /// Note-off seen (or the pedal let it go), envelope releasing.
    released: bool,
    /// Note-off seen while the sustain pedal was down.
    sustained: bool,
    /// Cut by polyphony or `off_by`; fading out.
    killed: bool,
    /// Ignores note-offs (one-shot, release-triggered and controller-triggered).
    one_shot: bool,
    release_time: f32,

    generator: Option<Generator>,
    /// In frames (samples) or cycles (generators).
    pos: f64,
    /// Per output sample, before bend and live pitch modulation.
    step: f64,
    end: f64,
    loop_mode: LoopMode,
    loop_start: f64,
    loop_end: f64,
    noise: u32,

    env: Envelope,
    /// Velocity, crossfades, key tracking, release decay.
    base_gain: f32,
    /// Output = [ll·L + rl·R, lr·L + rr·R], ramped per block towards the target.
    matrix: [f32; 4],
}

impl Voice {
    const IDLE: Voice = Voice {
        active: false,
        id: 0,
        region: 0,
        key: None,
        released: false,
        sustained: false,
        killed: false,
        one_shot: false,
        release_time: 0.0,
        generator: None,
        pos: 0.0,
        step: 0.0,
        end: 0.0,
        loop_mode: LoopMode::NoLoop,
        loop_start: 0.0,
        loop_end: 0.0,
        noise: 1,
        env: Envelope::IDLE,
        base_gain: 0.0,
        matrix: [0.0; 4],
    };

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.sustained = false;
            self.env.release(self.release_time);
        }
    }

    fn kill(&mut self, region: &Region) {
        if self.killed {
            return;
        }
        self.killed = true;
        self.released = true;
        let seconds = match region.off_mode {
            OffMode::Fast => FAST_OFF,
            OffMode::Normal => self.release_time,
            OffMode::Time => region.off_time,
        };
        self.env.release(seconds);
    }
}

pub struct Synth {
    instrument: Arc<Instrument>,
    sample_rate: f32,
    voices: Box<[Voice]>,
    /// Normalized 0..=1.
    cc: [f32; CC_COUNT],
    /// −1..=1.
    bend: f32,
    sustain_down: bool,
    held: [bool; 128],
    held_count: u32,
    /// When each key was last pressed (in samples) and how hard, for release triggers.
    note_on_time: [u64; 128],
    note_velocity: [u8; 128],
    note_channel: [u8; 128],
    /// Let go while the pedal was down: its release comes when the pedal lifts.
    pending_release: [bool; 128],
    keyswitch: Option<u8>,
    seq_counters: Box<[u32]>,
    rng: u32,
    clock: u64,
    next_id: u64,
}

impl Synth {
    /// A player for `instrument` at `sample_rate`, with at most `max_voices`
    /// voices at once (the oldest are stolen beyond that).
    pub fn new(instrument: Arc<Instrument>, sample_rate: f32, max_voices: usize) -> Synth {
        let regions = instrument.regions.len();
        Synth {
            sample_rate,
            voices: vec![Voice::IDLE; max_voices.max(1)].into_boxed_slice(),
            cc: instrument.initial_cc,
            bend: 0.0,
            sustain_down: false,
            held: [false; 128],
            held_count: 0,
            note_on_time: [0; 128],
            note_velocity: [0; 128],
            note_channel: [0; 128],
            pending_release: [false; 128],
            keyswitch: instrument.keyswitch_default,
            seq_counters: vec![0; regions].into_boxed_slice(),
            rng: 0x9E37_79B9,
            clock: 0,
            next_id: 0,
            instrument,
        }
    }

    pub fn instrument(&self) -> &Arc<Instrument> {
        &self.instrument
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// A controller's current value, normalized.
    pub fn cc(&self, cc: u16) -> f32 {
        self.cc.get(cc as usize).copied().unwrap_or(0.0)
    }

    /// Handles one MIDI channel message (note on/off, CC, pitch bend).
    pub fn midi(&mut self, bytes: &[u8]) {
        let [status, a, b] = match *bytes {
            [s, a, b, ..] => [s, a & 0x7f, b & 0x7f],
            [s, a] => [s, a & 0x7f, 0],
            _ => return,
        };
        let channel = status & 0x0f;
        match status & 0xf0 {
            0x80 => self.note_off(channel, a),
            0x90 => self.note_on(channel, a, b),
            0xb0 => self.control_change(channel, a, b),
            0xe0 => self.pitch_bend(((b as i32) << 7 | a as i32) - 8192),
            _ => {}
        }
    }

    /// `channel` is 0-based.
    pub fn note_on(&mut self, channel: u8, key: u8, velocity: u8) {
        let key = key.min(127);
        if velocity == 0 {
            return self.note_off(channel, key);
        }
        let instrument = self.instrument.clone();
        if let Some((lo, hi)) = instrument.keyswitch_range
            && (lo..=hi).contains(&key)
        {
            self.keyswitch = Some(key);
        }
        let first = self.held_count == 0;
        if !self.held[key as usize] {
            self.held[key as usize] = true;
            self.held_count += 1;
        }
        self.pending_release[key as usize] = false;
        self.note_on_time[key as usize] = self.clock;
        self.note_velocity[key as usize] = velocity;
        self.note_channel[key as usize] = channel;
        let rand = self.random();
        for &r in &instrument.attack_by_key[key as usize] {
            let region = &instrument.regions[r as usize];
            match region.trigger {
                Trigger::First if !first => continue,
                Trigger::Legato if first => continue,
                _ => {}
            }
            if self.matches(region, r, channel, velocity, rand) {
                self.start_voice(&instrument, r, Some(key), velocity, 0.0);
            }
        }
    }

    pub fn note_off(&mut self, _channel: u8, key: u8) {
        let key = key.min(127) as usize;
        if self.held[key] {
            self.held[key] = false;
            self.held_count -= 1;
        }
        if self.sustain_down {
            self.pending_release[key] = true;
            for voice in self.voices.iter_mut() {
                if voice.active && voice.key == Some(key as u8) && !voice.one_shot && !voice.released {
                    voice.sustained = true;
                }
            }
            // Key-up noises happen now; the rest waits for the pedal.
            self.release_triggers(key as u8, Some(false));
        } else {
            self.release_voices(key as u8);
            self.release_triggers(key as u8, None);
        }
    }

    /// `channel` is 0-based.
    pub fn control_change(&mut self, channel: u8, cc: u8, value: u8) {
        match cc {
            120 => return self.all_sound_off(),
            121 => {
                self.bend = 0.0;
                self.set_cc(channel, SUSTAIN_PEDAL, 0.0);
                return;
            }
            123 => return self.all_notes_off(),
            _ => {}
        }
        self.set_cc(channel, cc as u16, value as f32 / 127.0);
    }

    /// Sets a controller to a normalized value (SFZ's high-resolution CCs).
    pub fn set_cc(&mut self, channel: u8, cc: u16, value: f32) {
        let Some(slot) = self.cc.get_mut(cc as usize) else { return };
        let old = *slot;
        let value = value.clamp(0.0, 1.0);
        *slot = value;
        if cc == SUSTAIN_PEDAL {
            let down = value >= 0.5;
            if self.sustain_down && !down {
                self.sustain_down = false;
                for key in 0..128u8 {
                    if std::mem::take(&mut self.pending_release[key as usize]) {
                        self.release_voices(key);
                        self.release_triggers(key, Some(true));
                    }
                }
            }
            self.sustain_down = down;
        }
        let instrument = self.instrument.clone();
        let (old, new) = (old * 127.0, value * 127.0);
        let mut rand = None;
        for &r in &instrument.cc_triggered {
            let region = &instrument.regions[r as usize];
            let entered = region.cc_triggers.iter().any(|t| t.cc == cc && t.contains(new) && !t.contains(old));
            if !entered {
                continue;
            }
            let rand = *rand.get_or_insert_with(|| self.random());
            if self.matches(region, r, channel, 127, rand) {
                self.start_voice(&instrument, r, None, 127, 0.0);
            }
        }
    }

    /// −8192..=8191.
    pub fn pitch_bend(&mut self, value: i32) {
        self.bend = (value as f32 / 8192.0).clamp(-1.0, 1.0);
    }

    /// Releases every note, ignoring the sustain pedal; no release samples.
    pub fn all_notes_off(&mut self) {
        self.held = [false; 128];
        self.held_count = 0;
        self.pending_release = [false; 128];
        for voice in self.voices.iter_mut() {
            if voice.active && !voice.one_shot {
                voice.release();
            }
        }
    }

    /// Silences everything at once.
    pub fn all_sound_off(&mut self) {
        self.all_notes_off();
        for voice in self.voices.iter_mut() {
            voice.active = false;
        }
    }

    /// Renders the next `left.len()` frames (overwriting both buffers).
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        let (left, right) = (&mut left[..frames], &mut right[..frames]);
        left.fill(0.0);
        right.fill(0.0);
        let Synth { instrument, voices, cc, bend, .. } = self;
        for voice in voices.iter_mut().filter(|v| v.active) {
            let region = &instrument.regions[voice.region as usize];
            let (matrix, pitch_cents) = live_values(instrument, region, cc, voice);
            let bend_cents = if *bend >= 0.0 { *bend * region.bend_up } else { -*bend * region.bend_down };
            let step = voice.step * 2f64.powf(((pitch_cents + bend_cents) / 1200.0) as f64);
            let sample = match region.source {
                Source::Sample(i) => Some(&instrument.samples[i as usize]),
                _ => None,
            };
            render_voice(voice, sample, step, matrix, left, right);
        }
        self.clock += frames as u64;
    }

    fn release_voices(&mut self, key: u8) {
        for voice in self.voices.iter_mut() {
            if voice.active && voice.key == Some(key) && !voice.one_shot {
                voice.release();
            }
        }
    }

    /// Starts the key's release-trigger regions: all of them, or only those that
    /// do (`Some(true)`) or don't (`Some(false)`) wait for the sustain pedal.
    fn release_triggers(&mut self, key: u8, waiting: Option<bool>) {
        let instrument = self.instrument.clone();
        let candidates = &instrument.release_by_key[key as usize];
        if candidates.is_empty() {
            return;
        }
        let held = (self.clock - self.note_on_time[key as usize]) as f32 / self.sample_rate;
        let velocity = self.note_velocity[key as usize];
        let channel = self.note_channel[key as usize];
        if velocity == 0 {
            return; // never pressed
        }
        let rand = self.random();
        for &r in candidates {
            let region = &instrument.regions[r as usize];
            if waiting.is_some_and(|w| region.waits_for_pedal() != w) {
                continue;
            }
            if self.matches(region, r, channel, velocity, rand) {
                self.start_voice(&instrument, r, Some(key), velocity, held);
            }
        }
    }

    fn matches(&mut self, region: &Region, r: u32, channel: u8, velocity: u8, rand: f32) -> bool {
        let channel = channel + 1;
        if !(region.lochan..=region.hichan).contains(&channel) || !(region.lovel..=region.hivel).contains(&velocity) {
            return false;
        }
        if region.sw_last.is_some_and(|sw| self.keyswitch != Some(sw)) {
            return false;
        }
        if !region.cc_conditions.iter().all(|c| c.contains(self.cc[c.cc as usize % CC_COUNT] * 127.0)) {
            return false;
        }
        if rand < region.lorand || (rand >= region.hirand && region.hirand < 1.0) {
            return false;
        }
        if region.seq_length > 1 {
            let counter = &mut self.seq_counters[r as usize];
            let position = *counter % region.seq_length + 1;
            *counter = counter.wrapping_add(1);
            return position == region.seq_position;
        }
        true
    }

    fn start_voice(&mut self, instrument: &Instrument, r: u32, key: Option<u8>, velocity: u8, held: f32) {
        let region = &instrument.regions[r as usize];

        // Voices this one turns off: `off_by` this group, then the polyphony limits.
        for voice in self.voices.iter_mut() {
            if voice.active && !voice.killed {
                let other = &instrument.regions[voice.region as usize];
                if other.off_by == Some(region.group) {
                    voice.kill(other);
                }
            }
        }
        if let (Some(limit), Some(key)) = (region.note_polyphony, key) {
            self.limit_voices(instrument, limit, |v, other| v.key == Some(key) && other.group == region.group);
        }
        if let Some(limit) = region.polyphony {
            self.limit_voices(instrument, limit, |_, other| other.group == region.group);
        }

        let index = self.free_voice();
        let sample_rate = self.sample_rate;
        let cc = &self.cc;
        let modulation = |target: Target| -> f32 {
            region
                .mods
                .iter()
                .filter(|m| m.target == target)
                .map(|m| m.depth * instrument.curve(m.curve).eval(cc[m.cc as usize % CC_COUNT]))
                .sum()
        };

        let vel = velocity as f32 / 127.0;
        let curve = instrument.velcurves[r as usize].as_ref().map_or(vel * vel, |c| c.eval(vel));
        let veltrack = ((region.amp_veltrack + modulation(Target::AmpVeltrack)) / 100.0).clamp(-1.0, 1.0);
        let velocity_gain = if veltrack >= 0.0 { 1.0 - veltrack * (1.0 - curve) } else { -veltrack * (1.0 - curve) };
        let fade = crossfade_in(velocity, region.xfin_lovel, region.xfin_hivel)
            * crossfade_out(velocity, region.xfout_lovel, region.xfout_hivel);
        let fade = if region.xf_power { fade.sqrt() } else { fade };
        let amp_key = key.unwrap_or(region.amp_keycenter) as f32 - region.amp_keycenter as f32;
        let mut db = amp_key * region.amp_keytrack;
        if region.is_release() {
            db -= region.rt_decay * held;
        }
        let base_gain = velocity_gain * fade * db_to_gain(db);

        let pitch_key = key.unwrap_or(region.pitch_keycenter) as f32 - region.pitch_keycenter as f32;
        let semitones = pitch_key * region.pitch_keytrack / 100.0
            + region.transpose as f32
            + (region.tune + region.pitch_veltrack * vel) / 100.0;
        let ratio = 2f64.powf(semitones as f64 / 12.0);

        let mut times = region.ampeg;
        times.delay += modulation(Target::EgDelay);
        times.attack += modulation(Target::EgAttack);
        times.hold += modulation(Target::EgHold);
        times.decay += modulation(Target::EgDecay);
        times.sustain = (times.sustain + modulation(Target::EgSustain) / 100.0).clamp(0.0, 1.0);
        times.release = (times.release + modulation(Target::EgRelease)).max(0.0);

        let loop_mode = region.effective_loop_mode();
        let mut voice = Voice {
            active: true,
            id: self.next_id,
            region: r,
            key,
            released: false,
            sustained: false,
            killed: false,
            one_shot: loop_mode == LoopMode::OneShot || region.is_release() || key.is_none(),
            release_time: times.release,
            env: Envelope::start(&times, sample_rate),
            base_gain,
            noise: self.rng | 1,
            ..Voice::IDLE
        };
        match region.source {
            Source::Sample(i) => {
                let sample = &instrument.samples[i as usize];
                let frames = sample.frames() as f64;
                voice.step = ratio * sample.rate() as f64 / sample_rate as f64;
                voice.end = region.end.map_or(frames, |e| (e as f64 + 1.0).min(frames));
                let offset = region.offset as f64 + modulation(Target::Offset) as f64;
                voice.pos = offset.clamp(0.0, voice.end);
                voice.loop_mode = loop_mode;
                voice.loop_start = region.loop_start.map_or(0.0, |s| s as f64).min(voice.end);
                voice.loop_end = region.loop_end.map_or(voice.end, |e| (e as f64 + 1.0).min(voice.end));
                if voice.loop_end <= voice.loop_start {
                    voice.loop_mode = LoopMode::NoLoop;
                }
            }
            Source::Generator(generator) => {
                let frequency = 440.0 * 2f64.powf((region.pitch_keycenter as f64 - 69.0) / 12.0);
                voice.generator = Some(generator);
                voice.step = frequency * ratio / sample_rate as f64;
                voice.end = f64::INFINITY;
            }
            Source::None => return,
        }
        self.next_id += 1;
        // Start at the target gains rather than ramping up from silence.
        voice.matrix = live_values(instrument, region, &self.cc, &voice).0;
        self.voices[index] = voice;
    }

    /// Fades out the oldest voices that `same` picks until fewer than `limit` are left.
    fn limit_voices(&mut self, instrument: &Instrument, limit: u32, same: impl Fn(&Voice, &Region) -> bool) {
        loop {
            let mut count = 0;
            let mut oldest: Option<usize> = None;
            for (i, voice) in self.voices.iter().enumerate() {
                if voice.active && !voice.killed && same(voice, &instrument.regions[voice.region as usize]) {
                    count += 1;
                    if oldest.is_none_or(|o| self.voices[o].id > voice.id) {
                        oldest = Some(i);
                    }
                }
            }
            match oldest {
                Some(i) if count >= limit => {
                    let region = &instrument.regions[self.voices[i].region as usize];
                    self.voices[i].kill(region);
                }
                _ => return,
            }
        }
    }

    /// A free voice, or else the one that matters least: the oldest fading one,
    /// then the oldest released one, then the oldest.
    fn free_voice(&mut self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            return i;
        }
        let rank = |v: &Voice| (if v.killed { 0 } else if v.released { 1 } else { 2 }, v.id);
        (0..self.voices.len()).min_by_key(|&i| rank(&self.voices[i])).unwrap_or(0)
    }

    /// 0..1, for `lorand`/`hirand`.
    fn random(&mut self) -> f32 {
        self.rng = xorshift(self.rng);
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }
}

fn xorshift(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn crossfade_in(velocity: u8, lo: u8, hi: u8) -> f32 {
    if velocity >= hi {
        1.0
    } else if velocity <= lo {
        0.0
    } else {
        (velocity - lo) as f32 / (hi - lo) as f32
    }
}

fn crossfade_out(velocity: u8, lo: u8, hi: u8) -> f32 {
    if velocity <= lo {
        1.0
    } else if velocity >= hi {
        0.0
    } else {
        (hi - velocity) as f32 / (hi - lo) as f32
    }
}

/// The voice's output matrix and pitch offset (cents) for the current
/// controller values: what follows the controllers while it plays.
fn live_values(instrument: &Instrument, region: &Region, cc: &[f32; CC_COUNT], voice: &Voice) -> ([f32; 4], f32) {
    let mut amplitude = region.amplitude / 100.0;
    let (mut volume, mut pan, mut width, mut position, mut pitch) =
        (region.volume, region.pan, region.width, region.position, 0.0);
    for m in &region.mods {
        if !m.target.is_live() {
            continue;
        }
        let value = m.depth * instrument.curve(m.curve).eval(cc[m.cc as usize % CC_COUNT]);
        match m.target {
            Target::Amplitude => amplitude *= value / 100.0,
            Target::Volume => volume += value,
            Target::Pan => pan += value,
            Target::Width => width += value,
            Target::Position => position += value,
            Target::Pitch => pitch += value,
            _ => {}
        }
    }
    let gain = voice.base_gain * amplitude * db_to_gain(volume);
    // Width mixes the channels (100: as recorded, 0: mono, −100: swapped), then
    // pan and position balance them; centred is unity on both sides.
    let w = (width / 100.0).clamp(-1.0, 1.0);
    let (a, b) = ((1.0 + w) / 2.0, (1.0 - w) / 2.0);
    let p = ((pan + position) / 100.0).clamp(-1.0, 1.0);
    let angle = (p + 1.0) * FRAC_PI_4;
    let (pl, pr) = (angle.cos() * SQRT_2 * gain, angle.sin() * SQRT_2 * gain);
    ([pl * a, pl * b, pr * b, pr * a], pitch)
}

/// 4-point, 3rd-order Hermite interpolation between `x0` and `x1`.
#[inline]
fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b = w + a;
    ((a * t - b) * t + c) * t + x0
}

fn render_voice(
    voice: &mut Voice,
    sample: Option<&Sample>,
    step: f64,
    target: [f32; 4],
    left: &mut [f32],
    right: &mut [f32],
) {
    let frames = left.len();
    let ramp = 1.0 / frames.max(1) as f32;
    let delta: [f32; 4] = std::array::from_fn(|i| (target[i] - voice.matrix[i]) * ramp);
    let mut m = voice.matrix;
    for i in 0..frames {
        let env = voice.env.next();
        if voice.env.is_done() {
            voice.active = false;
            break;
        }
        for (m, d) in m.iter_mut().zip(&delta) {
            *m += d;
        }
        let (l, r) = match sample {
            Some(sample) => {
                let index = voice.pos as usize;
                let t = (voice.pos - index as f64) as f32;
                let w = sample.window(index);
                let looping = match voice.loop_mode {
                    LoopMode::Continuous => true,
                    LoopMode::Sustain => !voice.released,
                    _ => false,
                };
                let frame = if sample.channels() == 2 {
                    (hermite(w[0], w[2], w[4], w[6], t), hermite(w[1], w[3], w[5], w[7], t))
                } else {
                    let s = hermite(w[0], w[1], w[2], w[3], t);
                    (s, s)
                };
                voice.pos += step;
                if looping && voice.pos >= voice.loop_end {
                    voice.pos -= voice.loop_end - voice.loop_start;
                } else if voice.pos >= voice.end {
                    voice.active = false;
                }
                frame
            }
            None => {
                let phase = voice.pos as f32;
                let s = match voice.generator {
                    Some(Generator::Sine) => (phase * std::f32::consts::TAU).sin(),
                    Some(Generator::Triangle) => 1.0 - 4.0 * (phase - 0.5).abs(),
                    Some(Generator::Saw) => 2.0 * phase - 1.0,
                    Some(Generator::Square) => {
                        if phase < 0.5 {
                            1.0
                        } else {
                            -1.0
                        }
                    }
                    Some(Generator::Noise) => {
                        voice.noise = xorshift(voice.noise);
                        voice.noise as f32 / u32::MAX as f32 * 2.0 - 1.0
                    }
                    Some(Generator::Silence) | None => 0.0,
                };
                voice.pos = (voice.pos + step).fract();
                (s, s)
            }
        };
        left[i] += env * (m[0] * l + m[1] * r);
        right[i] += env * (m[2] * l + m[3] * r);
        if !voice.active {
            break;
        }
    }
    voice.matrix = target;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(sfz: &str) -> Synth {
        let instrument = Arc::new(Instrument::from_sfz(sfz, None).unwrap());
        Synth::new(instrument, 1000.0, 16)
    }

    fn peak(synth: &mut Synth, frames: usize) -> f32 {
        let (mut l, mut r) = (vec![0.0; frames], vec![0.0; frames]);
        synth.render(&mut l, &mut r);
        l.iter().chain(&r).fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn note_on_and_release() {
        let mut s = synth("<region> sample=*square ampeg_release=0.01");
        s.note_on(0, 60, 127);
        assert!(peak(&mut s, 100) > 0.9);
        s.note_off(0, 60);
        peak(&mut s, 100);
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn sustain_pedal_holds_and_releases_triggers_on_lift() {
        let mut s = synth("<region> sample=*square <region> trigger=release sample=*sine loop_mode=one_shot ampeg_decay=0.05 ampeg_sustain=0");
        s.control_change(0, 64, 127);
        s.note_on(0, 60, 100);
        s.note_off(0, 60);
        peak(&mut s, 50);
        assert_eq!(s.active_voices(), 1, "held by the pedal, no release trigger yet");
        s.control_change(0, 64, 0);
        assert_eq!(s.active_voices(), 2, "release trigger on lift");
    }

    #[test]
    fn key_up_noises_ignore_the_pedal() {
        let mut s = synth(
            "<region> sample=*square
             <region> trigger=release_key sample=*noise loop_mode=one_shot
             <region> trigger=release sustain_sw=off sample=*saw loop_mode=one_shot",
        );
        s.control_change(0, 64, 127);
        s.note_on(0, 60, 100);
        s.note_off(0, 60);
        assert_eq!(s.active_voices(), 3, "both key-up regions at once, the note held");
        s.control_change(0, 64, 0);
        assert_eq!(s.active_voices(), 3, "nothing more on lift");
    }

    #[test]
    fn keyswitches_cc_conditions_and_note_polyphony() {
        let mut s = synth(
            "<global> sw_lokey=24 sw_hikey=25 sw_default=24 note_polyphony=1
             <region> sw_last=24 sample=*sine
             <region> sw_last=25 locc1=64 sample=*saw",
        );
        s.note_on(0, 60, 100);
        assert_eq!(s.active_voices(), 1);
        s.note_on(0, 25, 100); // switch
        s.note_on(0, 60, 100); // CC 1 is 0: the saw doesn't play, the sine plays on
        let playing = |s: &Synth| s.voices.iter().filter(|v| v.active && !v.killed).map(|v| v.region).collect::<Vec<_>>();
        assert_eq!(playing(&s), [0]);
        s.control_change(0, 1, 100);
        s.note_on(0, 60, 100); // the saw, cutting the sine (note_polyphony=1)
        assert_eq!(playing(&s), [1]);
    }

    #[test]
    fn controller_triggered_regions_fire_on_entering_the_range() {
        let mut s = synth("<region> lokey=-1 hikey=-1 on_locc64=127 on_hicc64=127 sample=*noise loop_mode=one_shot");
        s.control_change(0, 64, 100);
        assert_eq!(s.active_voices(), 0);
        s.control_change(0, 64, 127);
        s.control_change(0, 64, 127);
        assert_eq!(s.active_voices(), 1);
    }

    #[test]
    fn amplitude_follows_its_controller() {
        let mut s = synth("<region> sample=*square amplitude_oncc7=100");
        s.note_on(0, 60, 127);
        let full = peak(&mut s, 100);
        assert!((full - 100.0 / 127.0).abs() < 0.01, "{full}");
        s.control_change(0, 7, 0);
        peak(&mut s, 10); // ramps down over a block
        assert!(peak(&mut s, 10) < 1e-6);
    }
}
