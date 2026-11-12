//! A plugin's presets: the ones its preset-discovery providers find (factory
//! presets, and user presets it saved itself), plus state files saved here as
//! user presets (CLAP has no shared format for those; see `state.rs`).

use std::ffi::{CString, c_char};
use std::path::{Path, PathBuf};

use clap_sys::factory::preset_discovery::*;
use clap_sys::timestamp::clap_timestamp;
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;
use live_keys_core::PresetInfo;

use crate::module::{Module, c_string};
use crate::plugin::PresetLocation;

/// Most preset files looked at per location, against runaway folders.
const MAX_FILES: usize = 50_000;
const MAX_DEPTH: usize = 12;

/// Prefix of a [`PresetInfo::id`] for a state file saved by this host.
const STATE_PREFIX: &str = "state\t";
/// Prefix of one for a discovered preset: then its location kind, location and
/// load key, separated by tabs (empty for none).
const DISCOVERED_PREFIX: &str = "clap-preset\t";

/// A preset id, decoded.
pub(crate) enum PresetId<'a> {
    State(&'a Path),
    Discovered(PresetLocation<'a>),
}

pub(crate) fn parse_id<'a>(id: &'a str) -> Option<PresetId<'a>> {
    if let Some(path) = id.strip_prefix(STATE_PREFIX) {
        return Some(PresetId::State(Path::new(path)));
    }
    let mut parts = id.strip_prefix(DISCOVERED_PREFIX)?.splitn(3, '\t');
    let kind = parts.next()?.parse().ok()?;
    let non_empty = |s: &'a str| (!s.is_empty()).then_some(s);
    let location = non_empty(parts.next()?);
    let load_key = non_empty(parts.next()?);
    Some(PresetId::Discovered(PresetLocation { kind, location, load_key }))
}

pub(crate) fn state_id(path: &Path) -> String {
    format!("{STATE_PREFIX}{}", path.display())
}

fn discovered_id(kind: u32, location: Option<&str>, load_key: Option<&str>) -> String {
    format!("{DISCOVERED_PREFIX}{kind}\t{}\t{}", location.unwrap_or(""), load_key.unwrap_or(""))
}

/// Where this host saves user presets of plugin `plugin_id`.
pub(crate) fn user_dir(plugin_id: &str) -> Option<PathBuf> {
    let folder: String = plugin_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    Some(dirs::data_dir()?.join("live-keys").join("clap-presets").join(folder))
}

/// A preset name as a file name: path separators and control characters replaced.
pub(crate) fn file_name(name: &str) -> String {
    let name: String = name.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') { format!("_{name}") } else { name.to_string() }
}

/// The state files in [`user_dir`].
pub(crate) fn user_presets(plugin_id: &str) -> Vec<PresetInfo> {
    let Some(dir) = user_dir(plugin_id) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != crate::state::EXTENSION {
                return None;
            }
            Some(PresetInfo {
                name: path.file_stem()?.to_string_lossy().into_owned(),
                id: state_id(&path),
                user: true,
            })
        })
        .collect()
}

/// What a provider's `init` declared.
#[derive(Default)]
struct Declared {
    /// File extensions of its preset files, without the dot; `None` = any file.
    extensions: Vec<Option<String>>,
    locations: Vec<(u32, u32, Option<String>)>,
}

unsafe extern "C" fn declare_filetype(
    indexer: *const clap_preset_discovery_indexer,
    filetype: *const clap_preset_discovery_filetype,
) -> bool {
    let declared = unsafe { &mut *((*indexer).indexer_data as *mut Declared) };
    let Some(filetype) = (unsafe { filetype.as_ref() }) else { return false };
    let extension = unsafe { c_string(filetype.file_extension) }
        .map(|ext| ext.trim_start_matches('.').to_lowercase())
        .filter(|ext| !ext.is_empty());
    declared.extensions.push(extension);
    true
}

unsafe extern "C" fn declare_location(
    indexer: *const clap_preset_discovery_indexer,
    location: *const clap_preset_discovery_location,
) -> bool {
    let declared = unsafe { &mut *((*indexer).indexer_data as *mut Declared) };
    let Some(location) = (unsafe { location.as_ref() }) else { return false };
    declared
        .locations
        .push((location.flags, location.kind, unsafe { c_string(location.location) }));
    true
}

unsafe extern "C" fn declare_soundpack(
    _indexer: *const clap_preset_discovery_indexer,
    _soundpack: *const clap_preset_discovery_soundpack,
) -> bool {
    true
}

unsafe extern "C" fn indexer_extension(
    _indexer: *const clap_preset_discovery_indexer,
    _id: *const c_char,
) -> *const std::ffi::c_void {
    std::ptr::null()
}

/// One preset being described to the receiver.
struct Pending {
    name: Option<String>,
    load_key: Option<String>,
    plugin_ids: Vec<String>,
    flags: u32,
}

/// Collects the presets of one location (or file) that are for `plugin_id`.
struct Receiver<'a> {
    plugin_id: &'a str,
    kind: u32,
    location: Option<String>,
    location_flags: u32,
    current: Option<Pending>,
    found: Vec<PresetInfo>,
}

impl Receiver<'_> {
    fn finish(&mut self) {
        let Some(pending) = self.current.take() else { return };
        if !pending.plugin_ids.iter().any(|id| id == self.plugin_id) {
            return;
        }
        let name = pending.name.filter(|name| !name.is_empty()).or_else(|| {
            let location = self.location.as_deref()?;
            Some(Path::new(location).file_stem()?.to_string_lossy().into_owned())
        });
        self.found.push(PresetInfo {
            id: discovered_id(self.kind, self.location.as_deref(), pending.load_key.as_deref()),
            name: name.unwrap_or_else(|| "Untitled".to_string()),
            user: pending.flags & CLAP_PRESET_DISCOVERY_IS_USER_CONTENT != 0,
        });
    }
}

unsafe fn receiver<'a>(r: *const clap_preset_discovery_metadata_receiver) -> &'a mut Receiver<'a> {
    unsafe { &mut *((*r).receiver_data as *mut Receiver) }
}

unsafe extern "C" fn on_error(r: *const clap_preset_discovery_metadata_receiver, _os_error: i32, message: *const c_char) {
    let receiver = unsafe { receiver(r) };
    log::debug!(
        "preset discovery for {} failed at {:?}: {}",
        receiver.plugin_id,
        receiver.location,
        unsafe { c_string(message) }.unwrap_or_default()
    );
}

unsafe extern "C" fn begin_preset(
    r: *const clap_preset_discovery_metadata_receiver,
    name: *const c_char,
    load_key: *const c_char,
) -> bool {
    let receiver = unsafe { receiver(r) };
    receiver.finish();
    receiver.current = Some(Pending {
        name: unsafe { c_string(name) },
        load_key: unsafe { c_string(load_key) }.filter(|key| !key.is_empty()),
        plugin_ids: Vec::new(),
        flags: receiver.location_flags,
    });
    true
}

unsafe extern "C" fn add_plugin_id(r: *const clap_preset_discovery_metadata_receiver, id: *const clap_universal_plugin_id) {
    let receiver = unsafe { receiver(r) };
    let (Some(current), Some(id)) = (receiver.current.as_mut(), unsafe { id.as_ref() }) else {
        return;
    };
    if unsafe { c_string(id.abi) }.as_deref() == Some("clap")
        && let Some(id) = unsafe { c_string(id.id) }
    {
        current.plugin_ids.push(id);
    }
}

unsafe extern "C" fn set_flags(r: *const clap_preset_discovery_metadata_receiver, flags: u32) {
    if let Some(current) = unsafe { receiver(r) }.current.as_mut() {
        current.flags = flags;
    }
}

unsafe extern "C" fn ignore_str(_r: *const clap_preset_discovery_metadata_receiver, _value: *const c_char) {}
unsafe extern "C" fn ignore_timestamps(
    _r: *const clap_preset_discovery_metadata_receiver,
    _created: clap_timestamp,
    _modified: clap_timestamp,
) {
}
unsafe extern "C" fn ignore_extra_info(
    _r: *const clap_preset_discovery_metadata_receiver,
    _key: *const c_char,
    _value: *const c_char,
) {
}

/// Every preset file below `path` (or `path` itself) with one of `extensions`.
fn preset_files(path: &Path, extensions: &[Option<String>]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, extensions: &[Option<String>], found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if found.len() >= MAX_FILES {
                return;
            }
            let path = entry.path();
            if path.is_dir() {
                if depth < MAX_DEPTH {
                    walk(&path, depth + 1, extensions, found);
                }
            } else if matches(&path, extensions) {
                found.push(path);
            }
        }
    }
    fn matches(path: &Path, extensions: &[Option<String>]) -> bool {
        let extension = path.extension().map(|ext| ext.to_string_lossy().to_lowercase());
        extensions.is_empty()
            || extensions.iter().any(|wanted| wanted.is_none() || *wanted == extension)
    }
    let mut found = Vec::new();
    if path.is_file() {
        found.push(path.to_path_buf());
    } else {
        walk(path, 0, extensions, &mut found);
    }
    found
}

/// The presets `module`'s preset-discovery providers know for plugin
/// `plugin_id` (its CLAP id). Main thread only; can take a while (providers
/// read every preset file).
pub(crate) fn discover(module: &Module, plugin_id: &str) -> Vec<PresetInfo> {
    let Some(factory) = module.factory(&[CLAP_PRESET_DISCOVERY_FACTORY_ID, CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT])
    else {
        return Vec::new();
    };
    let factory = unsafe { &*(factory as *const clap_preset_discovery_factory) };
    let (Some(count), Some(get_descriptor), Some(create)) = (factory.count, factory.get_descriptor, factory.create)
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for index in 0..unsafe { count(factory) } {
        let Some(descriptor) = (unsafe { get_descriptor(factory, index).as_ref() }) else { continue };
        let mut declared = Declared::default();
        let indexer = clap_preset_discovery_indexer {
            clap_version: CLAP_VERSION,
            name: c"Live Keys".as_ptr(),
            vendor: c"Live Keys".as_ptr(),
            url: c"".as_ptr(),
            version: c"0.1.0".as_ptr(),
            indexer_data: &mut declared as *mut Declared as *mut std::ffi::c_void,
            declare_filetype: Some(declare_filetype),
            declare_location: Some(declare_location),
            declare_soundpack: Some(declare_soundpack),
            get_extension: Some(indexer_extension),
        };
        let provider = unsafe { create(factory, &indexer, descriptor.id) };
        let Some(provider_ref) = (unsafe { provider.as_ref() }) else { continue };
        if provider_ref.init.is_some_and(|init| unsafe { init(provider) })
            && let Some(get_metadata) = provider_ref.get_metadata
        {
            for (flags, kind, location) in std::mem::take(&mut declared.locations) {
                let targets: Vec<Option<String>> = match (kind, &location) {
                    (CLAP_PRESET_DISCOVERY_LOCATION_FILE, Some(path)) => {
                        preset_files(Path::new(path), &declared.extensions)
                            .into_iter()
                            .map(|file| Some(file.display().to_string()))
                            .collect()
                    }
                    (CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN, _) => vec![None],
                    _ => Vec::new(),
                };
                for target in targets {
                    let mut collector = Receiver {
                        plugin_id,
                        kind,
                        location: target,
                        location_flags: flags,
                        current: None,
                        found: Vec::new(),
                    };
                    let target_c = collector.location.as_deref().and_then(|s| CString::new(s).ok());
                    let metadata_receiver = clap_preset_discovery_metadata_receiver {
                        receiver_data: &mut collector as *mut Receiver as *mut std::ffi::c_void,
                        on_error: Some(on_error),
                        begin_preset: Some(begin_preset),
                        add_plugin_id: Some(add_plugin_id),
                        set_soundpack_id: Some(ignore_str),
                        set_flags: Some(set_flags),
                        add_creator: Some(ignore_str),
                        set_description: Some(ignore_str),
                        set_timestamps: Some(ignore_timestamps),
                        add_feature: Some(ignore_str),
                        add_extra_info: Some(ignore_extra_info),
                    };
                    let target_ptr = target_c.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
                    unsafe { get_metadata(provider, kind, target_ptr, &metadata_receiver) };
                    collector.finish();
                    found.append(&mut collector.found);
                }
            }
        }
        if let Some(destroy) = provider_ref.destroy {
            unsafe { destroy(provider) };
        }
    }
    found
}
