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
    /// `PluginInfo::id` of the instrument last loaded into this slot. Recorded
    /// only, not auto-loaded.
    pub plugin_id: Option<String>,
    /// `PluginInfo::id`s of the MIDI effects last in front of this slot's
    /// instrument, in order. Recorded only, not auto-loaded.
    pub midi_effect_ids: Vec<String>,
    /// `PluginInfo::id`s of the audio effects last after this slot's instrument,
    /// in order. Recorded only, not auto-loaded.
    pub effect_ids: Vec<String>,
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
}

/// Bottom of the volume fader's range, treated as silence.
pub const MIN_VOLUME_DB: f32 = -60.0;
pub const MAX_VOLUME_DB: f32 = 6.0;

impl Default for SlotConfig {
    fn default() -> Self {
        let filter = MidiFilter::default();
        Self {
            plugin_id: None,
            midi_effect_ids: Vec::new(),
            effect_ids: Vec::new(),
            midi_inputs: 1,
            midi_channel: filter.channel,
            note_low: filter.note_low,
            note_high: filter.note_high,
            velocity_low: filter.velocity_low,
            velocity_high: filter.velocity_high,
            volume_db: 0.0,
        }
    }
}

/// A slot's MIDI routing and volume: what a scene recalls besides the chain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotRouting {
    pub midi_inputs: u32,
    pub midi_channel: Option<u8>,
    pub note_low: u8,
    pub note_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    pub volume_db: f32,
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
    }

    pub fn filter(&self) -> MidiFilter {
        MidiFilter {
            inputs: self.midi_inputs,
            channel: self.midi_channel,
            note_low: self.note_low,
            note_high: self.note_high,
            velocity_low: self.velocity_low,
            velocity_high: self.velocity_high,
        }
    }

    /// Linear gain for [`SlotConfig::volume_db`].
    pub fn gain(&self) -> f32 {
        if self.volume_db <= MIN_VOLUME_DB {
            0.0
        } else {
            10f32.powf(self.volume_db / 20.0)
        }
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
