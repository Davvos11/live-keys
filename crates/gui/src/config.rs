use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub audio_device: Option<String>,
    pub midi_port: Option<String>,
    #[serde(default)]
    pub extra_plugin_dirs: Vec<PathBuf>,
    pub last_plugin_uri: Option<String>,
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
