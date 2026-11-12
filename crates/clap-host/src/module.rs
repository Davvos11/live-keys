//! Finding `.clap` files, loading them, and reading their plugin factories.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::clap_version_is_compatible;

/// How deep [`find_files`] looks below a search folder (vendors nest them).
const MAX_SEARCH_DEPTH: usize = 4;

/// The standard CLAP folders on Linux: `CLAP_PATH`'s entries first, then
/// `~/.clap` and `/usr/lib/clap` (the spec's), and `/usr/local/lib/clap`.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("CLAP_PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".clap"));
    }
    paths.push(PathBuf::from("/usr/lib/clap"));
    paths.push(PathBuf::from("/usr/local/lib/clap"));
    paths
}

/// Every `.clap` file in `dirs` (and below), in search order, each once.
pub(crate) fn find_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = entries.filter_map(|entry| Some(entry.ok()?.path())).collect();
        entries.sort();
        for path in entries {
            // On Linux a plugin is a plain shared library named `.clap`.
            if path.is_file() && path.extension().is_some_and(|ext| ext == "clap") {
                let path = path.canonicalize().unwrap_or(path);
                if !found.contains(&path) {
                    found.push(path);
                }
            } else if path.is_dir() && depth < MAX_SEARCH_DEPTH {
                walk(&path, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    for dir in dirs {
        walk(dir, 0, &mut found);
    }
    found
}

/// A loaded `.clap` library whose entry point has been initialized.
///
/// Never unloaded, and its entry is never `deinit`ed: frameworks like JUCE keep
/// process-wide state that doesn't survive being unloaded and loaded again, which
/// every rescan would otherwise do (the same reasoning as for VST3 modules).
pub(crate) struct Module {
    entry: &'static clap_plugin_entry,
    pub(crate) factory: &'static clap_plugin_factory,
}

// SAFETY: the entry and factory are immutable tables of function pointers that
// the CLAP spec requires to be callable from any thread.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

static MODULES: LazyLock<Mutex<HashMap<PathBuf, Arc<Module>>>> = LazyLock::new(Default::default);

/// Loads (once per process) the `.clap` library at `path`.
pub(crate) fn load(path: &Path) -> Result<Arc<Module>> {
    let mut modules = MODULES.lock().unwrap();
    if let Some(module) = modules.get(path) {
        return Ok(module.clone());
    }
    // SAFETY: loading a plugin runs its initializers; that's what hosting means.
    let library = unsafe { libloading::Library::new(path) }.context("couldn't load the library")?;
    let entry: &'static clap_plugin_entry = unsafe {
        let symbol = library
            .get::<*const clap_plugin_entry>(b"clap_entry\0")
            .context("not a CLAP plugin (no clap_entry)")?;
        (*symbol).as_ref().context("clap_entry is null")?
    };
    anyhow::ensure!(
        clap_version_is_compatible(entry.clap_version),
        "unsupported CLAP version {}.{}",
        entry.clap_version.major,
        entry.clap_version.minor
    );
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())?;
    let init = entry.init.context("clap_entry has no init")?;
    anyhow::ensure!(unsafe { init(path_c.as_ptr()) }, "the plugin's entry failed to initialize");
    let factory = unsafe { get_factory(entry, CLAP_PLUGIN_FACTORY_ID) }
        .map(|factory| unsafe { &*(factory as *const clap_plugin_factory) })
        .context("no plugin factory")?;
    // Kept loaded for the rest of the process (see `Module`).
    std::mem::forget(library);
    let module = Arc::new(Module { entry, factory });
    modules.insert(path.to_path_buf(), module.clone());
    Ok(module)
}

unsafe fn get_factory(entry: &clap_plugin_entry, id: &CStr) -> Option<*const std::ffi::c_void> {
    let factory = unsafe { (entry.get_factory?)(id.as_ptr()) };
    (!factory.is_null()).then_some(factory)
}

impl Module {
    /// The factory with `id` (one of the ids tried in order), if the plugin has one.
    pub(crate) fn factory(&self, ids: &[&CStr]) -> Option<*const std::ffi::c_void> {
        ids.iter().find_map(|id| unsafe { get_factory(self.entry, id) })
    }

    /// Every plugin the factory offers.
    pub(crate) fn descriptors(&self) -> Vec<Descriptor> {
        let factory = self.factory;
        let (Some(count), Some(get)) = (factory.get_plugin_count, factory.get_plugin_descriptor) else {
            return Vec::new();
        };
        (0..unsafe { count(factory) })
            .filter_map(|index| {
                let desc = unsafe { get(factory, index).as_ref()? };
                Some(Descriptor {
                    id: unsafe { c_string(desc.id) }.filter(|id| !id.is_empty())?,
                    name: unsafe { c_string(desc.name) }.unwrap_or_default(),
                    vendor: unsafe { c_string(desc.vendor) }.unwrap_or_default(),
                    features: unsafe { features(desc) },
                })
            })
            .collect()
    }
}

/// What a factory says about one of its plugins.
pub(crate) struct Descriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// `CLAP_PLUGIN_FEATURE_*` strings ("instrument", "audio-effect", …).
    pub features: Vec<String>,
}

unsafe fn features(desc: &clap_plugin_descriptor) -> Vec<String> {
    let mut features = Vec::new();
    let mut next = desc.features;
    if next.is_null() {
        return features;
    }
    while !unsafe { *next }.is_null() {
        features.extend(unsafe { c_string(*next) });
        next = unsafe { next.add(1) };
    }
    features
}

/// A C string as an owned `String`, `None` if null.
pub(crate) unsafe fn c_string(ptr: *const c_char) -> Option<String> {
    (!ptr.is_null()).then(|| unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// A fixed-size C char array (a name field) as a `String`.
pub(crate) fn char_array(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
