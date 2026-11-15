//! The sidebar's metronome: its saved settings, the time signatures and
//! subdivisions it offers, and tap tempo.

use std::time::{Duration, Instant};

use live_keys_engine::{MAX_BPM, MIN_BPM, MetronomeSettings, Subdivision};
use serde::{Deserialize, Serialize};

/// The time signatures offered, as (beats per bar, beat unit). The tempo
/// counts the signature's beats: in 6/8, eighths.
pub const SIGNATURES: [(u8, u8); 8] = [(2, 4), (3, 4), (4, 4), (5, 4), (6, 8), (7, 8), (9, 8), (12, 8)];
const DEFAULT_SIGNATURE: usize = 2;

/// Taps further apart than this start a new tempo.
const TAP_RESET: Duration = Duration::from_secs(2);
/// How many taps the tempo is averaged over.
const TAPS_KEPT: usize = 5;

/// The metronome as saved in the config (it always starts stopped).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetronomeConfig {
    pub bpm: u16,
    /// Index into [`SIGNATURES`].
    pub signature: usize,
    /// Index into [`Subdivision::ALL`].
    pub subdivision: usize,
    /// The click's volume, 0..=1 (the knob's position).
    pub volume: f32,
}

impl Default for MetronomeConfig {
    fn default() -> Self {
        Self { bpm: 120, signature: DEFAULT_SIGNATURE, subdivision: 0, volume: 0.5 }
    }
}

impl MetronomeConfig {
    pub fn beats_per_bar(&self) -> u8 {
        SIGNATURES.get(self.signature).unwrap_or(&SIGNATURES[DEFAULT_SIGNATURE]).0
    }

    /// What the engine plays: the knob squared, for a gentler start, and
    /// silent while the master is muted.
    pub fn settings(&self, playing: bool, master_muted: bool) -> MetronomeSettings {
        MetronomeSettings {
            playing,
            bpm: f32::from(self.bpm),
            beats_per_bar: self.beats_per_bar(),
            subdivision: Subdivision::ALL.get(self.subdivision).copied().unwrap_or_default(),
            level: if master_muted { 0.0 } else { self.volume * self.volume },
        }
    }
}

pub fn signature_labels() -> Vec<String> {
    SIGNATURES.iter().map(|(beats, unit)| format!("{beats}/{unit}")).collect()
}

pub fn subdivision_labels() -> Vec<String> {
    Subdivision::ALL.iter().map(|subdivision| subdivision.label().to_string()).collect()
}

/// Recent taps on Tap.
#[derive(Default)]
pub struct TapTempo {
    taps: Vec<Instant>,
}

impl TapTempo {
    /// Records a tap at `now`; the tempo of the taps so far, once there are
    /// two close enough together.
    pub fn tap(&mut self, now: Instant) -> Option<u16> {
        if self.taps.last().is_some_and(|last| now.duration_since(*last) > TAP_RESET) {
            self.taps.clear();
        }
        self.taps.push(now);
        if self.taps.len() > TAPS_KEPT {
            self.taps.remove(0);
        }
        let (first, last) = (self.taps.first()?, self.taps.last()?);
        let intervals = self.taps.len() as u32 - 1;
        if intervals == 0 {
            return None;
        }
        let beat = last.duration_since(*first).as_secs_f32() / intervals as f32;
        Some((60.0 / beat).clamp(MIN_BPM, MAX_BPM).round() as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_tempo_averages_and_restarts_after_a_pause() {
        let start = Instant::now();
        let mut tap = TapTempo::default();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert_eq!(tap.tap(at(0)), None);
        assert_eq!(tap.tap(at(500)), Some(120));
        assert_eq!(tap.tap(at(1100)), Some(109)); // 1.1 s over two beats
        // A long pause starts over.
        assert_eq!(tap.tap(at(5000)), None);
        assert_eq!(tap.tap(at(5250)), Some(240));
    }
}
