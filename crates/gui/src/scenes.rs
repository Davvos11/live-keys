//! Scenes: what happens to each slot when a scene is recalled — restored to a
//! saved setup (its whole chain with every plugin's settings, its MIDI routing
//! and its volume), cleared, or left unchanged — under a name.
//!
//! Each scene is a folder `<root>/<name>/` holding `scene.json` and, per slot
//! saved as restored, a subfolder `slot-<n>/` that is a complete slot preset
//! (see `slot_presets`) — a snapshot, independent of the slot presets in the
//! library. A slot keeps its saved setup when it's later set to clear or leave
//! unchanged, so it can be set back to restore; one saved without a setup can't.
//! The list's order (the user's, by dragging) is `<root>/order.json`: folder
//! names, first to last; scenes missing from it come after, by name.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_engine::{Engine, PluginKey, PresetInfo};
use serde::{Deserialize, Serialize};

use crate::config::SlotRouting;
use crate::slot_presets;

const FILE_NAME: &str = "scene.json";
const ORDER_FILE: &str = "order.json";

/// What recalling a scene does to a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotMode {
    #[default]
    Leave,
    Restore,
    Clear,
}

impl SlotMode {
    /// As the UI numbers them: 0 leave unchanged, 1 restore, 2 clear.
    pub fn index(self) -> i32 {
        match self {
            SlotMode::Leave => 0,
            SlotMode::Restore => 1,
            SlotMode::Clear => 2,
        }
    }

    pub fn from_index(index: i32) -> Self {
        match index {
            1 => SlotMode::Restore,
            2 => SlotMode::Clear,
            _ => SlotMode::Leave,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    /// By slot index; slots not listed are left unchanged.
    pub slots: Vec<SceneSlot>,
}

impl Scene {
    pub fn slot(&self, slot: usize) -> Option<&SceneSlot> {
        self.slots.iter().find(|scene_slot| scene_slot.slot == slot)
    }

    pub fn mode(&self, slot: usize) -> SlotMode {
        self.slot(slot).map_or(SlotMode::Leave, |scene_slot| scene_slot.mode)
    }

    /// Whether slot `slot` has a saved setup to restore.
    pub fn can_restore(&self, slot: usize) -> bool {
        self.slot(slot).is_some_and(|scene_slot| scene_slot.saved.is_some())
    }

    /// The slots recalling it changes (restores or clears).
    pub fn affected_slots(&self) -> Vec<usize> {
        self.slots
            .iter()
            .filter(|scene_slot| scene_slot.mode != SlotMode::Leave)
            .map(|scene_slot| scene_slot.slot)
            .collect()
    }
}

/// One slot of a scene.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneSlot {
    pub slot: usize,
    pub mode: SlotMode,
    /// Its setup, if it was saved as restored.
    pub saved: Option<SavedSlot>,
}

/// A slot's setup in a scene.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSlot {
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

/// What saving needs to know about one slot.
pub struct SlotToSave {
    pub slot: usize,
    pub mode: SlotMode,
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
    /// Slot `slot`'s saved setup, with its chain read as a stored slot preset.
    pub fn saved(&self, slot: usize) -> Option<(&SavedSlot, Result<slot_presets::Stored>)> {
        let saved = self.scene.slot(slot)?.saved.as_ref()?;
        Some((saved, slot_presets::read_dir(&self.dir.join(&saved.chain))))
    }
}

/// Where scenes live: `~/.local/share/live-keys/scenes` on Linux.
fn root() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("scenes"))
}

fn folder_name(dir: &Path) -> String {
    dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_order(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join(ORDER_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_order(root: &Path, order: &[String]) -> Result<()> {
    fs::write(root.join(ORDER_FILE), serde_json::to_string_pretty(order)?)?;
    Ok(())
}

/// Rewrites the order with `change` applied to it.
fn change_order(change: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    let root = root().context("no data folder")?;
    let mut order = read_order(&root);
    change(&mut order);
    write_order(&root, &order)
}

/// Moves the scene in `dir` to position `index` of the list (as [`list`]
/// returns it).
pub fn move_to(dir: &Path, index: usize) -> Result<()> {
    let mut order: Vec<String> = list().iter().map(|stored| folder_name(&stored.dir)).collect();
    let name = folder_name(dir);
    let from = order.iter().position(|other| *other == name).context("no such scene")?;
    let moved = order.remove(from);
    order.insert(index.min(order.len()), moved);
    change_order(|stored_order| *stored_order = order)
}

/// Every scene, in the user's order. Unreadable ones are skipped (and logged).
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
    let order = read_order(&root);
    found.sort_by_key(|stored| {
        let position = order.iter().position(|name| *name == folder_name(&stored.dir));
        (position.unwrap_or(usize::MAX), stored.scene.name.to_lowercase())
    });
    found
}

/// Saves `slots` as scene `name`, replacing one of the same name; slots set to
/// restore have their setup saved. Other slots keep the setup they have in
/// `previous` (the scene being overwritten, if any), so they can still be set
/// back to restore. `plugin_preset` is as for [`slot_presets::save`].
pub fn save(
    engine: &mut Engine,
    name: &str,
    slots: &[SlotToSave],
    previous: Option<&Stored>,
    plugin_preset: impl Fn(PluginKey) -> (Option<PresetInfo>, bool),
) -> Result<Stored> {
    anyhow::ensure!(
        slots.iter().any(|to_save| to_save.mode != SlotMode::Leave),
        "every slot is set to leave unchanged"
    );
    let root = root().context("no data folder to save scenes in")?;
    fs::create_dir_all(&root)?;
    let dir = root.join(slot_presets::file_name(name));
    let scene = slot_presets::replace_dir(&dir, |temp| {
        let mut scene = Scene {
            name: name.to_string(),
            slots: Vec::new(),
        };
        for to_save in slots {
            let saved = if to_save.mode == SlotMode::Restore {
                let chain = format!("slot-{}", to_save.slot + 1);
                let chain_dir = temp.join(&chain);
                fs::create_dir(&chain_dir)?;
                slot_presets::write(engine, to_save.slot, name, &chain_dir, &plugin_preset)
                    .with_context(|| format!("slot {}", to_save.slot + 1))?;
                Some(SavedSlot {
                    routing: to_save.routing,
                    chain,
                    slot_preset: to_save.slot_preset.clone(),
                })
            } else if let Some(previous) = previous
                && let Some(saved) = previous.scene.slot(to_save.slot).and_then(|slot| slot.saved.clone())
            {
                copy_dir(&previous.dir.join(&saved.chain), &temp.join(&saved.chain))?;
                Some(saved)
            } else {
                None
            };
            scene.slots.push(SceneSlot {
                slot: to_save.slot,
                mode: to_save.mode,
                saved,
            });
        }
        write(temp, &scene)?;
        Ok(scene)
    })?;
    let name = folder_name(&dir);
    change_order(|order| {
        if !order.contains(&name) {
            order.push(name);
        }
    })?;
    Ok(Stored { dir, scene })
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Renames `stored` to `name` and sets its slots' modes (by slot index; a slot
/// without a saved setup can't be set to restore). Refuses a name another scene
/// already has.
pub fn update(stored: &Stored, name: &str, modes: &[SlotMode]) -> Result<Stored> {
    let mut scene = stored.scene.clone();
    scene.name = name.to_string();
    for (slot, &mode) in modes.iter().enumerate() {
        anyhow::ensure!(
            mode != SlotMode::Restore || scene.can_restore(slot),
            "slot {} has nothing saved to restore",
            slot + 1
        );
        match scene.slots.iter_mut().find(|scene_slot| scene_slot.slot == slot) {
            Some(scene_slot) => scene_slot.mode = mode,
            None => scene.slots.push(SceneSlot {
                slot,
                mode,
                saved: None,
            }),
        }
    }
    anyhow::ensure!(
        !scene.affected_slots().is_empty(),
        "every slot is set to leave unchanged"
    );
    let dir = stored.dir.with_file_name(slot_presets::file_name(name));
    if dir != stored.dir {
        anyhow::ensure!(!dir.exists(), "there's already a scene called \"{name}\"");
        fs::rename(&stored.dir, &dir)?;
        let (old_name, new_name) = (folder_name(&stored.dir), folder_name(&dir));
        change_order(|order| {
            for name in order.iter_mut().filter(|name| **name == old_name) {
                name.clone_from(&new_name);
            }
        })?;
    }
    write(&dir, &scene)?;
    Ok(Stored { dir, scene })
}

fn write(dir: &Path, scene: &Scene) -> Result<()> {
    fs::write(dir.join(FILE_NAME), serde_json::to_string_pretty(scene)?)?;
    Ok(())
}

/// Copies `stored` as a new scene named "<name> copy" (or "copy 2", …, if
/// that's taken), placed right after it in the list.
pub fn duplicate(stored: &Stored) -> Result<Stored> {
    let root = root().context("no data folder")?;
    let (name, dir) = (1..)
        .map(|n| match n {
            1 => format!("{} copy", stored.scene.name),
            n => format!("{} copy {n}", stored.scene.name),
        })
        .map(|name| {
            let dir = root.join(slot_presets::file_name(&name));
            (name, dir)
        })
        .find(|(_, dir)| !dir.exists())
        .context("no free name")?;
    let mut scene = stored.scene.clone();
    scene.name = name;
    slot_presets::replace_dir(&dir, |temp| {
        copy_dir(&stored.dir, temp)?;
        write(temp, &scene)
    })?;
    let mut order: Vec<String> = list()
        .iter()
        .map(|other| folder_name(&other.dir))
        .filter(|other| *other != folder_name(&dir))
        .collect();
    let after = order.iter().position(|other| *other == folder_name(&stored.dir));
    order.insert(after.map_or(order.len(), |index| index + 1), folder_name(&dir));
    change_order(|stored_order| *stored_order = order)?;
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
    let name = folder_name(dir);
    change_order(|order| order.retain(|other| *other != name))
}
