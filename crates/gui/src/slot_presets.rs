//! Slot presets: a slot's whole chain — which plugins, in which order, and each
//! one's settings — saved as a folder and recalled in one go. Scenes, the
//! session and a cleared slot's undo are made of them.
//!
//! Each is a folder holding `slot.json` (the chain) and one state file per
//! plugin in its format's own standard form (an LV2 state bundle, a
//! `.vstpreset`), written by `Engine::save_state_file`. These aren't the
//! plugins' own presets, so they don't show up in the plugins' preset lists.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_engine::{Engine, LoadedPluginMeta, PluginKey, PresetInfo, Section};
use serde::{Deserialize, Serialize};

use crate::mappings::{SavedValue, SceneMapping};

const FILE_NAME: &str = "slot.json";

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
    /// Its MIDI-learn mappings kept with the scene.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mappings: Vec<SceneMapping>,
    /// The bases of its targets that were modulated (MIDI learn), which the
    /// state file only has with the modulation on: restored after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<SavedValue>,
}

/// What saving a plugin into a slot preset needs besides its state: its
/// current plugin preset and whether it changed since, and its mappings.
pub struct PluginExtras {
    pub preset: Option<PresetInfo>,
    pub preset_changed: bool,
    pub mappings: Vec<SceneMapping>,
    pub values: Vec<SavedValue>,
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

fn read(file: &Path) -> Result<SlotPreset> {
    let text = fs::read_to_string(file)?;
    Ok(serde_json::from_str(&text)?)
}

/// Reads the slot preset in `dir`.
pub fn read_dir(dir: &Path) -> Result<Stored> {
    let preset = read(&dir.join(FILE_NAME))?;
    Ok(Stored {
        dir: dir.to_path_buf(),
        preset,
    })
}

/// A loaded plugin, as saving needs it.
struct Loaded {
    key: PluginKey,
    id: String,
    name: String,
}

/// Writes slot `slot`'s chain into the empty folder `dir`: `slot.json` and a
/// state file per plugin. An empty slot makes a preset without plugins.
pub fn write(
    engine: &mut Engine,
    slot: usize,
    name: &str,
    dir: &Path,
    plugin_extras: impl Fn(PluginKey) -> PluginExtras,
) -> Result<SlotPreset> {
    let chain = engine.slot(slot).context("no such slot")?;
    let loaded = |meta: &LoadedPluginMeta| Loaded {
        key: meta.key,
        id: meta.id.clone(),
        name: meta.name.clone(),
    };
    let instrument = chain.instrument.as_ref().map(loaded);
    let midi_effects: Vec<Loaded> = chain.effects(Section::Midi).iter().map(loaded).collect();
    let audio_effects: Vec<Loaded> = chain.effects(Section::Audio).iter().map(loaded).collect();

    let mut save_plugin = |plugin: &Loaded, stem: &str| -> Result<PluginState> {
        let path = engine
            .save_state_file(plugin.key, dir, stem)
            .with_context(|| format!("saving {}'s settings failed", plugin.name))?;
        let extras = plugin_extras(plugin.key);
        Ok(PluginState {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            state: path.strip_prefix(dir)?.display().to_string(),
            preset: extras.preset.as_ref().map(PresetRef::from),
            preset_changed: extras.preset_changed,
            mappings: extras.mappings,
            values: extras.values,
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
    fs::write(dir.join(FILE_NAME), serde_json::to_string_pretty(&preset)?)?;
    Ok(preset)
}

/// Fills folder `dir` anew with `fill`, which writes into the empty folder it's
/// given. That's a temporary folder next to `dir`, swapped in at the end, so a
/// failed save doesn't lose what it was replacing.
pub fn replace_dir<T>(dir: &Path, fill: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let parent = dir.parent().context("no parent folder")?;
    let name = dir.file_name().context("no folder name")?.to_string_lossy();
    let temp = parent.join(format!(".{name}.saving"));
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    fs::create_dir_all(&temp)?;
    let value = match fill(&temp) {
        Ok(value) => value,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(err);
        }
    };
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::rename(&temp, dir)?;
    Ok(value)
}

/// `name` as a folder name: path separators and other troublesome characters
/// replaced.
pub fn file_name(name: &str) -> String {
    let name: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' })
        .collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() { "_".to_string() } else { name.to_string() }
}
