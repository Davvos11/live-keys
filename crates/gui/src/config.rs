use std::path::PathBuf;

use anyhow::{Context, Result};
use live_keys_engine::{MidiFilter, list_midi_ports, midi_device_name};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub audio_device: Option<String>,
    /// The MIDI inputs slots can listen to, in order; never empty after [`load`].
    #[serde(default)]
    pub midi_inputs: Vec<MidiInputConfig>,
    #[serde(default)]
    pub extra_plugin_dirs: Vec<PathBuf>,
    /// Live-tab slots, indexed by slot. Empty in a new config; callers pad it.
    #[serde(default)]
    pub slots: Vec<SlotConfig>,
    /// `PluginInfo::id`s pinned to the top of the plugin picker.
    #[serde(default)]
    pub pinned_plugins: Vec<String>,
    /// `PresetInfo::id`s of favourite presets, listed first in a parameter
    /// window's preset browser. Unique across plugins (a preset URI or file path).
    #[serde(default)]
    pub pinned_presets: Vec<String>,
    /// The master output's volume in dB (at or below [`MIN_VOLUME_DB`]:
    /// silent), and whether it's muted.
    #[serde(default)]
    pub master_volume_db: f32,
    #[serde(default)]
    pub master_muted: bool,
    /// Semitones added to every slot's notes, on top of the slot's own
    /// transposition.
    #[serde(default)]
    pub master_transpose: i8,
    #[serde(default)]
    pub metronome: crate::metronome::MetronomeConfig,
    #[serde(default)]
    pub surfaces: SurfacesConfig,
}

/// The control surfaces panel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SurfacesConfig {
    /// The MIDI devices shown, by `midi_device_name`, in order; `None` until the
    /// user picks: then every device plugged in that has a known surface.
    pub devices: Option<Vec<String>>,
    /// Collapsed to one row by the user.
    pub collapsed: bool,
}

impl AppConfig {
    /// The master output's linear gain.
    pub fn master_gain(&self) -> f32 {
        gain(self.master_volume_db, self.master_muted)
    }
}

/// Linear gain for `volume_db`, or silence while muted or at the bottom.
fn gain(volume_db: f32, muted: bool) -> f32 {
    if muted || volume_db <= MIN_VOLUME_DB { 0.0 } else { 10f32.powf(volume_db / 20.0) }
}

/// One MIDI input: the devices feeding it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiInputConfig {
    /// By `midi_device_name`. Devices not plugged in stay listed and are
    /// connected when they appear.
    pub devices: Vec<String>,
}

/// One Live-tab slot's saved setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotConfig {
    /// Which MIDI inputs (bit `n` = `AppConfig::midi_inputs[n]`) it listens to.
    pub midi_inputs: u32,
    /// 0-based MIDI channel, or `None` for omni.
    pub midi_channel: Option<u8>,
    pub note_low: u8,
    pub note_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    /// Output volume in dB; at or below [`MIN_VOLUME_DB`] the slot is muted.
    pub volume_db: f32,
    /// Silenced by its mute button, whatever its volume.
    pub muted: bool,
    /// Semitones added to the notes it plays.
    pub transpose: i8,
}

/// Bottom of the volume fader's range, treated as silence.
pub const MIN_VOLUME_DB: f32 = -60.0;
pub const MAX_VOLUME_DB: f32 = 6.0;
/// The transposition range, in semitones either way.
pub const MAX_TRANSPOSE: i32 = 48;

impl Default for SlotConfig {
    fn default() -> Self {
        let filter = MidiFilter::default();
        Self {
            midi_inputs: 1,
            midi_channel: filter.channel,
            note_low: filter.note_low,
            note_high: filter.note_high,
            velocity_low: filter.velocity_low,
            velocity_high: filter.velocity_high,
            volume_db: 0.0,
            muted: false,
            transpose: filter.transpose,
        }
    }
}

/// A slot's MIDI routing (transposition included), volume and mute: what a scene
/// recalls besides the chain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotRouting {
    pub midi_inputs: u32,
    pub midi_channel: Option<u8>,
    pub note_low: u8,
    pub note_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    pub volume_db: f32,
    pub muted: bool,
    pub transpose: i8,
}

impl SlotConfig {
    pub fn routing(&self) -> SlotRouting {
        SlotRouting {
            midi_inputs: self.midi_inputs,
            midi_channel: self.midi_channel,
            note_low: self.note_low,
            note_high: self.note_high,
            velocity_low: self.velocity_low,
            velocity_high: self.velocity_high,
            volume_db: self.volume_db,
            muted: self.muted,
            transpose: self.transpose,
        }
    }

    pub fn set_routing(&mut self, routing: SlotRouting) {
        self.midi_inputs = routing.midi_inputs;
        self.midi_channel = routing.midi_channel;
        self.note_low = routing.note_low;
        self.note_high = routing.note_high;
        self.velocity_low = routing.velocity_low;
        self.velocity_high = routing.velocity_high;
        self.volume_db = routing.volume_db;
        self.muted = routing.muted;
        self.transpose = routing.transpose;
    }

    /// The slot's MIDI filter, its transposition plus the master's.
    pub fn filter(&self, master_transpose: i8) -> MidiFilter {
        MidiFilter {
            inputs: self.midi_inputs,
            channel: self.midi_channel,
            note_low: self.note_low,
            note_high: self.note_high,
            velocity_low: self.velocity_low,
            velocity_high: self.velocity_high,
            transpose: self.transpose + master_transpose,
        }
    }

    /// Linear gain for [`SlotConfig::volume_db`], or silence while muted.
    pub fn gain(&self) -> f32 {
        gain(self.volume_db, self.muted)
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("live-keys").join("config.json"))
}

/// Loads the saved config, or a default (empty) one if none exists yet or it can't be
/// read/parsed.
pub fn load() -> AppConfig {
    let mut config: AppConfig = config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default();
    if config.midi_inputs.is_empty() {
        // First run: every device plugged in now, so a keyboard works right away.
        let devices = list_midi_ports()
            .unwrap_or_default()
            .iter()
            .filter(|port| !port.starts_with("Midi Through"))
            .map(|port| midi_device_name(port).to_string())
            .collect();
        config.midi_inputs.push(MidiInputConfig { devices });
    }
    config
}

pub fn save(config: &AppConfig) -> Result<()> {
    let path = config_path().context("could not determine the OS config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}
