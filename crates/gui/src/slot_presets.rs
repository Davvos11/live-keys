//! Slot presets: a slot's whole chain — which plugins, in which order, and each
//! one's settings — saved under a name and recalled in one go.
//!
//! Each preset is a folder `<root>/<instrument>/<name>/` holding `slot.json` (the
//! chain) and one state file per plugin in its format's own standard form (an LV2
//! state bundle, a `.vstpreset`), written by `Engine::save_state_file`. These
//! aren't the plugins' own presets, so they don't show up in the plugins' preset
//! lists. The instrument folder is only for people browsing the files; grouping
//! goes by the instrument id in `slot.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_engine::{Engine, LoadedPluginMeta, PluginKey, PresetInfo, Section};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "slot.json";
/// The folder of presets of slots without an instrument.
pub const NO_INSTRUMENT: &str = "No instrument";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotPreset {
    pub name: String,
    pub instrument: Option<PluginState>,
    #[serde(default)]
    pub midi_effects: Vec<PluginState>,
    #[serde(default)]
    pub audio_effects: Vec<PluginState>,
}

impl SlotPreset {
    pub fn instrument_id(&self) -> Option<&str> {
        self.instrument.as_ref().map(|plugin| plugin.id.as_str())
    }

    pub fn instrument_name(&self) -> &str {
        self.instrument.as_ref().map_or(NO_INSTRUMENT, |plugin| plugin.name.as_str())
    }

    pub fn effects(&self, section: Section) -> &[PluginState] {
        match section {
            Section::Midi => &self.midi_effects,
            Section::Audio => &self.audio_effects,
        }
    }
}

/// One plugin of a slot preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginState {
    /// `PluginInfo::id`.
    pub id: String,
    pub name: String,
    /// The plugin's state file, relative to the preset's folder.
    pub state: String,
    /// The plugin preset it was set to when saved, so its preset bar shows that
    /// again, and whether it had been changed since.
    #[serde(default)]
    pub preset: Option<PresetRef>,
    #[serde(default)]
    pub preset_changed: bool,
}

/// A [`PresetInfo`], as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetRef {
    pub id: String,
    pub name: String,
    pub user: bool,
}

impl From<&PresetInfo> for PresetRef {
    fn from(preset: &PresetInfo) -> Self {
        Self {
            id: preset.id.clone(),
            name: preset.name.clone(),
            user: preset.user,
        }
    }
}

impl From<&PresetRef> for PresetInfo {
    fn from(preset: &PresetRef) -> Self {
        Self {
            id: preset.id.clone(),
            name: preset.name.clone(),
            user: preset.user,
        }
    }
}

/// A slot preset on disk.
#[derive(Debug, Clone)]
pub struct Stored {
    pub dir: PathBuf,
    pub preset: SlotPreset,
}

/// Where slot presets live: `~/.local/share/live-keys/slot-presets` on Linux.
fn root() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("slot-presets"))
}

/// Every slot preset, sorted by name. Unreadable ones are skipped (and logged).
pub fn list() -> Vec<Stored> {
    let mut found = Vec::new();
    let Some(root) = root() else {
        return found;
    };
    let folders = fs::read_dir(&root).into_iter().flatten().flatten();
    for preset_dir in folders.flat_map(|folder| fs::read_dir(folder.path()).into_iter().flatten().flatten()) {
        let dir = preset_dir.path();
        let file = dir.join(FILE_NAME);
        if !file.is_file() {
            continue;
        }
        match read(&file) {
            Ok(preset) => found.push(Stored { dir, preset }),
            Err(err) => log::warn!("skipping slot preset {}: {err:#}", file.display()),
        }
    }
    found.sort_by_key(|stored| stored.preset.name.to_lowercase());
    found
}

fn read(file: &Path) -> Result<SlotPreset> {
    let text = fs::read_to_string(file)?;
    Ok(serde_json::from_str(&text)?)
}

/// A loaded plugin, as saving needs it.
struct Loaded {
    key: PluginKey,
    id: String,
    name: String,
}

/// Saves slot `slot`'s chain as slot preset `name`, replacing one of the same
/// name for the same instrument. `plugin_preset` gives each plugin's current
/// plugin preset and whether it was changed since.
pub fn save(
    engine: &mut Engine,
    slot: usize,
    name: &str,
    plugin_preset: impl Fn(PluginKey) -> (Option<PresetInfo>, bool),
) -> Result<Stored> {
    let chain = engine.slot(slot).context("no such slot")?;
    let loaded = |meta: &LoadedPluginMeta| Loaded {
        key: meta.key,
        id: meta.id.clone(),
        name: meta.name.clone(),
    };
    let instrument = chain.instrument.as_ref().map(loaded);
    let midi_effects: Vec<Loaded> = chain.effects(Section::Midi).iter().map(loaded).collect();
    let audio_effects: Vec<Loaded> = chain.effects(Section::Audio).iter().map(loaded).collect();
    anyhow::ensure!(
        instrument.is_some() || !midi_effects.is_empty() || !audio_effects.is_empty(),
        "the slot is empty"
    );

    let root = root().context("no data folder to save slot presets in")?;
    let instrument_name = instrument.as_ref().map_or(NO_INSTRUMENT, |plugin| plugin.name.as_str());
    let folder = root.join(file_name(instrument_name));
    let dir = folder.join(file_name(name));
    // Written next to the old one and swapped in at the end, so a failed save
    // doesn't lose the preset it was replacing.
    let temp = folder.join(format!(".{}.saving", file_name(name)));
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    fs::create_dir_all(&temp)?;

    let result = (|| {
        let mut save_plugin = |plugin: &Loaded, stem: &str| -> Result<PluginState> {
            let path = engine
                .save_state_file(plugin.key, &temp, stem)
                .with_context(|| format!("saving {}'s settings failed", plugin.name))?;
            let (preset, preset_changed) = plugin_preset(plugin.key);
            Ok(PluginState {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                state: path.strip_prefix(&temp)?.display().to_string(),
                preset: preset.as_ref().map(PresetRef::from),
                preset_changed,
            })
        };
        let instrument = instrument.as_ref().map(|plugin| save_plugin(plugin, "instrument")).transpose()?;
        let mut effects = |plugins: &[Loaded], prefix: &str| -> Result<Vec<PluginState>> {
            plugins
                .iter()
                .enumerate()
                .map(|(index, plugin)| save_plugin(plugin, &format!("{prefix}-{}", index + 1)))
                .collect()
        };
        let preset = SlotPreset {
            name: name.to_string(),
            instrument,
            midi_effects: effects(&midi_effects, "midi")?,
            audio_effects: effects(&audio_effects, "audio")?,
        };
        fs::write(temp.join(FILE_NAME), serde_json::to_string_pretty(&preset)?)?;
        anyhow::Ok(preset)
    })();
    let preset = match result {
        Ok(preset) => preset,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(err);
        }
    };
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::rename(&temp, &dir)?;
    Ok(Stored { dir, preset })
}

/// Deletes the slot preset in `dir`, and its instrument folder if that's left
/// empty.
pub fn delete(dir: &Path) -> Result<()> {
    let root = root().context("no data folder")?;
    anyhow::ensure!(
        dir.starts_with(&root) && dir.join(FILE_NAME).is_file(),
        "{} isn't a slot preset",
        dir.display()
    );
    fs::remove_dir_all(dir)?;
    if let Some(folder) = dir.parent() {
        // Fails (harmlessly) unless empty.
        let _ = fs::remove_dir(folder);
    }
    Ok(())
}

/// `name` as a folder name: path separators and other troublesome characters
/// replaced.
fn file_name(name: &str) -> String {
    let name: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' })
        .collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() { "_".to_string() } else { name.to_string() }
}
