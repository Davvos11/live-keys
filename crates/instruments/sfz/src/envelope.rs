//! The amplitude envelope: delay, linear attack, hold, exponential decay to the
//! sustain level, exponential release.

use crate::region::EnvelopeTimes;

/// Below this the envelope counts as silent and the voice ends (−80 dB).
const SILENT: f32 = 1e-4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Delay,
    Attack,
    Hold,
    Decay,
    Sustain,
    Release,
    Done,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Envelope {
    stage: Stage,
    level: f32,
    /// Samples left in the delay or hold stage.
    counter: u32,
    attack_step: f32,
    hold: u32,
    decay_coef: f32,
    sustain: f32,
    release_coef: f32,
    sample_rate: f32,
}

/// The per-sample factor that takes a level down 60 dB in `seconds`.
fn coefficient(seconds: f32, sample_rate: f32) -> f32 {
    let samples = (seconds * sample_rate).max(1.0);
    (-6.907_755 / samples).exp() // ln(1e-3) = −6.9078
}

impl Envelope {
    pub(crate) const IDLE: Envelope = Envelope {
        stage: Stage::Done,
        level: 0.0,
        counter: 0,
        attack_step: 1.0,
        hold: 0,
        decay_coef: 0.0,
        sustain: 1.0,
        release_coef: 0.0,
        sample_rate: 48000.0,
    };

    pub(crate) fn start(times: &EnvelopeTimes, sample_rate: f32) -> Envelope {
        let samples = |seconds: f32| (seconds.max(0.0) * sample_rate) as u32;
        let attack = samples(times.attack);
        Envelope {
            stage: Stage::Delay,
            level: 0.0,
            counter: samples(times.delay),
            attack_step: if attack == 0 { 1.0 } else { 1.0 / attack as f32 },
            hold: samples(times.hold),
            decay_coef: coefficient(times.decay, sample_rate),
            sustain: times.sustain.clamp(0.0, 1.0),
            release_coef: coefficient(times.release, sample_rate),
            sample_rate,
        }
    }

    /// Moves to the release stage, from wherever the envelope is, taking
    /// `seconds` (to −60 dB).
    pub(crate) fn release(&mut self, seconds: f32) {
        if self.stage != Stage::Done {
            self.stage = Stage::Release;
            self.release_coef = coefficient(seconds, self.sample_rate);
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    #[inline]
    pub(crate) fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Delay => {
                if self.counter == 0 {
                    self.stage = Stage::Attack;
                    return self.next();
                }
                self.counter -= 1;
            }
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Hold;
                    self.counter = self.hold;
                }
            }
            Stage::Hold => {
                if self.counter == 0 {
                    self.stage = Stage::Decay;
                } else {
                    self.counter -= 1;
                }
            }
            Stage::Decay => {
                self.level = self.sustain + (self.level - self.sustain) * self.decay_coef;
                if self.level - self.sustain < SILENT {
                    self.level = self.sustain;
                    self.stage = if self.sustain < SILENT { Stage::Done } else { Stage::Sustain };
                }
            }
            Stage::Sustain => {}
            Stage::Release => {
                self.level *= self.release_coef;
                if self.level < SILENT {
                    self.level = 0.0;
                    self.stage = Stage::Done;
                }
            }
            Stage::Done => return 0.0,
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_decay_sustain_release() {
        let times = EnvelopeTimes { delay: 0.0, attack: 0.01, hold: 0.0, decay: 0.1, sustain: 0.5, release: 0.1 };
        let mut env = Envelope::start(&times, 1000.0);
        let levels: Vec<f32> = (0..10).map(|_| env.next()).collect();
        assert!((levels[4] - 0.5).abs() < 1e-6 && (levels[9] - 1.0).abs() < 1e-6, "{levels:?}");
        for _ in 0..1000 {
            env.next();
        }
        assert!((env.next() - 0.5).abs() < 1e-3);
        env.release(0.1);
        // −60 dB after the release time.
        for _ in 0..100 {
            env.next();
        }
        assert!((env.next() / 0.5 - 1e-3).abs() < 1e-4);
        for _ in 0..100 {
            env.next();
        }
        assert!(env.is_done());
    }
}
