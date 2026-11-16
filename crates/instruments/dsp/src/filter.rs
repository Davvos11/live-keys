//! Filters: Andrew Simper's (Cytomic) trapezoidal state-variable filter in all
//! its modes, one-pole filters, and cascades for steeper slopes. Stable under
//! fast modulation, so cutoff can move every control block.
//!
//! Every SVF mode is the same core with a different mix of its outputs (input
//! `v0`, band `v1`, low `v2`): `out = m0·v0 + m1·v1 + m2·v2`.
//!
//! Up to two channels (stereo), each with its own state.

use std::f32::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// 6 dB/octave.
    LowPass1,
    HighPass1,
    /// 12 dB/octave.
    LowPass2,
    HighPass2,
    BandPass2,
    Notch2,
    AllPass2,
    /// A peak/dip of `gain_db` around the cutoff.
    Bell,
    LowShelf,
    HighShelf,
    /// 24 dB/octave (two 2-pole stages).
    LowPass4,
    HighPass4,
    /// 36 dB/octave (three 2-pole stages).
    LowPass6,
    HighPass6,
}

/// Butterworth Q values per stage, for a flat response before resonance.
const Q2: [f32; 1] = [std::f32::consts::FRAC_1_SQRT_2];
const Q4: [f32; 2] = [0.541_196_1, 1.306_563];
const Q6: [f32; 3] = [0.517_638_1, std::f32::consts::FRAC_1_SQRT_2, 1.931_851_6];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Svf { a1: f32, a2: f32, a3: f32, m0: f32, m1: f32, m2: f32 },
    OnePole { g: f32, high: bool },
}

const PASS: Stage = Stage::Svf { a1: 1.0, a2: 0.0, a3: 0.0, m0: 1.0, m1: 0.0, m2: 0.0 };

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    mode: FilterMode,
    stages: [Stage; 3],
    count: usize,
    /// Per channel, per stage: the two integrator states.
    state: [[[f32; 2]; 3]; 2],
}

impl Filter {
    /// A filter of `mode`, passing everything through until `set` is called.
    pub fn new(mode: FilterMode) -> Filter {
        let count = match mode {
            FilterMode::LowPass4 | FilterMode::HighPass4 => 2,
            FilterMode::LowPass6 | FilterMode::HighPass6 => 3,
            _ => 1,
        };
        Filter { mode, stages: [PASS; 3], count, state: [[[0.0; 2]; 3]; 2] }
    }

    pub fn mode(&self) -> FilterMode {
        self.mode
    }

    /// Sets the cutoff (Hz; clamped to 10 Hz..0.49·rate), the resonance (dB of
    /// peak above a flat response; 0 = Butterworth), and for bell and shelves
    /// the gain (dB). Cheap enough to call every control block.
    pub fn set(&mut self, cutoff: f32, resonance_db: f32, gain_db: f32, sample_rate: f32) {
        let cutoff = cutoff.clamp(10.0, sample_rate * 0.49);
        let g = (PI * cutoff / sample_rate).tan();
        let resonance = 10f32.powf(resonance_db.max(0.0) / 20.0);
        let a = 10f32.powf(gain_db / 40.0);
        let qs: &[f32] = match self.mode {
            FilterMode::LowPass4 | FilterMode::HighPass4 => &Q4,
            FilterMode::LowPass6 | FilterMode::HighPass6 => &Q6,
            _ => &Q2,
        };
        for (i, &q) in qs.iter().enumerate() {
            // Resonance goes on the last (highest-Q) stage.
            let q = if i + 1 == qs.len() { q * resonance } else { q };
            self.stages[i] = self.stage(g, q, a);
        }
    }

    fn stage(&self, g: f32, q: f32, a: f32) -> Stage {
        let k = 1.0 / q;
        let svf = |g: f32, k: f32, m0: f32, m1: f32, m2: f32| {
            let a1 = 1.0 / (1.0 + g * (g + k));
            let a2 = g * a1;
            Stage::Svf { a1, a2, a3: g * a2, m0, m1, m2 }
        };
        match self.mode {
            FilterMode::LowPass1 => Stage::OnePole { g: g / (1.0 + g), high: false },
            FilterMode::HighPass1 => Stage::OnePole { g: g / (1.0 + g), high: true },
            FilterMode::LowPass2 | FilterMode::LowPass4 | FilterMode::LowPass6 => svf(g, k, 0.0, 0.0, 1.0),
            FilterMode::HighPass2 | FilterMode::HighPass4 | FilterMode::HighPass6 => svf(g, k, 1.0, -k, -1.0),
            FilterMode::BandPass2 => svf(g, k, 0.0, 1.0, 0.0),
            FilterMode::Notch2 => svf(g, k, 1.0, -k, 0.0),
            FilterMode::AllPass2 => svf(g, k, 1.0, -2.0 * k, 0.0),
            FilterMode::Bell => {
                let k = 1.0 / (q * a);
                svf(g, k, 1.0, k * (a * a - 1.0), 0.0)
            }
            FilterMode::LowShelf => svf(g / a.sqrt(), k, 1.0, k * (a - 1.0), a * a - 1.0),
            FilterMode::HighShelf => svf(g * a.sqrt(), k, a * a, k * (1.0 - a) * a, 1.0 - a * a),
        }
    }

    /// Filters one sample of `channel` (0 or 1).
    #[inline]
    pub fn process(&mut self, channel: usize, x: f32) -> f32 {
        let state = &mut self.state[channel & 1];
        let mut x = x;
        for (stage, s) in self.stages[..self.count].iter().zip(state.iter_mut()) {
            x = match *stage {
                Stage::Svf { a1, a2, a3, m0, m1, m2 } => {
                    let v3 = x - s[1];
                    let v1 = a1 * s[0] + a2 * v3;
                    let v2 = s[1] + a2 * s[0] + a3 * v3;
                    s[0] = 2.0 * v1 - s[0];
                    s[1] = 2.0 * v2 - s[1];
                    m0 * x + m1 * v1 + m2 * v2
                }
                Stage::OnePole { g, high } => {
                    let v = (x - s[0]) * g;
                    let low = v + s[0];
                    s[0] = low + v;
                    if high { x - low } else { low }
                }
            };
        }
        x
    }

    /// Clears the state (silence in, silence out).
    pub fn reset(&mut self) {
        self.state = [[[0.0; 2]; 3]; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    /// Gain in dB of a steady sine at `frequency` through the filter.
    fn response(mode: FilterMode, cutoff: f32, resonance: f32, gain: f32, frequency: f32) -> f32 {
        let mut filter = Filter::new(mode);
        filter.set(cutoff, resonance, gain, RATE);
        let (mut power_in, mut power_out) = (0.0f64, 0.0f64);
        for i in 0..48000 {
            let x = (2.0 * PI as f64 * frequency as f64 * i as f64 / RATE as f64).sin() as f32;
            let y = filter.process(0, x);
            if i >= 24000 {
                power_in += (x * x) as f64;
                power_out += (y * y) as f64;
            }
        }
        10.0 * (power_out / power_in).log10() as f32
    }

    fn near(actual: f32, expected: f32, tolerance: f32) {
        assert!((actual - expected).abs() < tolerance, "{actual} dB, expected {expected} ± {tolerance}");
    }

    #[test]
    fn slopes_and_cutoff() {
        // −3 dB at the cutoff for Butterworth responses, then 6 dB/octave/pole.
        near(response(FilterMode::LowPass2, 1000.0, 0.0, 0.0, 1000.0), -3.0, 0.2);
        near(response(FilterMode::LowPass2, 1000.0, 0.0, 0.0, 100.0), 0.0, 0.1);
        near(response(FilterMode::LowPass2, 1000.0, 0.0, 0.0, 8000.0), -36.0, 2.0);
        near(response(FilterMode::LowPass4, 1000.0, 0.0, 0.0, 1000.0), -3.0, 0.2);
        near(response(FilterMode::LowPass4, 1000.0, 0.0, 0.0, 4000.0), -48.0, 2.0);
        near(response(FilterMode::LowPass6, 1000.0, 0.0, 0.0, 1000.0), -3.0, 0.2);
        near(response(FilterMode::LowPass1, 1000.0, 0.0, 0.0, 1000.0), -3.0, 0.2);
        near(response(FilterMode::HighPass2, 1000.0, 0.0, 0.0, 1000.0), -3.0, 0.2);
        near(response(FilterMode::HighPass4, 1000.0, 0.0, 0.0, 250.0), -48.0, 2.0);
        near(response(FilterMode::HighPass1, 1000.0, 0.0, 0.0, 10000.0), 0.0, 0.2);
    }

    #[test]
    fn resonance_bell_and_shelves() {
        // Resonance raises the peak at the cutoff by about its dB.
        near(response(FilterMode::LowPass2, 1000.0, 12.0, 0.0, 1000.0), 9.0, 0.5);
        near(response(FilterMode::Bell, 1000.0, 0.0, 6.0, 1000.0), 6.0, 0.2);
        near(response(FilterMode::Bell, 1000.0, 0.0, 6.0, 50.0), 0.0, 0.2);
        near(response(FilterMode::LowShelf, 1000.0, 0.0, -6.0, 50.0), -6.0, 0.2);
        near(response(FilterMode::LowShelf, 1000.0, 0.0, -6.0, 15000.0), 0.0, 0.2);
        near(response(FilterMode::HighShelf, 1000.0, 0.0, 6.0, 15000.0), 6.0, 0.2);
        assert!(response(FilterMode::Notch2, 1000.0, 0.0, 0.0, 1000.0) < -40.0);
        near(response(FilterMode::AllPass2, 1000.0, 0.0, 0.0, 3000.0), 0.0, 0.1);
    }
}
