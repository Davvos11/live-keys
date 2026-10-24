//! The session: the rack as it was when the app last saved it — every slot's
//! chain with each plugin's settings, which slot preset and scene were current,
//! and which plugin windows were open — restored on startup. (Routing, volumes,
//! devices and the rest live in the config, which is saved as they change.)
//!
//! A folder `<data dir>/live-keys/session/` with `session.json` and per slot a
//! subfolder `slot-<n>/` that is a complete slot preset (see `slot_presets`).
//! While a session is being restored, a marker file says so: if a plugin
//! crashes the app during the restore, the next start finds the marker and
//! skips restoring rather than crashing again.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use live_keys_engine::{ChainPosition, Engine, PluginKey, PresetInfo, SLOT_COUNT};
use serde::{Deserialize, Serialize};

use crate::scenes::SlotPresetRef;
use crate::slot_presets;

const FILE_NAME: &str = "session.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub slots: Vec<SessionSlot>,
    pub current_scene: Option<CurrentSceneRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSlot {
    pub slot: usize,
    /// The slot's chain, as a slot preset in this subfolder.
    pub chain: String,
    /// The library slot preset it showed.
    pub slot_preset: Option<SlotPresetRef>,
    /// Plugins whose own GUI was open.
    pub open_guis: Vec<Position>,
    /// Plugins whose parameter window was open.
    pub open_params: Vec<Position>,
}

/// The scene that was current.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentSceneRef {
    pub dir: PathBuf,
    pub changed: bool,
}

/// Where a plugin is in its slot's chain, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Instrument,
    Midi(usize),
    Audio(usize),
}

impl From<ChainPosition> for Position {
    fn from(position: ChainPosition) -> Self {
        match position {
            ChainPosition::Instrument => Position::Instrument,
            ChainPosition::MidiEffect(index) => Position::Midi(index),
            ChainPosition::Effect(index) => Position::Audio(index),
        }
    }
}

impl From<Position> for ChainPosition {
    fn from(position: Position) -> Self {
        match position {
            Position::Instrument => ChainPosition::Instrument,
            Position::Midi(index) => ChainPosition::MidiEffect(index),
            Position::Audio(index) => ChainPosition::Effect(index),
        }
    }
}

/// What saving needs to know about one slot besides its chain.
pub struct SlotToSave {
    pub slot_preset: Option<SlotPresetRef>,
    pub open_guis: Vec<Position>,
    pub open_params: Vec<Position>,
}

fn dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("session"))
}

fn marker() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("session.restoring"))
}

/// Saves every slot (`slots`, by slot index) and the current scene.
/// `plugin_preset` is as for [`slot_presets::save`].
pub fn save(
    engine: &mut Engine,
    slots: Vec<SlotToSave>,
    current_scene: Option<CurrentSceneRef>,
    plugin_preset: impl Fn(PluginKey) -> (Option<PresetInfo>, bool),
) -> Result<()> {
    let dir = dir().context("no data folder to save the session in")?;
    fs::create_dir_all(dir.parent().context("no parent folder")?)?;
    slot_presets::replace_dir(&dir, |temp| {
        let mut session = Session {
            slots: Vec::new(),
            current_scene,
        };
        for (slot, to_save) in slots.into_iter().enumerate().take(SLOT_COUNT) {
            let chain = format!("slot-{}", slot + 1);
            let chain_dir = temp.join(&chain);
            fs::create_dir(&chain_dir)?;
            slot_presets::write(engine, slot, "Session", &chain_dir, &plugin_preset)
                .with_context(|| format!("slot {}", slot + 1))?;
            session.slots.push(SessionSlot {
                slot,
                chain,
                slot_preset: to_save.slot_preset,
                open_guis: to_save.open_guis,
                open_params: to_save.open_params,
            });
        }
        fs::write(temp.join(FILE_NAME), serde_json::to_string_pretty(&session)?)?;
        Ok(())
    })
}

/// Each slot's chain, by slot index, as a stored slot preset.
pub type Chains = Vec<(usize, Result<slot_presets::Stored>)>;

/// The saved session, with each slot's chain read as a stored slot preset;
/// `None` if there's none.
pub fn load() -> Option<(Session, Chains)> {
    let dir = dir()?;
    let text = fs::read_to_string(dir.join(FILE_NAME)).ok()?;
    let session: Session = match serde_json::from_str(&text) {
        Ok(session) => session,
        Err(err) => {
            log::warn!("ignoring the saved session: {err:#}");
            return None;
        }
    };
    let chains = session
        .slots
        .iter()
        .map(|slot| (slot.slot, slot_presets::read_dir(&dir.join(&slot.chain))))
        .collect();
    Some((session, chains))
}

/// Marks a restore as started. Returns `false` if the last one never finished
/// (the app crashed or was killed during it), in which case it shouldn't be
/// tried again.
pub fn begin_restore() -> bool {
    let Some(marker) = marker() else {
        return true;
    };
    if marker.exists() {
        return false;
    }
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(err) = fs::write(&marker, "") {
        log::warn!("couldn't write {}: {err}", marker.display());
    }
    true
}

/// Marks the restore as finished (or given up on).
pub fn end_restore() {
    if let Some(marker) = marker() {
        let _ = fs::remove_file(marker);
    }
}
