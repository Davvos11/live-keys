//! CLAP plugin hosting: scanning for `.clap` files, and running a plugin split
//! into a real-time half ([`ClapPlugin`], a [`HostedPlugin`]) and a main-thread
//! half ([`ClapController`]).
//!
//! [`HostedPlugin`]: live_keys_core::HostedPlugin

mod events;
mod host;
mod module;
mod plugin;
mod preset;
mod state;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_core::{PluginFormat, PluginInfo, PluginKind, PresetInfo};

pub use clap_sys;
pub use module::default_search_paths;
pub use plugin::{ClapController, ClapGui, ClapPlugin, destroy_pending};
pub use state::EXTENSION as STATE_FILE_EXTENSION;

/// Prefix of every CLAP [`PluginInfo::id`], followed by the plugin's CLAP id.
pub const ID_PREFIX: &str = "clap:";

struct ScannedPlugin {
    info: PluginInfo,
    binary: PathBuf,
}

impl ScannedPlugin {
    fn clap_id(&self) -> &str {
        &self.info.id[ID_PREFIX.len()..]
    }
}

/// What a plugin's CLAP features make it.
fn kind(features: &[String]) -> PluginKind {
    let has = |feature: &str| features.iter().any(|f| f == feature);
    if has("instrument") {
        PluginKind::Instrument
    } else if has("note-effect") && !has("audio-effect") {
        PluginKind::MidiEffect
    } else {
        PluginKind::AudioEffect
    }
}

/// A CLAP scan in progress, a file at a time so a GUI can keep drawing: loading
/// a plugin's library runs its code, which some plugins expect to happen on the
/// thread that will later run their GUIs, the main thread.
pub struct ClapScan {
    files: std::vec::IntoIter<PathBuf>,
    plugins: Vec<ScannedPlugin>,
}

impl ClapScan {
    /// Starts a scan of [`default_search_paths`] plus `extra_dirs`.
    pub fn new(extra_dirs: &[PathBuf]) -> Self {
        let mut dirs = default_search_paths();
        dirs.extend_from_slice(extra_dirs);
        Self { files: module::find_files(&dirs).into_iter(), plugins: Vec::new() }
    }

    /// Loads one file; `false` once there's nothing left to do.
    pub fn step(&mut self) -> bool {
        let Some(file) = self.files.next() else { return false };
        match module::load(&file) {
            Ok(module) => {
                for descriptor in module.descriptors() {
                    let id = format!("{ID_PREFIX}{}", descriptor.id);
                    // The same plugin can be installed in more than one folder;
                    // the first found wins.
                    if self.plugins.iter().any(|plugin| plugin.info.id == id) {
                        continue;
                    }
                    log::debug!("CLAP {} by {}: {:?}", descriptor.name, descriptor.vendor, descriptor.features);
                    self.plugins.push(ScannedPlugin {
                        info: PluginInfo {
                            id,
                            name: descriptor.name,
                            format: PluginFormat::Clap,
                            kind: kind(&descriptor.features),
                        },
                        binary: file.clone(),
                    });
                }
            }
            Err(err) => log::warn!("skipping CLAP plugin {}: {err:#}", file.display()),
        }
        true
    }

    /// Runs what's left of the scan. `max_block_size` must be at least as large as
    /// the largest number of frames that will ever be passed to
    /// `ClapPlugin::process` in one call.
    pub fn finish(mut self, max_block_size: usize) -> ClapHost {
        while self.step() {}
        log::info!("found {} CLAP plugins", self.plugins.len());
        ClapHost { plugins: self.plugins, max_block_size, discovered: RefCell::default() }
    }
}

/// The CLAP plugins found by the last scan, and how to instantiate them.
/// Main thread only.
pub struct ClapHost {
    plugins: Vec<ScannedPlugin>,
    max_block_size: usize,
    /// Presets found by preset discovery, per plugin id: it reads every preset
    /// file, so only once per plugin.
    discovered: RefCell<HashMap<String, Vec<PresetInfo>>>,
}

impl ClapHost {
    /// Scans [`default_search_paths`] plus `extra_dirs` in one go; see [`ClapScan`].
    pub fn new(max_block_size: usize, extra_dirs: &[PathBuf]) -> Self {
        ClapScan::new(extra_dirs).finish(max_block_size)
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.plugins.iter().map(|plugin| plugin.info.clone()).collect()
    }

    fn plugin(&self, id: &str) -> Result<&ScannedPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.info.id == id)
            .with_context(|| format!("CLAP plugin not found: {id}"))
    }

    /// Instantiates and activates the plugin with [`PluginInfo::id`] `id`. Main
    /// thread only.
    pub fn instantiate(&self, id: &str, sample_rate: f64) -> Result<(ClapPlugin, ClapController)> {
        let plugin = self.plugin(id)?;
        let module = module::load(&plugin.binary)?;
        plugin::instantiate(module, plugin.clap_id(), plugin.info.name.clone(), sample_rate, self.max_block_size)
    }

    /// The presets of plugin `id`: those its own preset discovery finds, and
    /// those saved here, sorted by name.
    pub fn presets(&self, id: &str) -> Vec<PresetInfo> {
        let Ok(plugin) = self.plugin(id) else { return Vec::new() };
        let mut presets = preset::user_presets(plugin.clap_id());
        let discovered = self
            .discovered
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| match module::load(&plugin.binary) {
                Ok(module) => preset::discover(&module, plugin.clap_id()),
                Err(_) => Vec::new(),
            })
            .clone();
        presets.extend(discovered);
        presets.sort_by_cached_key(|preset| preset.name.to_lowercase());
        presets
    }

    /// Applies preset `preset_id` (a [`PresetInfo::id`] from
    /// [`ClapHost::presets`]) to `controller`, a running plugin `id`.
    pub fn load_preset(&self, controller: &ClapController, id: &str, preset_id: &str) -> Result<()> {
        match preset::parse_id(preset_id).context("not a CLAP preset")? {
            preset::PresetId::State(path) => self.load_file(controller, id, path),
            preset::PresetId::Discovered(location) => controller.load_preset(&location),
        }
    }

    /// Saves `controller`'s (a running plugin `id`) current state as user preset
    /// `name`, replacing one of the same name.
    pub fn save_preset(&self, controller: &ClapController, id: &str, name: &str) -> Result<PresetInfo> {
        let plugin = self.plugin(id)?;
        let dir = preset::user_dir(plugin.clap_id()).context("no data folder")?;
        let path = dir.join(format!("{}.{}", preset::file_name(name), state::EXTENSION));
        self.save_file(controller, id, &path)?;
        Ok(PresetInfo { id: preset::state_id(&path), name: name.to_string(), user: true })
    }

    /// Saves `controller`'s (a running plugin `id`) current state as a state file
    /// at `path`; [`ClapHost::load_file`] reads it back.
    pub fn save_file(&self, controller: &ClapController, id: &str, path: &Path) -> Result<()> {
        let plugin = self.plugin(id)?;
        state::write_file(path, plugin.clap_id(), &controller.state()?)
    }

    /// Restores a state file written by [`ClapHost::save_file`].
    pub fn load_file(&self, controller: &ClapController, id: &str, path: &Path) -> Result<()> {
        let clap_id = id.strip_prefix(ID_PREFIX).context("not a CLAP plugin id")?;
        controller.set_state(&state::read_file(path, clap_id)?)
    }

    /// Deletes a user preset: a state file saved here, or a preset file a
    /// plugin's preset discovery marks as the user's.
    pub fn delete_preset(&self, id: &str, preset_id: &str) -> Result<()> {
        let path = match preset::parse_id(preset_id).context("not a CLAP preset")? {
            preset::PresetId::State(path) => path.to_path_buf(),
            preset::PresetId::Discovered(location) => {
                let user = self
                    .discovered
                    .borrow()
                    .get(id)
                    .is_some_and(|presets| presets.iter().any(|preset| preset.id == preset_id && preset.user));
                match (location.location, location.load_key, user) {
                    (Some(path), None, true) => PathBuf::from(path),
                    _ => anyhow::bail!("this preset can't be deleted"),
                }
            }
        };
        std::fs::remove_file(&path).with_context(|| format!("couldn't delete {}", path.display()))?;
        if let Some(presets) = self.discovered.borrow_mut().get_mut(id) {
            presets.retain(|preset| preset.id != preset_id);
        }
        Ok(())
    }
}
