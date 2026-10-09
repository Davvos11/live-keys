use std::path::PathBuf;

use anyhow::{Context, Result};
use live_keys_engine::MidiFilter;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub audio_device: Option<String>,
    pub midi_port: Option<String>,
    #[serde(default)]
    pub extra_plugin_dirs: Vec<PathBuf>,
    /// Live-tab slots, indexed by slot. May be shorter than the slot count in a file
    /// saved by an older version; callers pad it.
    #[serde(default)]
    pub slots: Vec<SlotConfig>,
}

/// One Live-tab slot's saved setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotConfig {
    /// `PluginInfo::id` of the plugin last loaded into this slot. Recorded only,
    /// not auto-loaded.
    #[serde(alias = "plugin_uri")]
    pub plugin_id: Option<String>,
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
            midi_channel: filter.channel,
            note_low: filter.note_low,
            note_high: filter.note_high,
            velocity_low: filter.velocity_low,
            velocity_high: filter.velocity_high,
            volume_db: 0.0,
        }
    }
}

impl SlotConfig {
    pub fn filter(&self) -> MidiFilter {
        MidiFilter {
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
    config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save(config: &AppConfig) -> Result<()> {
    let path = config_path().context("could not determine the OS config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}
