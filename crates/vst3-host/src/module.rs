//! Finding `.vst3` bundles on disk and loading their shared libraries.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use libloading::os::unix::{Library, RTLD_LAZY, RTLD_LOCAL};
use vst3::ComPtr;
use vst3::Steinberg::{
    IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait, PClassInfo,
    PClassInfo2, TUID, kResultOk,
};

use crate::com::char8_to_string;

/// Standard Linux VST3 install locations, per the VST3 SDK's "Plug-in Locations".
const STANDARD_VST3_PATHS: &[&str] = &["~/.vst3", "/usr/lib/vst3", "/usr/local/lib/vst3"];
/// How deep to look for `.vst3` bundles below a search folder — vendors often
/// group theirs in a subfolder.
const MAX_SCAN_DEPTH: usize = 4;
/// `PClassInfo::category` of a class that's an audio processor, as opposed to e.g.
/// its separate edit controller class.
const AUDIO_MODULE_CATEGORY: &str = "Audio Module Class";

pub fn default_search_paths() -> Vec<PathBuf> {
    STANDARD_VST3_PATHS
        .iter()
        .map(|path| match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
            None => PathBuf::from(path),
        })
        .collect()
}

/// Every `.vst3` bundle (or legacy single-file `.vst3` library) below `dirs`.
pub(crate) fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "vst3") {
                found.push(path);
            } else if depth < MAX_SCAN_DEPTH && path.is_dir() {
                walk(&path, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    for dir in dirs {
        walk(dir, 0, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

/// The shared library inside a bundle: `<name>.vst3/Contents/<arch>-linux/<name>.so`.
pub(crate) fn bundle_binary(bundle: &Path) -> Option<PathBuf> {
    if bundle.is_file() {
        return Some(bundle.to_path_buf());
    }
    let stem = bundle.file_stem()?;
    let binary = bundle
        .join("Contents")
        .join(format!("{}-linux", std::env::consts::ARCH))
        .join(stem)
        .with_extension("so");
    binary.is_file().then_some(binary)
}

/// A loaded VST3 library and its class factory.
pub(crate) struct Module {
    pub factory: ComPtr<IPluginFactory>,
    _library: Library,
}

/// Every module loaded so far, by library path. Modules are loaded once and never
/// unloaded (`ModuleExit` is never called): plugin frameworks — JUCE especially —
/// keep process-wide state that doesn't survive being unloaded and loaded again,
/// and a rescan would otherwise do exactly that.
static MODULES: LazyLock<Mutex<HashMap<PathBuf, Arc<Module>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn load(binary: &Path) -> Result<Arc<Module>> {
    let mut modules = MODULES.lock().unwrap();
    if let Some(module) = modules.get(binary) {
        return Ok(module.clone());
    }

    // SAFETY: loading a plugin runs its initializers — unavoidable for hosting it.
    let library = unsafe { Library::open(Some(binary), RTLD_LAZY | RTLD_LOCAL) }
        .with_context(|| format!("failed to load {}", binary.display()))?;
    // `ModuleEntry` takes the platform library handle (the SDK keeps it to find
    // the bundle's resources). Round-tripped through `into_raw`/`from_raw` to get
    // at it without closing the library.
    let handle = library.into_raw();
    let library = unsafe { Library::from_raw(handle) };

    unsafe {
        let module_entry = library
            .get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"ModuleEntry\0")
            .context("not a VST3 module: no ModuleEntry")?;
        anyhow::ensure!(module_entry(handle), "ModuleEntry failed");
    }
    let factory = unsafe {
        let get_factory = library
            .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0")
            .context("not a VST3 module: no GetPluginFactory")?;
        ComPtr::from_raw(get_factory()).context("GetPluginFactory returned null")?
    };

    let module = Arc::new(Module {
        factory,
        _library: library,
    });
    modules.insert(binary.to_path_buf(), module.clone());
    Ok(module)
}

/// One plugin class a module offers.
pub(crate) struct ClassInfo {
    pub cid: TUID,
    pub name: String,
    /// From the class's sub-categories ("Instrument|Synth", "Fx|Delay").
    pub is_instrument: bool,
}

/// The audio-processor classes (i.e. loadable plugins) `module` offers.
pub(crate) fn audio_classes(module: &Module) -> Vec<ClassInfo> {
    let factory = &module.factory;
    let factory2 = factory.cast::<IPluginFactory2>();
    let count = unsafe { factory.countClasses() };
    (0..count)
        .filter_map(|index| {
            // Prefer `PClassInfo2` for its sub-categories ("Instrument|Synth",
            // "Fx|Delay"); without it there's no way to tell an instrument apart.
            let (cid, category, name, sub_categories) = match &factory2 {
                Some(factory2) => {
                    let mut info: PClassInfo2 = unsafe { std::mem::zeroed() };
                    if unsafe { factory2.getClassInfo2(index, &mut info) } != kResultOk {
                        return None;
                    }
                    (info.cid, info.category, info.name, char8_to_string(&info.subCategories))
                }
                None => {
                    let mut info: PClassInfo = unsafe { std::mem::zeroed() };
                    if unsafe { factory.getClassInfo(index, &mut info) } != kResultOk {
                        return None;
                    }
                    (info.cid, info.category, info.name, String::new())
                }
            };
            (char8_to_string(&category) == AUDIO_MODULE_CATEGORY).then(|| ClassInfo {
                cid,
                name: char8_to_string(&name),
                is_instrument: sub_categories.split('|').any(|sub| sub == "Instrument"),
            })
        })
        .collect()
}

pub(crate) fn cid_to_hex(cid: &TUID) -> String {
    cid.iter().map(|&byte| format!("{:02X}", byte as u8)).collect()
}
