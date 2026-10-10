//! `.vstpreset` files: the VST3 SDK's preset format, which every VST3 host reads
//! and writes, found in the standard preset folders and matched to a plugin by
//! the class ID in their header.
//!
//! Layout (little-endian): a 48-byte header — `"VST3"`, version (`i32`, 1), the
//! class ID as 32 ASCII hex digits, the chunk list's offset (`i64`) — then the
//! chunks' data, then the chunk list: `"List"`, a count (`i32`), and per chunk an
//! id (4 bytes: `"Comp"` = the component's state, `"Cont"` = the controller's,
//! `"Info"` = metadata), offset (`i64`) and size (`i64`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use live_keys_core::PresetInfo;

const HEADER_SIZE: usize = 48;
const EXTENSION: &str = "vstpreset";
/// Standard Linux preset folders, per the VST3 SDK's "Preset Locations": the
/// user's first, which is also where new presets go.
const PRESET_PATHS: &[&str] = &["~/.vst3/presets", "/usr/share/vst3/presets", "/usr/local/share/vst3/presets"];
const MAX_SCAN_DEPTH: usize = 6;

/// One preset file's contents.
pub(crate) struct PresetFile {
    pub component: Vec<u8>,
    pub controller: Option<Vec<u8>>,
}

fn expand(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|home| home.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

fn user_root() -> Option<PathBuf> {
    expand(PRESET_PATHS[0])
}

/// The folder new presets of a plugin go to: `~/.vst3/presets/<vendor>/<plugin>`.
pub(crate) fn user_dir(vendor: &str, plugin: &str) -> Option<PathBuf> {
    let vendor = if vendor.is_empty() { "Unknown" } else { vendor };
    Some(user_root()?.join(file_name(vendor)).join(file_name(plugin)))
}

/// `name` made safe as a file name.
pub(crate) fn file_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let name = name.trim().trim_start_matches('.').to_string();
    if name.is_empty() { "Preset".to_string() } else { name }
}

/// Every preset file below the standard folders whose class ID is `class_id`
/// (hex, any case), sorted by name.
pub(crate) fn find(class_id: &str) -> Vec<PresetInfo> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == EXTENSION) {
                found.push(path);
            } else if depth < MAX_SCAN_DEPTH && path.is_dir() {
                walk(&path, depth + 1, found);
            }
        }
    }
    let mut files = Vec::new();
    for root in PRESET_PATHS.iter().filter_map(|path| expand(path)) {
        walk(&root, 0, &mut files);
    }
    let user_root = user_root();
    let mut presets: Vec<PresetInfo> = files
        .into_iter()
        .filter(|path| read_class_id(path).is_some_and(|id| id.eq_ignore_ascii_case(class_id)))
        .map(|path| PresetInfo {
            name: path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default(),
            user: user_root.as_ref().is_some_and(|root| path.starts_with(root)),
            id: path.display().to_string(),
        })
        .collect();
    presets.sort_by_key(|preset| preset.name.to_lowercase());
    presets.dedup_by(|a, b| a.id == b.id);
    presets
}

fn read_class_id(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut header = [0u8; HEADER_SIZE];
    std::fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    (&header[..4] == b"VST3").then(|| String::from_utf8_lossy(&header[8..40]).into_owned())
}

/// Reads the preset at `path`, which must be for class `class_id`.
pub(crate) fn read(path: &Path, class_id: &str) -> Result<PresetFile> {
    let data = std::fs::read(path).with_context(|| format!("couldn't read {}", path.display()))?;
    anyhow::ensure!(data.len() >= HEADER_SIZE && &data[..4] == b"VST3", "not a VST3 preset");
    let file_class = String::from_utf8_lossy(&data[8..40]);
    anyhow::ensure!(file_class.eq_ignore_ascii_case(class_id), "preset is for another plugin");

    let i32_at = |at: usize| data.get(at..at + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()));
    let i64_at = |at: usize| data.get(at..at + 8).map(|b| i64::from_le_bytes(b.try_into().unwrap()));
    let list = i64_at(40).context("truncated preset")? as usize;
    anyhow::ensure!(data.get(list..list + 4) == Some(b"List"), "preset has no chunk list");
    let count = i32_at(list + 4).context("truncated preset")?.max(0) as usize;
    let (mut component, mut controller) = (None, None);
    for entry in 0..count {
        let at = list + 8 + entry * 20;
        let id = data.get(at..at + 4).context("truncated preset")?;
        let offset = i64_at(at + 4).context("truncated preset")? as usize;
        let size = i64_at(at + 12).context("truncated preset")? as usize;
        let chunk = data.get(offset..offset + size).context("preset chunk out of range")?.to_vec();
        match id {
            b"Comp" => component = Some(chunk),
            b"Cont" => controller = Some(chunk),
            _ => {}
        }
    }
    Ok(PresetFile {
        component: component.context("preset has no component state")?,
        controller,
    })
}

/// Writes a preset for class `class_id` to `path`, creating its folder.
pub(crate) fn write(path: &Path, class_id: &str, component: &[u8], controller: &[u8]) -> Result<()> {
    let mut data = Vec::with_capacity(HEADER_SIZE + component.len() + controller.len() + 48);
    data.extend_from_slice(b"VST3");
    data.extend_from_slice(&1i32.to_le_bytes());
    let mut id = class_id.to_ascii_uppercase().into_bytes();
    id.resize(32, b'0');
    data.extend_from_slice(&id);
    data.extend_from_slice(&0i64.to_le_bytes()); // list offset, patched below
    let mut entries = Vec::new();
    for (chunk_id, chunk) in [(b"Comp", component), (b"Cont", controller)] {
        entries.push((chunk_id, data.len() as i64, chunk.len() as i64));
        data.extend_from_slice(chunk);
    }
    let list = data.len() as i64;
    data[40..48].copy_from_slice(&list.to_le_bytes());
    data.extend_from_slice(b"List");
    data.extend_from_slice(&(entries.len() as i32).to_le_bytes());
    for (chunk_id, offset, size) in entries {
        data.extend_from_slice(chunk_id);
        data.extend_from_slice(&offset.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    }
    std::fs::write(path, data).with_context(|| format!("couldn't write {}", path.display()))
}

/// Deletes a user preset file (and its plugin/vendor folders once empty).
pub(crate) fn delete(path: &Path) -> Result<()> {
    let root = user_root().context("no home folder")?;
    anyhow::ensure!(path.starts_with(&root), "only presets you saved can be deleted");
    std::fs::remove_file(path).with_context(|| format!("couldn't delete {}", path.display()))?;
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root || std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_presets_read_back() {
        let dir = std::env::temp_dir().join(format!("live-keys-preset-test-{}", std::process::id()));
        let path = dir.join("p.vstpreset");
        let class_id = "0123456789abcdef0123456789ABCDEF";
        write(&path, class_id, b"component bytes", b"ctl").unwrap();
        assert_eq!(read_class_id(&path).unwrap(), class_id.to_ascii_uppercase());
        let preset = read(&path, class_id).unwrap();
        assert_eq!(preset.component, b"component bytes");
        assert_eq!(preset.controller.as_deref(), Some(&b"ctl"[..]));
        assert!(read(&path, "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
