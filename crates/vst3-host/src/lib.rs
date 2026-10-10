//! VST3 plugin hosting: scanning for `.vst3` bundles, and running a plugin split
//! into a real-time half ([`Vst3Plugin`], a [`HostedPlugin`]) and a main-thread
//! half ([`Vst3Controller`]).
//!
//! [`HostedPlugin`]: live_keys_core::HostedPlugin

mod com;
mod module;
mod plugin;
mod preset;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_core::{PluginFormat, PluginInfo, PluginKind, PresetInfo};
use vst3::Steinberg::TUID;

pub use com::EditCallback;
pub use module::default_search_paths;
pub use plugin::{Vst3Controller, Vst3Plugin};
pub use vst3;

/// Prefix of every VST3 [`PluginInfo::id`], followed by the class ID in hex.
pub const ID_PREFIX: &str = "vst3:";

struct ScannedClass {
    info: PluginInfo,
    binary: PathBuf,
    cid: TUID,
    vendor: String,
}

/// The VST3 plugins found by the last scan, and how to instantiate them.
pub struct Vst3Host {
    classes: Vec<ScannedClass>,
    max_block_size: usize,
}

impl Vst3Host {
    /// Scans [`default_search_paths`] plus `extra_dirs`. Loads every bundle's
    /// library to ask its factory what it contains — VST3 has no other way to
    /// find out — so a plugin that crashes on load takes the scan down with it.
    /// `max_block_size` must be at least as large as the largest number of frames
    /// that will ever be passed to `Vst3Plugin::process` in one call.
    pub fn new(max_block_size: usize, extra_dirs: &[PathBuf]) -> Self {
        let mut dirs = default_search_paths();
        dirs.extend_from_slice(extra_dirs);

        let mut classes = Vec::new();
        for bundle in module::find_bundles(&dirs) {
            let Some(binary) = module::bundle_binary(&bundle) else {
                log::warn!("VST3 bundle {} has no binary for this platform", bundle.display());
                continue;
            };
            let module = match module::load(&binary) {
                Ok(module) => module,
                Err(err) => {
                    log::warn!("skipping VST3 bundle {}: {err:#}", bundle.display());
                    continue;
                }
            };
            for class in module::audio_classes(&module) {
                let id = format!("{ID_PREFIX}{}", module::cid_to_hex(&class.cid));
                // The same plugin can be installed in more than one search folder;
                // the first found wins.
                if classes.iter().any(|scanned: &ScannedClass| scanned.info.id == id) {
                    continue;
                }
                classes.push(ScannedClass {
                    info: PluginInfo {
                        id,
                        name: class.name,
                        format: PluginFormat::Vst3,
                        kind: if class.is_instrument {
                            PluginKind::Instrument
                        } else {
                            // Effects can't be told apart by category; MIDI ones
                            // need a look at their buses.
                            plugin::probe_kind(&module, &class.cid)
                                .unwrap_or(PluginKind::AudioEffect)
                        },
                    },
                    binary: binary.clone(),
                    cid: class.cid,
                    vendor: class.vendor,
                });
            }
        }
        log::info!("found {} VST3 plugin classes", classes.len());
        Self {
            classes,
            max_block_size,
        }
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.classes.iter().map(|class| class.info.clone()).collect()
    }

    fn class(&self, id: &str) -> Result<&ScannedClass> {
        self.classes
            .iter()
            .find(|class| class.info.id == id)
            .with_context(|| format!("VST3 plugin not found: {id}"))
    }

    /// The `.vstpreset` files in the standard preset folders for the plugin with
    /// [`PluginInfo::id`] `id`, sorted by name.
    pub fn presets(&self, id: &str) -> Vec<PresetInfo> {
        match id.strip_prefix(ID_PREFIX) {
            Some(class_id) => preset::find(class_id),
            None => Vec::new(),
        }
    }

    /// Applies the preset file at `path` to the running plugin `controller` (of
    /// plugin `id`). Main thread only.
    pub fn load_preset(&self, controller: &Vst3Controller, id: &str, path: &str) -> Result<()> {
        let class_id = id.strip_prefix(ID_PREFIX).context("not a VST3 plugin id")?;
        let preset = preset::read(Path::new(path), class_id)?;
        controller.set_state(&preset.component, preset.controller.as_deref())
    }

    /// Saves the running plugin `controller`'s (of plugin `id`) current state as
    /// user preset `name`, in `~/.vst3/presets/<vendor>/<plugin>/`, replacing a
    /// preset of the same name. Main thread only.
    pub fn save_preset(&self, controller: &Vst3Controller, id: &str, name: &str) -> Result<PresetInfo> {
        let class = self.class(id)?;
        let class_id = id.strip_prefix(ID_PREFIX).context("not a VST3 plugin id")?;
        let dir = preset::user_dir(&class.vendor, &class.info.name).context("no home folder")?;
        let path = dir.join(format!("{}.vstpreset", preset::file_name(name)));
        let (component, controller_state) = controller.state()?;
        preset::write(&path, class_id, &component, &controller_state)?;
        Ok(PresetInfo {
            id: path.display().to_string(),
            name: name.to_string(),
            user: true,
        })
    }

    /// Saves the running plugin `controller`'s (of plugin `id`) current state as a
    /// `.vstpreset` file at `path`, which isn't one of its presets (e.g. part of
    /// a bigger preset of this app's); [`Vst3Host::load_preset`] reads it back.
    /// Main thread only.
    pub fn save_file(&self, controller: &Vst3Controller, id: &str, path: &Path) -> Result<()> {
        let class_id = id.strip_prefix(ID_PREFIX).context("not a VST3 plugin id")?;
        let (component, controller_state) = controller.state()?;
        preset::write(path, class_id, &component, &controller_state)
    }

    /// Deletes a user preset file (see [`PresetInfo::user`]).
    pub fn delete_preset(&self, path: &str) -> Result<()> {
        preset::delete(Path::new(path))
    }

    /// Instantiates the plugin with [`PluginInfo::id`] `id`. Must be called from the
    /// main thread (the controller half is bound to the thread that creates it).
    pub fn instantiate(&self, id: &str, sample_rate: f64) -> Result<(Vst3Plugin, Vst3Controller)> {
        let class = self.class(id)?;
        let module = module::load(&class.binary)?;
        plugin::instantiate(
            module,
            &class.cid,
            class.info.name.clone(),
            sample_rate,
            self.max_block_size,
        )
    }
}
