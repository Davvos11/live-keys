//! live-keys' own instruments, as plugins of the built-in format
//! (`PluginFormat::Builtin`): the front-end that wraps an instrument core (an
//! SFZ instrument with its controls, e.g. `live-keys-piano`) in a
//! [`HostedPlugin`], with presets.
//!
//! - **Ids**: [`ID_PREFIX`] + the core's id (`builtin:piano`).
//! - **Parameters**: the core's controls, in order: id = index, symbol = the
//!   control's symbol, values in the control's own range.
//! - **Presets**: the core's shipped presets, plus user presets as files in the
//!   core's state format (`live_keys_sfz::controls`) under
//!   `<data dir>/live-keys/presets/<core id>/<name>.lkpreset`. State files
//!   (slot presets, the session) are the same format.
//! - **Samples**: each core loads its own; an instrument is loaded once and
//!   shared by every instance while any is alive.

use anyhow::{Context, Result, bail};
use live_keys_core::{HostedPlugin, MidiEvent, ParamChoice, ParamInfo, ParamKind, PluginFormat, PluginInfo, PluginKind, PresetInfo};
use live_keys_sfz::{Control, Instrument, Synth, controls};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

pub const ID_PREFIX: &str = "builtin:";
/// The extension of a state file ([`BuiltinHost::save_file`]).
pub const STATE_FILE_EXTENSION: &str = "lkstate";
const PRESET_EXTENSION: &str = "lkpreset";
/// Preset ids: a shipped preset's name, or a user preset's file.
const SHIPPED: &str = "shipped\t";
const FILE: &str = "file\t";

/// One of our instruments, as the host sees it.
struct Definition {
    /// The core's id; the plugin id is [`ID_PREFIX`] + this.
    id: &'static str,
    name: &'static str,
    controls: &'static [Control],
    presets: &'static [(&'static str, &'static [(&'static str, f32)])],
    voices: usize,
    load: fn() -> Result<Instrument>,
}

const DEFINITIONS: &[Definition] = &[Definition {
    id: live_keys_piano::ID,
    name: live_keys_piano::NAME,
    controls: &live_keys_piano::CONTROLS,
    presets: live_keys_piano::PRESETS,
    voices: live_keys_piano::VOICES,
    load: load_piano,
}];

fn load_piano() -> Result<Instrument> {
    let dir = live_keys_piano::default_sample_dir().context("no data folder to find the samples in")?;
    live_keys_piano::load(&dir)
}

fn definition(plugin_id: &str) -> Result<&'static Definition> {
    let id = plugin_id.strip_prefix(ID_PREFIX).with_context(|| format!("not a built-in plugin: {plugin_id}"))?;
    DEFINITIONS.iter().find(|d| d.id == id).with_context(|| format!("no built-in plugin {plugin_id}"))
}

/// The built-in instruments: listing, loading, presets.
#[derive(Default)]
pub struct BuiltinHost {
    /// Loaded instruments by core id, shared while any instance holds them.
    loaded: Mutex<HashMap<&'static str, Weak<Instrument>>>,
}

impl BuiltinHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        DEFINITIONS
            .iter()
            .map(|d| PluginInfo {
                id: format!("{ID_PREFIX}{}", d.id),
                name: d.name.to_string(),
                format: PluginFormat::Builtin,
                kind: PluginKind::Instrument,
            })
            .collect()
    }

    /// The plugin's parameters: its controls.
    pub fn params(&self, plugin_id: &str) -> Result<Vec<ParamInfo>> {
        Ok(definition(plugin_id)?.controls.iter().enumerate().map(|(i, c)| param_info(i, c)).collect())
    }

    /// A new instance at `sample_rate`, rendering up to `max_block_size` frames a
    /// call, its controls at their defaults. Loads the instrument (samples
    /// included) unless another instance already has.
    pub fn instantiate(&self, plugin_id: &str, sample_rate: f64, max_block_size: usize) -> Result<BuiltinPlugin> {
        let definition = definition(plugin_id)?;
        let instrument = {
            let mut loaded = self.loaded.lock().unwrap();
            match loaded.get(definition.id).and_then(Weak::upgrade) {
                Some(instrument) => instrument,
                None => {
                    let instrument = Arc::new((definition.load)()?);
                    loaded.insert(definition.id, Arc::downgrade(&instrument));
                    instrument
                }
            }
        };
        let mut synth = Synth::new(instrument, sample_rate as f32, definition.voices);
        for control in definition.controls {
            control.apply(&mut synth, control.default);
        }
        Ok(BuiltinPlugin {
            name: definition.name.to_string(),
            synth,
            controls: definition.controls,
            left: vec![0.0; max_block_size],
            right: vec![0.0; max_block_size],
            frames: 0,
        })
    }

    /// Shipped presets first, then the user's (by name).
    pub fn presets(&self, plugin_id: &str) -> Vec<PresetInfo> {
        let Ok(definition) = definition(plugin_id) else { return Vec::new() };
        let mut presets: Vec<PresetInfo> = definition
            .presets
            .iter()
            .map(|(name, _)| PresetInfo { id: format!("{SHIPPED}{name}"), name: name.to_string(), user: false })
            .collect();
        let mut user: Vec<PresetInfo> = user_preset_dir(definition)
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()? == PRESET_EXTENSION).then_some(())?;
                let name = path.file_stem()?.to_string_lossy().into_owned();
                Some(PresetInfo { id: format!("{FILE}{}", path.display()), name, user: true })
            })
            .collect();
        user.sort_by_key(|p| p.name.to_lowercase());
        presets.extend(user);
        presets
    }

    /// A preset's value for every parameter (defaults where it says nothing).
    pub fn preset_values(&self, plugin_id: &str, preset_id: &str) -> Result<Vec<(u32, f32)>> {
        let definition = definition(plugin_id)?;
        if let Some(name) = preset_id.strip_prefix(SHIPPED) {
            let (_, values) = definition.presets.iter().find(|(n, _)| *n == name).context("no such preset")?;
            let values = definition
                .controls
                .iter()
                .map(|c| values.iter().find(|(s, _)| *s == c.symbol).map_or(c.default, |&(_, v)| v));
            return Ok(values.enumerate().map(|(i, v)| (i as u32, v)).collect());
        }
        let path = preset_id.strip_prefix(FILE).context("unknown preset")?;
        self.load_file(plugin_id, Path::new(path))
    }

    /// Saves user preset `name` from `value` (per parameter id), replacing one of
    /// the same name.
    pub fn save_preset(&self, plugin_id: &str, name: &str, value: &dyn Fn(u32) -> f32) -> Result<PresetInfo> {
        let definition = definition(plugin_id)?;
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("not a usable preset name: {name:?}");
        }
        let dir = user_preset_dir(definition).context("no data folder for presets")?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{name}.{PRESET_EXTENSION}"));
        self.save_file(plugin_id, &path, value)?;
        Ok(PresetInfo { id: format!("{FILE}{}", path.display()), name: name.to_string(), user: true })
    }

    /// Deletes a user preset (shipped ones can't be).
    pub fn delete_preset(&self, plugin_id: &str, preset_id: &str) -> Result<()> {
        let definition = definition(plugin_id)?;
        let path = PathBuf::from(preset_id.strip_prefix(FILE).context("only user presets can be deleted")?);
        // Only files in the plugin's own preset folder.
        if user_preset_dir(definition).is_none_or(|dir| path.parent() != Some(dir.as_path())) {
            bail!("not one of this plugin's user presets");
        }
        std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))
    }

    /// Writes a state file of `value` (per parameter id).
    pub fn save_file(&self, plugin_id: &str, path: &Path, value: &dyn Fn(u32) -> f32) -> Result<()> {
        let definition = definition(plugin_id)?;
        let text = controls::write_state(definition.controls, |i| value(i as u32));
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Reads a state file: every parameter's value.
    pub fn load_file(&self, plugin_id: &str, path: &Path) -> Result<Vec<(u32, f32)>> {
        let definition = definition(plugin_id)?;
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let values = controls::read_state(definition.controls, &text).with_context(|| format!("in {}", path.display()))?;
        Ok(values.into_iter().enumerate().map(|(i, v)| (i as u32, v)).collect())
    }
}

fn user_preset_dir(definition: &Definition) -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("live-keys").join("presets").join(definition.id))
}

fn param_info(index: usize, control: &Control) -> ParamInfo {
    let choice = !control.choices.is_empty();
    ParamInfo {
        id: index as u32,
        symbol: control.symbol.to_string(),
        name: control.name.to_string(),
        default_value: control.default,
        min_value: control.min,
        max_value: control.max,
        kind: if choice { ParamKind::Choice } else { ParamKind::Continuous },
        steps: 0,
        logarithmic: false,
        unit: control.unit.to_string(),
        choices: control
            .choices
            .iter()
            .enumerate()
            .map(|(i, label)| ParamChoice { value: i as f32, label: label.to_string() })
            .collect(),
    }
}

/// A running instance: the core's synth, its controls as parameters.
pub struct BuiltinPlugin {
    name: String,
    synth: Synth,
    controls: &'static [Control],
    left: Vec<f32>,
    right: Vec<f32>,
    /// Frames the last `process` rendered.
    frames: usize,
}

impl HostedPlugin for BuiltinPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn audio_output_count(&self) -> usize {
        2
    }

    fn audio_input_count(&self) -> usize {
        0
    }

    fn audio_input_mut(&mut self, _channel: usize) -> &mut [f32] {
        &mut []
    }

    fn process(&mut self, midi_in: &[MidiEvent], frames: usize) {
        let frames = frames.min(self.left.len());
        // Render up to each event's frame, then apply it.
        let mut done = 0;
        for event in midi_in {
            let at = (event.frame_offset as usize).min(frames);
            if at > done {
                self.synth.render(&mut self.left[done..at], &mut self.right[done..at]);
                done = at;
            }
            self.synth.midi(event.as_slice());
        }
        if done < frames {
            self.synth.render(&mut self.left[done..frames], &mut self.right[done..frames]);
        }
        self.frames = frames;
    }

    fn audio_output(&self, channel: usize) -> &[f32] {
        let buffer = if channel == 0 { &self.left } else { &self.right };
        &buffer[..self.frames]
    }

    fn set_param(&mut self, id: u32, value: f32) {
        if let Some(control) = self.controls.get(id as usize) {
            control.apply(&mut self.synth, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_preset_values_cover_every_parameter() {
        let host = BuiltinHost::new();
        let id = format!("{ID_PREFIX}{}", live_keys_piano::ID);
        let presets = host.presets(&id);
        assert!(presets.len() >= live_keys_piano::PRESETS.len());
        let params = host.params(&id).unwrap();
        let values = host.preset_values(&id, &presets[1].id).unwrap();
        assert_eq!(values.len(), params.len());
        let brightness = params.iter().position(|p| p.symbol == "brightness").unwrap();
        assert_eq!(values[brightness].1, -6.0);
        // Unspecified ones are at their defaults.
        let tune = params.iter().position(|p| p.symbol == "tune").unwrap();
        assert_eq!(values[tune].1, params[tune].default_value);
    }

    #[test]
    fn state_files_round_trip() {
        let host = BuiltinHost::new();
        let id = format!("{ID_PREFIX}{}", live_keys_piano::ID);
        let path = std::env::temp_dir().join(format!("live-keys-builtin-{}.{STATE_FILE_EXTENSION}", std::process::id()));
        host.save_file(&id, &path, &|i| if i == 8 { 1.0 } else { i as f32 }).unwrap();
        let values = host.load_file(&id, &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(values[1], (1, 1.0));
        assert_eq!(values[8], (8, 1.0));
    }
}
