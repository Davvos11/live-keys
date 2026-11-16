//! live-keys' sampled grand piano: the Salamander Grand Piano samples (CC BY 3.0,
//! Alexander Holm) played by the SFZ engine through our own SFZ
//! (`salamander.sfz`, adapted from sfzinstruments'; see its header).
//!
//! The samples aren't part of the build: they're downloaded into
//! `<data dir>/live-keys/samples/salamander/` (a copy of
//! sfzinstruments/SalamanderGrandPiano), which `load` reads from.

use anyhow::Result;
use live_keys_sfz::Instrument;
use std::path::Path;

/// The piano's SFZ; its includes and samples resolve against the sample folder.
pub const SFZ: &str = include_str!("../salamander.sfz");

/// Loads the piano from the Salamander sample folder.
pub fn load(sample_dir: &Path) -> Result<Instrument> {
    Instrument::from_sfz(SFZ, Some(sample_dir))
}
