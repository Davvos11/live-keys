//! A low-frequency oscillator for modulation: −1..=1, with an optional delay
//! before it starts and a fade-in after that.

use std::f32::consts::TAU;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Triangle,
    SawUp,
    SawDown,
    Square,
    /// A new random value each cycle.
    SampleAndHold,
}

#[derive(Debug, Clone, Copy)]
pub struct Lfo {
    waveform: Waveform,
    /// Cycles per sample.
    increment: f32,
    /// 0..1.
    phase: f32,
    /// Samples left before it starts.
    delay: u32,
    /// Fade-in per sample (1 = none), and how far it's faded in.
    fade_step: f32,
    fade: f32,
    held: f32,
    rng: u32,
    sample_rate: f32,
}

impl Lfo {
    /// Stopped at 0.
    pub const OFF: Lfo = Lfo {
        waveform: Waveform::Sine,
        increment: 0.0,
        phase: 0.0,
        delay: 0,
        fade_step: 1.0,
        fade: 0.0,
        held: 0.0,
        rng: 1,
        sample_rate: 48000.0,
    };

    /// `frequency` in Hz; `delay` and `fade` (time to full depth after the
    /// delay) in seconds; `phase` 0..1 where the cycle starts. `seed` varies
    /// sample-and-hold between voices.
    pub fn new(waveform: Waveform, frequency: f32, delay: f32, fade: f32, phase: f32, seed: u32, sample_rate: f32) -> Lfo {
        let fade_samples = fade.max(0.0) * sample_rate;
        let mut lfo = Lfo {
            waveform,
            increment: frequency.max(0.0) / sample_rate,
            phase: phase.rem_euclid(1.0),
            delay: (delay.max(0.0) * sample_rate) as u32,
            fade_step: if fade_samples < 1.0 { 1.0 } else { 1.0 / fade_samples },
            fade: if fade_samples < 1.0 { 1.0 } else { 0.0 },
            held: 0.0,
            rng: seed | 1,
            sample_rate,
        };
        lfo.held = lfo.random();
        lfo
    }

    pub fn set_frequency(&mut self, frequency: f32) {
        self.increment = frequency.max(0.0) / self.sample_rate;
    }

    /// The value now, without advancing.
    pub fn value(&self) -> f32 {
        if self.delay > 0 {
            return 0.0;
        }
        let p = self.phase;
        let shape = match self.waveform {
            Waveform::Sine => (p * TAU).sin(),
            // Starts at 0 rising, like the sine.
            Waveform::Triangle => {
                let q = (p + 0.25).fract();
                4.0 * (q - 0.5).abs() - 1.0
            }
            Waveform::SawUp => 2.0 * p - 1.0,
            Waveform::SawDown => 1.0 - 2.0 * p,
            Waveform::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::SampleAndHold => self.held,
        };
        shape * self.fade
    }

    /// Advances `samples` samples (an LFO is normally read once per control
    /// block); the new value.
    pub fn advance(&mut self, samples: usize) -> f32 {
        let mut samples = samples as u32;
        if self.delay > 0 {
            let waited = samples.min(self.delay);
            self.delay -= waited;
            samples -= waited;
        }
        if samples > 0 {
            self.fade = (self.fade + self.fade_step * samples as f32).min(1.0);
            let phase = self.phase + self.increment * samples as f32;
            if phase >= 1.0 && self.waveform == Waveform::SampleAndHold {
                self.held = self.random();
            }
            self.phase = phase.fract();
        }
        self.value()
    }

    fn random(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_delay_and_fade() {
        // 1 Hz at 1000 samples/s, starting after 0.5 s, full depth 1 s later.
        let mut lfo = Lfo::new(Waveform::Sine, 1.0, 0.5, 1.0, 0.0, 1, 1000.0);
        assert_eq!(lfo.advance(500), 0.0);
        // A quarter cycle in, half faded in: the sine's peak at half depth.
        assert!((lfo.advance(250) - 0.25).abs() < 1e-3);
        lfo.advance(750);
        // Then a full cycle on, at full depth.
        let a = lfo.advance(250);
        let b = lfo.advance(1000);
        assert!((a - 1.0).abs() < 1e-3 && (b - 1.0).abs() < 1e-3, "{a} {b}");
    }

    #[test]
    fn shapes_stay_in_range() {
        for waveform in [Waveform::Sine, Waveform::Triangle, Waveform::SawUp, Waveform::SawDown, Waveform::Square, Waveform::SampleAndHold] {
            let mut lfo = Lfo::new(waveform, 3.7, 0.0, 0.0, 0.3, 7, 1000.0);
            let mut seen = (f32::MAX, f32::MIN);
            for _ in 0..5000 {
                let v = lfo.advance(1);
                seen = (seen.0.min(v), seen.1.max(v));
            }
            assert!(seen.0 >= -1.0 && seen.1 <= 1.0 && seen.1 - seen.0 > 1.0, "{waveform:?}: {seen:?}");
        }
    }
}
