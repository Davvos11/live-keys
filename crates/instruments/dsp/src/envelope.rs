//! A DAHDSR envelope: delay, linear attack, hold, exponential decay to the
//! sustain level, exponential release. For amplitude (voice ends when it's
//! done) or as a modulation source (0..=1, scaled by a depth).

/// Below this the envelope counts as silent and finishes (−80 dB).
const SILENT: f32 = 1e-4;

/// Envelope settings: times in seconds, sustain 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dahdsr {
    pub delay: f32,
    pub attack: f32,
    pub hold: f32,
    /// Time to fall 60 dB towards the sustain level.
    pub decay: f32,
    pub sustain: f32,
    /// Time to fall 60 dB after release.
    pub release: f32,
}

impl Default for Dahdsr {
    /// Instant attack, full sustain, a 1 ms release.
    fn default() -> Self {
        Dahdsr { delay: 0.0, attack: 0.0, hold: 0.0, decay: 0.0, sustain: 1.0, release: 0.001 }
    }
}

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
pub struct Envelope {
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
    (-6.907_755 / samples).exp() // ln(1e-3)
}

impl Envelope {
    /// An envelope that's done (silent) until started.
    pub const IDLE: Envelope = Envelope {
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

    /// Starts from silence.
    pub fn start(settings: &Dahdsr, sample_rate: f32) -> Envelope {
        let samples = |seconds: f32| (seconds.max(0.0) * sample_rate) as u32;
        let attack = samples(settings.attack);
        Envelope {
            stage: Stage::Delay,
            level: 0.0,
            counter: samples(settings.delay),
            attack_step: if attack == 0 { 1.0 } else { 1.0 / attack as f32 },
            hold: samples(settings.hold),
            decay_coef: coefficient(settings.decay, sample_rate),
            sustain: settings.sustain.clamp(0.0, 1.0),
            release_coef: coefficient(settings.release, sample_rate),
            sample_rate,
        }
    }

    /// Moves to the release stage, from wherever the envelope is, with the
    /// release time it started with.
    pub fn release(&mut self) {
        if self.stage != Stage::Done {
            self.stage = Stage::Release;
        }
    }

    /// Moves to the release stage, taking `seconds` (to −60 dB).
    pub fn release_in(&mut self, seconds: f32) {
        if self.stage != Stage::Done {
            self.stage = Stage::Release;
            self.release_coef = coefficient(seconds, self.sample_rate);
        }
    }

    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    pub fn is_releasing(&self) -> bool {
        matches!(self.stage, Stage::Release | Stage::Done)
    }

    /// The current level, without advancing.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Advances one sample; the new level.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        match self.stage {
            Stage::Delay => {
                if self.counter == 0 {
                    self.stage = Stage::Attack;
                    return self.tick();
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

    /// Advances `samples` samples (for a modulation envelope read at control
    /// rate); the new level.
    pub fn advance(&mut self, samples: usize) -> f32 {
        for _ in 0..samples {
            if self.stage == Stage::Sustain || self.stage == Stage::Done {
                break;
            }
            self.tick();
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_decay_sustain_release() {
        let settings = Dahdsr { delay: 0.0, attack: 0.01, hold: 0.0, decay: 0.1, sustain: 0.5, release: 0.1 };
        let mut env = Envelope::start(&settings, 1000.0);
        let levels: Vec<f32> = (0..10).map(|_| env.tick()).collect();
        assert!((levels[4] - 0.5).abs() < 1e-6 && (levels[9] - 1.0).abs() < 1e-6, "{levels:?}");
        env.advance(1000);
        assert!((env.tick() - 0.5).abs() < 1e-3);
        env.release();
        // −60 dB after the release time.
        env.advance(100);
        assert!((env.tick() / 0.5 - 1e-3).abs() < 1e-4);
        env.advance(100);
        assert!(env.is_done());
    }
}
