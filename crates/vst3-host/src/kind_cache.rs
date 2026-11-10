//! A cache of what [`plugin::probe_kind`](crate::plugin::probe_kind) found, so a
//! scan only probes classes of binaries that are new or changed since the last
//! one. Probing creates and initializes every effect class — ~45 ms each for
//! LSP's, which has nearly 200 — and was most of the app's startup time.
//!
//! One line per probed class: binary path, size, modification time (ns since the
//! epoch), class ID in hex, `midi`/`audio`, tab-separated. A binary whose size or
//! time differs is probed again. The file is rewritten after every scan with only
//! what that scan saw, so uninstalled plugins drop out.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use live_keys_core::PluginKind;

/// A binary as it was when probed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct BinaryStamp {
    path: PathBuf,
    size: u64,
    modified_ns: u128,
}

impl BinaryStamp {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified_ns = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        Some(Self { path: path.to_path_buf(), size: metadata.len(), modified_ns })
    }
}

#[derive(Default)]
pub(crate) struct KindCache {
    old: HashMap<(BinaryStamp, String), PluginKind>,
    seen: Vec<(BinaryStamp, String, PluginKind)>,
}

fn cache_file() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("live-keys").join("vst3-kinds.tsv"))
}

impl KindCache {
    /// The cache as the last scan left it; empty if there is none or it can't be read.
    pub(crate) fn load() -> Self {
        let old = cache_file()
            .and_then(|file| fs::read_to_string(file).ok())
            .map(|text| text.lines().filter_map(parse_line).collect())
            .unwrap_or_default();
        Self { old, seen: Vec::new() }
    }

    /// The cached kind of class `cid_hex` in the binary `stamp`, else `probe()`'s
    /// (cached, unless it gives up). Either way, it's kept by [`Self::save`].
    pub(crate) fn kind(
        &mut self,
        stamp: Option<&BinaryStamp>,
        cid_hex: &str,
        probe: impl FnOnce() -> Option<PluginKind>,
    ) -> Option<PluginKind> {
        let Some(stamp) = stamp else { return probe() };
        let key = (stamp.clone(), cid_hex.to_string());
        let kind = match self.old.get(&key) {
            Some(&kind) => kind,
            None => probe()?,
        };
        self.seen.push((key.0, key.1, kind));
        Some(kind)
    }

    /// Writes what this scan saw, replacing the file. Failing only costs the next
    /// scan its probes, so it's just logged.
    pub(crate) fn save(&self) {
        let Some(file) = cache_file() else { return };
        let mut text = String::new();
        for (stamp, cid_hex, kind) in &self.seen {
            let Some(path) = stamp.path.to_str().filter(|path| !path.contains(['\t', '\n'])) else {
                continue;
            };
            let kind = if *kind == PluginKind::MidiEffect { "midi" } else { "audio" };
            text += &format!("{path}\t{}\t{}\t{cid_hex}\t{kind}\n", stamp.size, stamp.modified_ns);
        }
        let result = file
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&file, text));
        if let Err(err) = result {
            log::warn!("couldn't write {}: {err}", file.display());
        }
    }
}

fn parse_line(line: &str) -> Option<((BinaryStamp, String), PluginKind)> {
    let mut fields = line.split('\t');
    let path = PathBuf::from(fields.next()?);
    let size = fields.next()?.parse().ok()?;
    let modified_ns = fields.next()?.parse().ok()?;
    let cid_hex = fields.next()?.to_string();
    let kind = match fields.next()? {
        "midi" => PluginKind::MidiEffect,
        "audio" => PluginKind::AudioEffect,
        _ => return None,
    };
    Some(((BinaryStamp { path, size, modified_ns }, cid_hex), kind))
}
