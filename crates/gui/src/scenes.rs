//! Scenes: several slots' setups — each one's whole chain with every plugin's
//! settings, its MIDI routing and its volume — saved under a name and recalled
//! in one go. Slots a scene doesn't include are left alone when it's recalled.
//!
//! Each scene is a folder `<root>/<name>/` holding `scene.json` and, per
//! included slot, a subfolder `slot-<n>/` that is a complete slot preset (see
//! `slot_presets`) — a snapshot, independent of the slot presets in the library.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_engine::{Engine, PluginKey, PresetInfo};
use serde::{Deserialize, Serialize};

use crate::config::SlotRouting;
use crate::slot_presets;

const FILE_NAME: &str = "scene.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    /// The included slots, by slot index.
    pub slots: Vec<SceneSlot>,
}

/// One slot of a scene.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneSlot {
    pub slot: usize,
    pub routing: SlotRouting,
    /// The slot's chain, as a slot preset in this subfolder of the scene.
    pub chain: String,
    /// The library slot preset the slot showed when saved, so it shows again.
    pub slot_preset: Option<SlotPresetRef>,
}

/// A library slot preset, as a slot's current one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotPresetRef {
    pub dir: PathBuf,
    pub name: String,
    pub changed: bool,
}

/// What saving needs to know about one slot to include.
pub struct SlotToSave {
    pub slot: usize,
    pub routing: SlotRouting,
    pub slot_preset: Option<SlotPresetRef>,
}

/// A scene on disk.
#[derive(Debug, Clone)]
pub struct Stored {
    pub dir: PathBuf,
    pub scene: Scene,
}

impl Stored {
    /// Slot `slot`'s part of the scene: its routing, its library slot preset,
    /// and its chain as a stored slot preset.
    pub fn slot(&self, slot: usize) -> Option<(&SceneSlot, Result<slot_presets::Stored>)> {
        let included = self.scene.slots.iter().find(|included| included.slot == slot)?;
        Some((included, slot_presets::read_dir(&self.dir.join(&included.chain))))
    }
}

/// Where scenes live: `~/.local/share/live-keys/scenes` on Linux.
fn root() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("scenes"))
}

/// Every scene, sorted by name. Unreadable ones are skipped (and logged).
pub fn list() -> Vec<Stored> {
    let mut found = Vec::new();
    let Some(root) = root() else {
        return found;
    };
    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
        let dir = entry.path();
        let file = dir.join(FILE_NAME);
        if !file.is_file() {
            continue;
        }
        let read = fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<Scene>(&text)?));
        match read {
            Ok(scene) => found.push(Stored { dir, scene }),
            Err(err) => log::warn!("skipping scene {}: {err:#}", file.display()),
        }
    }
    found.sort_by_key(|stored| stored.scene.name.to_lowercase());
    found
}

/// Saves `slots` as scene `name`, replacing one of the same name.
/// `plugin_preset` is as for [`slot_presets::save`].
pub fn save(
    engine: &mut Engine,
    name: &str,
    slots: &[SlotToSave],
    plugin_preset: impl Fn(PluginKey) -> (Option<PresetInfo>, bool),
) -> Result<Stored> {
    anyhow::ensure!(!slots.is_empty(), "no slots chosen");
    let root = root().context("no data folder to save scenes in")?;
    fs::create_dir_all(&root)?;
    let dir = root.join(slot_presets::file_name(name));
    let scene = slot_presets::replace_dir(&dir, |temp| {
        let mut scene = Scene {
            name: name.to_string(),
            slots: Vec::new(),
        };
        for to_save in slots {
            let chain = format!("slot-{}", to_save.slot + 1);
            let chain_dir = temp.join(&chain);
            fs::create_dir(&chain_dir)?;
            slot_presets::write(engine, to_save.slot, name, &chain_dir, &plugin_preset)
                .with_context(|| format!("slot {}", to_save.slot + 1))?;
            scene.slots.push(SceneSlot {
                slot: to_save.slot,
                routing: to_save.routing,
                chain,
                slot_preset: to_save.slot_preset.clone(),
            });
        }
        fs::write(temp.join(FILE_NAME), serde_json::to_string_pretty(&scene)?)?;
        Ok(scene)
    })?;
    Ok(Stored { dir, scene })
}

/// Deletes the scene in `dir`.
pub fn delete(dir: &Path) -> Result<()> {
    let root = root().context("no data folder")?;
    anyhow::ensure!(
        dir.starts_with(&root) && dir.join(FILE_NAME).is_file(),
        "{} isn't a scene",
        dir.display()
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}
