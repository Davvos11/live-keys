//! LV2 presets (`pset:Preset`): listing, applying, saving and deleting them,
//! through lilv's own state code so presets stay interchangeable with other hosts
//! (Ardour, Carla, jalv). lilv's state functions take raw world/plugin/instance
//! pointers that `livi`/`lilv` don't hand out, so presets get a lilv world of
//! their own ([`PresetWorld`]), loaded from the same `LV2_PATH`.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, Result};
use lilv_sys as lib;
use live_keys_core::PresetInfo;
use lv2_raw::{LV2Feature, LV2UridMap};

const PSET_PRESET: &str = "http://lv2plug.in/ns/ext/presets#Preset";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const CONTROL_PORT: &str = "http://lv2plug.in/ns/lv2core#ControlPort";
const INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const URID_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
const URID_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
pub(crate) const STATE_INTERFACE: &CStr = c"http://lv2plug.in/ns/ext/state#interface";
const ATOM_FLOAT: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";
const ATOM_DOUBLE: &CStr = c"http://lv2plug.in/ns/ext/atom#Double";
const ATOM_INT: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
const ATOM_LONG: &CStr = c"http://lv2plug.in/ns/ext/atom#Long";
const ATOM_BOOL: &CStr = c"http://lv2plug.in/ns/ext/atom#Bool";
// `LV2_State_Flags`: the state holds no pointers and can move between machines.
const LV2_STATE_IS_POD: u32 = 1;
const LV2_STATE_IS_PORTABLE: u32 = 2;

/// What lilv's state functions need of a running plugin instance: its descriptor
/// and handle. Copied out of an `Lv2Plugin` at load time, since the plugin itself
/// lives on the audio thread.
#[derive(Clone, Copy)]
pub struct InstanceRef {
    pub(crate) descriptor: *const lv2_raw::core::LV2Descriptor,
    pub(crate) handle: lv2_raw::core::LV2Handle,
}

impl InstanceRef {
    /// Whether the plugin keeps state beyond its control ports (the LV2 state
    /// interface: sample paths, a synth's patch). Saving or restoring that state
    /// must not run concurrently with the plugin's `run`, unlike plain port
    /// values. Asks the plugin itself (`extension_data` may be called from any
    /// thread), since its metadata doesn't always say.
    pub fn has_state(&self) -> bool {
        unsafe {
            let descriptor = &*self.descriptor;
            !(descriptor.extension_data)(STATE_INTERFACE.as_ptr().cast()).is_null()
        }
    }

    /// A stand-in `LilvInstance` for lilv's state functions, which only read
    /// these two fields of it (through its inline accessors).
    fn lilv_instance(&self) -> lib::LilvInstanceImpl {
        lib::LilvInstanceImpl {
            lv2_descriptor: self.descriptor,
            lv2_handle: self.handle,
            pimpl: ptr::null_mut(),
        }
    }
}

/// An owned lilv node.
struct Node(*mut lib::LilvNode);

impl Drop for Node {
    fn drop(&mut self) {
        unsafe { lib::lilv_node_free(self.0) };
    }
}

/// An owned lilv state.
struct State(*mut lib::LilvState);

impl Drop for State {
    fn drop(&mut self) {
        unsafe { lib::lilv_state_free(self.0) };
    }
}

/// The URID map/unmap and the feature list to hand lilv, taken from the features
/// every instance was created with, so state keys map to the same URIDs.
pub(crate) struct StateFeatures {
    map: *mut LV2UridMap,
    unmap: *mut c_void,
    /// Null-terminated.
    list: Vec<*const LV2Feature>,
}

impl StateFeatures {
    pub fn new(features: &livi::Features) -> Self {
        // `iter_features` wants an instance's worker feature, which only exists
        // at instantiation; a placeholder is filtered back out.
        let placeholder = LV2Feature {
            uri: c"urn:live-keys:none".as_ptr(),
            data: ptr::null_mut(),
        };
        let list: Vec<*const LV2Feature> = features
            .iter_features(&placeholder)
            .filter(|feature| !ptr::eq(*feature, &placeholder))
            .map(|feature| feature as *const LV2Feature)
            .chain(std::iter::once(ptr::null()))
            .collect();
        let data = |uri: &CStr| {
            list.iter()
                .take_while(|feature| !feature.is_null())
                .find(|&&feature| unsafe { CStr::from_ptr((*feature).uri) } == uri)
                .map_or(ptr::null_mut(), |&feature| unsafe { (*feature).data })
        };
        Self {
            map: data(URID_MAP).cast(),
            unmap: data(URID_UNMAP),
            list,
        }
    }

    fn urid(&self, uri: &CStr) -> u32 {
        unsafe { ((*self.map).map)((*self.map).handle, uri.as_ptr()) }
    }
}

/// A lilv world used only for presets.
pub(crate) struct PresetWorld {
    world: *mut lib::LilvWorld,
}

impl Drop for PresetWorld {
    fn drop(&mut self) {
        unsafe { lib::lilv_world_free(self.world) };
    }
}

impl PresetWorld {
    pub fn new() -> Self {
        unsafe {
            let world = lib::lilv_world_new();
            lib::lilv_world_load_all(world);
            Self { world }
        }
    }

    fn uri(&self, uri: &str) -> Node {
        let uri = CString::new(uri).unwrap_or_default();
        Node(unsafe { lib::lilv_new_uri(self.world, uri.as_ptr()) })
    }

    fn plugin(&self, plugin_uri: &str) -> Result<*const lib::LilvPlugin> {
        let uri = self.uri(plugin_uri);
        let plugin = unsafe {
            lib::lilv_plugins_get_by_uri(lib::lilv_world_get_all_plugins(self.world), uri.0)
        };
        anyhow::ensure!(!plugin.is_null(), "LV2 plugin not found: {plugin_uri}");
        Ok(plugin)
    }

    /// The plugin's control-input ports, by symbol.
    fn control_inputs(&self, plugin: *const lib::LilvPlugin) -> HashMap<CString, u32> {
        let (control, input) = (self.uri(CONTROL_PORT), self.uri(INPUT_PORT));
        let count = unsafe { lib::lilv_plugin_get_num_ports(plugin) };
        (0..count)
            .filter_map(|index| unsafe {
                let port = lib::lilv_plugin_get_port_by_index(plugin, index);
                if port.is_null()
                    || !lib::lilv_port_is_a(plugin, port, control.0)
                    || !lib::lilv_port_is_a(plugin, port, input.0)
                {
                    return None;
                }
                let symbol = lib::lilv_node_as_string(lib::lilv_port_get_symbol(plugin, port));
                Some((CStr::from_ptr(symbol).to_owned(), index))
            })
            .collect()
    }

    /// Every preset for the plugin at `plugin_uri`, sorted by name.
    pub fn presets(&self, plugin_uri: &str) -> Vec<PresetInfo> {
        let Ok(plugin) = self.plugin(plugin_uri) else {
            return Vec::new();
        };
        let (preset_class, label) = (self.uri(PSET_PRESET), self.uri(RDFS_LABEL));
        let user_dir = user_preset_root();
        let mut presets = Vec::new();
        unsafe {
            let related = lib::lilv_plugin_get_related(plugin, preset_class.0);
            if related.is_null() {
                return presets;
            }
            let mut iter = lib::lilv_nodes_begin(related);
            while !lib::lilv_nodes_is_end(related, iter) {
                let preset = lib::lilv_nodes_get(related, iter);
                iter = lib::lilv_nodes_next(related, iter);
                // A preset's label usually lives in its own file, not the
                // manifest that links it to the plugin.
                lib::lilv_world_load_resource(self.world, preset);
                let uri = node_string(preset);
                let name = {
                    let node = Node(lib::lilv_world_get(self.world, preset, label.0, ptr::null()));
                    if node.0.is_null() { String::new() } else { node_string(node.0) }
                };
                let path = file_path(preset);
                presets.push(PresetInfo {
                    name: if name.is_empty() { uri.rsplit('/').next().unwrap_or(&uri).to_string() } else { name },
                    user: path
                        .as_deref()
                        .zip(user_dir.as_deref())
                        .is_some_and(|(path, dir)| path.starts_with(dir)),
                    id: uri,
                });
            }
            lib::lilv_nodes_free(related);
        }
        presets.sort_by_key(|preset| preset.name.to_lowercase());
        presets
    }

    fn state(&self, features: &StateFeatures, preset_uri: &str) -> Result<State> {
        let uri = self.uri(preset_uri);
        unsafe {
            lib::lilv_world_load_resource(self.world, uri.0);
            let state = lib::lilv_state_new_from_world(self.world, features.map, uri.0);
            anyhow::ensure!(!state.is_null(), "couldn't load preset {preset_uri}");
            Ok(State(state))
        }
    }

    /// Restores preset `preset_uri` onto `instance` and returns its control port
    /// values, for the caller to apply — lilv hands them over rather than setting
    /// ports itself.
    ///
    /// # Safety
    /// If the plugin has state (see [`InstanceRef::has_state`]), it mustn't be
    /// running (`run`) on another thread meanwhile.
    pub unsafe fn restore(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        preset_uri: &str,
    ) -> Result<Vec<(u32, f32)>> {
        let state = self.state(features, preset_uri)?;
        unsafe { self.restore_state(features, instance, plugin_uri, &state) }
    }

    /// Like [`PresetWorld::restore`], from a state file written by
    /// [`PresetWorld::save_file`] (which the world doesn't know about).
    ///
    /// # Safety
    /// As for [`PresetWorld::restore`].
    pub unsafe fn restore_file(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        path: &Path,
    ) -> Result<Vec<(u32, f32)>> {
        let path_c = CString::new(path.display().to_string())?;
        let state = unsafe {
            lib::lilv_state_new_from_file(self.world, features.map, ptr::null(), path_c.as_ptr())
        };
        anyhow::ensure!(!state.is_null(), "couldn't read plugin state {}", path.display());
        unsafe { self.restore_state(features, instance, plugin_uri, &State(state)) }
    }

    unsafe fn restore_state(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        state: &State,
    ) -> Result<Vec<(u32, f32)>> {
        let plugin = self.plugin(plugin_uri)?;
        let mut collect = PortValues {
            ports: self.control_inputs(plugin),
            types: [ATOM_FLOAT, ATOM_DOUBLE, ATOM_INT, ATOM_LONG, ATOM_BOOL].map(|uri| features.urid(uri)),
            values: Vec::new(),
        };
        let mut lilv_instance = instance.lilv_instance();
        unsafe {
            lib::lilv_state_restore(
                state.0,
                &mut lilv_instance,
                Some(collect_port_value),
                (&mut collect as *mut PortValues).cast(),
                0,
                features.list.as_ptr(),
            );
        }
        Ok(collect.values)
    }

    /// Saves the plugin's current settings as preset `name`, in a bundle of its
    /// own under `~/.lv2` (named like Ardour names its presets), replacing a
    /// preset of the same plugin and name. `value` gives each control port's
    /// current value by index.
    ///
    /// # Safety
    /// As for [`PresetWorld::restore`].
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn save(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        plugin_name: &str,
        name: &str,
        value: &dyn Fn(u32) -> f32,
    ) -> Result<PresetInfo> {
        let root = user_preset_root().context("no home folder to save presets in")?;
        let bundle = root.join(format!("{}_{}.lv2", file_name(plugin_name), file_name(name)));
        let file = format!("{}.ttl", file_name(name));
        unsafe { self.save_state(features, instance, plugin_uri, &bundle, &file, name, value) }?;

        // Make the new (or rewritten) bundle part of the world.
        let dir = CString::new(format!("{}/", bundle.display()))?;
        unsafe {
            let bundle_uri = Node(lib::lilv_new_file_uri(self.world, ptr::null(), dir.as_ptr()));
            lib::lilv_world_unload_bundle(self.world, bundle_uri.0);
            lib::lilv_world_load_bundle(self.world, bundle_uri.0);
        }
        let path = CString::new(bundle.join(&file).display().to_string())?;
        let uri = unsafe { Node(lib::lilv_new_file_uri(self.world, ptr::null(), path.as_ptr())) };
        Ok(PresetInfo {
            id: unsafe { node_string(uri.0) },
            name: name.to_string(),
            user: true,
        })
    }

    /// Saves the plugin's current settings as a state bundle at `bundle` (created
    /// if missing, its state file `state.ttl`) that isn't a preset: the world
    /// doesn't load it, so it doesn't show up among the plugin's presets. Returns
    /// the state file's path, for [`PresetWorld::restore_file`].
    ///
    /// # Safety
    /// As for [`PresetWorld::restore`].
    pub unsafe fn save_file(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        bundle: &Path,
        value: &dyn Fn(u32) -> f32,
    ) -> Result<PathBuf> {
        let file = "state.ttl";
        unsafe { self.save_state(features, instance, plugin_uri, bundle, file, "", value) }?;
        Ok(bundle.join(file))
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn save_state(
        &self,
        features: &StateFeatures,
        instance: InstanceRef,
        plugin_uri: &str,
        bundle: &Path,
        file: &str,
        label: &str,
        value: &dyn Fn(u32) -> f32,
    ) -> Result<()> {
        let plugin = self.plugin(plugin_uri)?;
        let ports: HashMap<CString, f32> = self
            .control_inputs(plugin)
            .into_iter()
            .map(|(symbol, index)| (symbol, value(index)))
            .collect();
        let mut source = SavedValues {
            values: ports,
            float: features.urid(ATOM_FLOAT),
        };

        // lilv wants directories with a trailing slash.
        let dir = CString::new(format!("{}/", bundle.display()))?;
        let file_c = CString::new(file)?;
        let label = CString::new(label)?;
        let mut lilv_instance = instance.lilv_instance();
        unsafe {
            let state = lib::lilv_state_new_from_instance(
                plugin,
                &mut lilv_instance,
                features.map,
                ptr::null(),
                dir.as_ptr(),
                dir.as_ptr(),
                dir.as_ptr(),
                Some(saved_port_value),
                (&mut source as *mut SavedValues).cast(),
                LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE,
                features.list.as_ptr(),
            );
            anyhow::ensure!(!state.is_null(), "the plugin's state couldn't be captured");
            let state = State(state);
            if !label.is_empty() {
                lib::lilv_state_set_label(state.0, label.as_ptr());
            }
            let result = lib::lilv_state_save(
                self.world,
                features.map,
                features.unmap,
                state.0,
                ptr::null(),
                dir.as_ptr(),
                file_c.as_ptr(),
            );
            anyhow::ensure!(result == 0, "saving plugin state to {} failed", bundle.display());
        }
        Ok(())
    }

    /// Deletes user preset `preset_uri`: its files, and its bundle if that's left
    /// empty.
    pub fn delete(&self, features: &StateFeatures, preset_uri: &str) -> Result<()> {
        let uri = self.uri(preset_uri);
        let path = unsafe { file_path(uri.0) }.context("not a preset file")?;
        let root = user_preset_root().context("no home folder")?;
        anyhow::ensure!(path.starts_with(&root), "only presets you saved can be deleted");
        let state = self.state(features, preset_uri)?;
        let result = unsafe { lib::lilv_state_delete(self.world, state.0) };
        anyhow::ensure!(result == 0, "deleting {} failed", path.display());
        Ok(())
    }
}

/// Where user presets go: `~/.lv2`, the standard per-user LV2 folder.
fn user_preset_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".lv2"))
}

/// `name` made safe as part of a file name.
fn file_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    if name.is_empty() { "preset".to_string() } else { name }
}

unsafe fn node_string(node: *const lib::LilvNode) -> String {
    unsafe {
        let text = lib::lilv_node_as_string(node);
        if text.is_null() { String::new() } else { CStr::from_ptr(text).to_string_lossy().into_owned() }
    }
}

/// The local file a `file://` URI node points to.
unsafe fn file_path(node: *const lib::LilvNode) -> Option<PathBuf> {
    unsafe {
        let uri = lib::lilv_node_as_uri(node);
        if uri.is_null() {
            return None;
        }
        let path = lib::lilv_file_uri_parse(uri, ptr::null_mut());
        if path.is_null() {
            return None;
        }
        let result = PathBuf::from(CStr::from_ptr(path).to_string_lossy().into_owned());
        lib::lilv_free(path.cast());
        Some(result)
    }
}

/// Collects the port values lilv reports while restoring a preset.
struct PortValues {
    ports: HashMap<CString, u32>,
    /// URIDs of atom Float, Double, Int, Long and Bool.
    types: [u32; 5],
    values: Vec<(u32, f32)>,
}

unsafe extern "C" fn collect_port_value(
    symbol: *const c_char,
    user_data: *mut c_void,
    value: *const c_void,
    _size: u32,
    value_type: u32,
) {
    unsafe {
        let collect = &mut *user_data.cast::<PortValues>();
        let Some(&index) = collect.ports.get(CStr::from_ptr(symbol)) else {
            return;
        };
        let [float, double, int, long, boolean] = collect.types;
        let value = match value_type {
            t if t == float => *value.cast::<f32>(),
            t if t == double => *value.cast::<f64>() as f32,
            t if t == int || t == boolean => *value.cast::<i32>() as f32,
            t if t == long => *value.cast::<i64>() as f32,
            _ => return,
        };
        collect.values.push((index, value));
    }
}

/// Port values handed to lilv while saving a preset.
struct SavedValues {
    values: HashMap<CString, f32>,
    float: u32,
}

unsafe extern "C" fn saved_port_value(
    symbol: *const c_char,
    user_data: *mut c_void,
    size: *mut u32,
    value_type: *mut u32,
) -> *const c_void {
    unsafe {
        let source = &*user_data.cast::<SavedValues>();
        match source.values.get(CStr::from_ptr(symbol)) {
            Some(value) => {
                *size = size_of::<f32>() as u32;
                *value_type = source.float;
                (value as *const f32).cast()
            }
            None => {
                *size = 0;
                *value_type = 0;
                ptr::null()
            }
        }
    }
}
