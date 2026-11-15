//! The metronome: its settings, and the click it mixes into the output on the
//! audio thread. It keeps its own tempo; plugins don't see it (they still get a
//! fixed tempo, see CLAUDE.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Slowest and fastest tempo the metronome takes, in beats per minute.
pub const MIN_BPM: f32 = 20.0;
pub const MAX_BPM: f32 = 300.0;

/// Which clicks the metronome adds between its beats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Subdivision {
    /// Only the beats.
    #[default]
    Quarter,
    Eighth,
    Sixteenth,
    QuarterTriplet,
    EighthTriplet,
    SixteenthTriplet,
    DottedQuarter,
    DottedEighth,
}

impl Subdivision {
    pub const ALL: [Subdivision; 8] = [
        Subdivision::Quarter,
        Subdivision::Eighth,
        Subdivision::Sixteenth,
        Subdivision::QuarterTriplet,
        Subdivision::EighthTriplet,
        Subdivision::SixteenthTriplet,
        Subdivision::DottedQuarter,
        Subdivision::DottedEighth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Subdivision::Quarter => "Quarter notes",
            Subdivision::Eighth => "Eighth notes",
            Subdivision::Sixteenth => "Sixteenth notes",
            Subdivision::QuarterTriplet => "Quarter triplets",
            Subdivision::EighthTriplet => "Eighth triplets",
            Subdivision::SixteenthTriplet => "Sixteenth triplets",
            Subdivision::DottedQuarter => "Dotted quarters",
            Subdivision::DottedEighth => "Dotted eighths",
        }
    }

    /// The time between two clicks, in beats.
    pub fn interval(self) -> f64 {
        match self {
            Subdivision::Quarter => 1.0,
            Subdivision::Eighth => 0.5,
            Subdivision::Sixteenth => 0.25,
            Subdivision::QuarterTriplet => 2.0 / 3.0,
            Subdivision::EighthTriplet => 1.0 / 3.0,
            Subdivision::SixteenthTriplet => 1.0 / 6.0,
            Subdivision::DottedQuarter => 1.5,
            Subdivision::DottedEighth => 0.75,
        }
    }
}

/// What the metronome plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetronomeSettings {
    pub playing: bool,
    /// Beats per minute, clamped to [`MIN_BPM`]..=[`MAX_BPM`].
    pub bpm: f32,
    /// Beats per bar; the first one is accented.
    pub beats_per_bar: u8,
    pub subdivision: Subdivision,
    /// Linear gain of the click.
    pub level: f32,
}

impl Default for MetronomeSettings {
    fn default() -> Self {
        Self {
            playing: false,
            bpm: 120.0,
            beats_per_bar: 4,
            subdivision: Subdivision::Quarter,
            level: 0.5,
        }
    }
}

/// The three sounds: a bar's first beat, the other beats, the clicks between.
#[derive(Clone, Copy)]
enum Accent {
    Bar,
    Beat,
    Between,
}

impl Accent {
    /// Pitch (Hz) and loudness.
    fn voice(self) -> (f32, f32) {
        match self {
            Accent::Bar => (1760.0, 1.0),
            Accent::Beat => (1320.0, 0.7),
            Accent::Between => (990.0, 0.4),
        }
    }
}

/// The metronome on the audio thread: where it is in the bar, and the click
/// sounding now (a decaying sine). Allocation-free.
pub(crate) struct Click {
    settings: MetronomeSettings,
    sample_rate: f64,
    /// Beats since the start of the bar.
    position: f64,
    frequency: f32,
    amplitude: f32,
    phase: f32,
    /// Per-sample amplitude factor: about 15 ms to -9 dB.
    decay: f32,
    /// The beat sounding now, plus one; 0 while stopped. Read by the GUI.
    beat: Arc<AtomicU32>,
}

impl Click {
    pub fn new(settings: MetronomeSettings, sample_rate: f64, beat: Arc<AtomicU32>) -> Self {
        let mut click = Self {
            settings: MetronomeSettings { playing: false, ..settings },
            sample_rate,
            position: 0.0,
            frequency: 0.0,
            amplitude: 0.0,
            phase: 0.0,
            decay: (-1.0 / (0.015 * sample_rate)).exp() as f32,
            beat,
        };
        click.set(settings);
        click
    }

    pub fn set(&mut self, mut settings: MetronomeSettings) {
        settings.bpm = settings.bpm.clamp(MIN_BPM, MAX_BPM);
        settings.beats_per_bar = settings.beats_per_bar.max(1);
        if settings.playing && !self.settings.playing {
            self.position = 0.0;
            self.trigger(Accent::Bar);
        }
        self.settings = settings;
        self.publish();
    }

    fn trigger(&mut self, accent: Accent) {
        let (frequency, amplitude) = accent.voice();
        self.frequency = frequency;
        self.amplitude = amplitude;
        self.phase = 0.0;
    }

    fn publish(&self) {
        let beat = if self.settings.playing { self.position as u32 + 1 } else { 0 };
        self.beat.store(beat, Ordering::Relaxed);
    }

    /// Adds the click to the interleaved `data`.
    pub fn render(&mut self, data: &mut [f32], channels: usize) {
        if !self.settings.playing && self.amplitude < 1e-4 {
            return;
        }
        let step = f64::from(self.settings.bpm) / 60.0 / self.sample_rate;
        let bar = f64::from(self.settings.beats_per_bar);
        let interval = self.settings.subdivision.interval();
        let phase_step = std::f32::consts::TAU * self.frequency / self.sample_rate as f32;
        let mut phase_step = phase_step;
        for frame in data.chunks_mut(channels) {
            if self.settings.playing {
                let previous = self.position;
                self.position += step;
                let accent = if self.position >= bar {
                    self.position -= bar;
                    Some(Accent::Bar)
                } else if previous.floor() != self.position.floor() {
                    Some(Accent::Beat)
                } else if (previous / interval).floor() != (self.position / interval).floor() {
                    Some(Accent::Between)
                } else {
                    None
                };
                if let Some(accent) = accent {
                    self.trigger(accent);
                    phase_step = std::f32::consts::TAU * self.frequency / self.sample_rate as f32;
                }
            }
            if self.amplitude < 1e-4 {
                continue;
            }
            let sample = self.phase.sin() * self.amplitude * self.settings.level;
            self.phase = (self.phase + phase_step) % std::f32::consts::TAU;
            self.amplitude *= self.decay;
            for out in frame {
                *out += sample;
            }
        }
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames at which clicks start, over `frames` frames at 1024 Hz (so a
    /// step is exact in binary).
    fn onsets(settings: MetronomeSettings, frames: usize) -> Vec<usize> {
        let mut click = Click::new(settings, 1024.0, Arc::default());
        let mut onsets = vec![0];
        let mut last = click.amplitude;
        let mut data = [0.0f32];
        for frame in 1..frames {
            click.render(&mut data, 1);
            if click.amplitude > last {
                onsets.push(frame);
            }
            last = click.amplitude;
        }
        onsets
    }

    #[test]
    fn clicks_on_beats_and_subdivisions() {
        // 120 BPM: a beat every 512 frames.
        let quarters = MetronomeSettings { playing: true, ..Default::default() };
        assert_eq!(onsets(quarters, 2100), [0, 512, 1024, 1536, 2048]);
        let dotted = MetronomeSettings { subdivision: Subdivision::DottedQuarter, beats_per_bar: 3, ..quarters };
        // Beats at 0/512/1024, the dotted click at 1.5 beats, then the next bar.
        assert_eq!(onsets(dotted, 1600), [0, 512, 768, 1024, 1536]);
    }
}
