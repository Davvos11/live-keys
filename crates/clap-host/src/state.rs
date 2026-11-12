//! Plugin state as bytes (`clap_ostream`/`clap_istream`), and the state files
//! this host keeps it in.
//!
//! CLAP has no standard preset file format — a plugin's own presets are found
//! through preset discovery (see `preset.rs`), and hosts save user presets their
//! own way. Ours is a small file: [`MAGIC`], the plugin id's length (u32, little
//! endian) and the id, then the state the plugin saved.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{Context, Result};
use clap_sys::stream::{clap_istream, clap_ostream};

const MAGIC: &[u8; 8] = b"LKCLAPS1";
/// The extension of a state file.
pub const EXTENSION: &str = "clapstate";

/// A `clap_ostream` appending to `buffer`.
pub(crate) fn ostream(buffer: &mut Vec<u8>) -> clap_ostream {
    clap_ostream { ctx: buffer as *mut Vec<u8> as *mut c_void, write: Some(write) }
}

unsafe extern "C" fn write(stream: *const clap_ostream, data: *const c_void, size: u64) -> i64 {
    let buffer = unsafe { &mut *((*stream).ctx as *mut Vec<u8>) };
    if size > 0 {
        if data.is_null() {
            return -1;
        }
        buffer.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) });
    }
    size as i64
}

/// Bytes read through a `clap_istream`.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn istream(&mut self) -> clap_istream {
        clap_istream { ctx: self as *mut Self as *mut c_void, read: Some(read) }
    }
}

unsafe extern "C" fn read(stream: *const clap_istream, data: *mut c_void, size: u64) -> i64 {
    let reader = unsafe { &mut *((*stream).ctx as *mut Reader) };
    let rest = &reader.bytes[reader.position..];
    let count = rest.len().min(size as usize);
    if count > 0 {
        if data.is_null() {
            return -1;
        }
        unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), data as *mut u8, count) };
    }
    reader.position += count;
    count as i64
}

/// Writes `state` of plugin `plugin_id` to `path`.
pub(crate) fn write_file(path: &Path, plugin_id: &str, state: &[u8]) -> Result<()> {
    let mut bytes = Vec::with_capacity(MAGIC.len() + 4 + plugin_id.len() + state.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(plugin_id.len() as u32).to_le_bytes());
    bytes.extend_from_slice(plugin_id.as_bytes());
    bytes.extend_from_slice(state);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes).with_context(|| format!("couldn't write {}", path.display()))
}

/// The plugin state in the file at `path`, which must be for `plugin_id`.
pub(crate) fn read_file(path: &Path, plugin_id: &str) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let (id, state) = parse(&bytes).context("not a CLAP state file")?;
    anyhow::ensure!(id == plugin_id.as_bytes(), "this file is for another plugin");
    Ok(state.to_vec())
}

fn parse(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let (length, rest) = rest.split_first_chunk::<4>()?;
    let length = u32::from_le_bytes(*length) as usize;
    (rest.len() >= length).then(|| rest.split_at(length))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("live-keys-clap-state-{}", std::process::id()));
        let path = dir.join("a.clapstate");
        write_file(&path, "org.example.synth", b"\0state\xff").unwrap();
        assert_eq!(read_file(&path, "org.example.synth").unwrap(), b"\0state\xff");
        assert!(read_file(&path, "org.example.other").is_err());
        std::fs::write(&path, b"LKCLAPS1\xff\0\0\0x").unwrap();
        assert!(read_file(&path, "x").is_err(), "a length past the end must not parse");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
