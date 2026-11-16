//! live-keys' sampled grand piano: the Salamander Grand Piano samples (CC BY 3.0,
//! Alexander Holm) played by the SFZ engine through our own SFZ
//! (`salamander.sfz`, adapted from sfzinstruments'; see its header), with the
//! controls a panel shows ([`CONTROLS`]) and shipped presets ([`PRESETS`]).
//!
//! No host code: a front-end (live-keys' built-in one, a standalone plugin later)
//! loads the instrument, makes a `Synth` and applies the controls to it.
//!
//! The samples aren't part of the build: they're downloaded into
//! [`default_sample_dir`] (a copy of sfzinstruments/SalamanderGrandPiano).

use anyhow::{Context, Result};
use live_keys_sfz::{Control, ControlTarget, Instrument};
use std::path::{Path, PathBuf};

/// Stable name of this instrument (front-ends derive their ids from it).
pub const ID: &str = "piano";
pub const NAME: &str = "Grand Piano";

/// The piano's SFZ; its includes and samples resolve against the sample folder.
pub const SFZ: &str = include_str!("../salamander.sfz");

/// Polyphony: notes, release noises and resonances, pedal noises.
pub const VOICES: usize = 256;

const fn control(
    symbol: &'static str,
    name: &'static str,
    (min, max, default): (f32, f32, f32),
    unit: &'static str,
    cc: u16,
) -> Control {
    Control { symbol, name, min, max, default, unit, choices: &[], target: ControlTarget::Cc(cc) }
}

/// The controls, each an SFZ controller of `salamander.sfz` (or its keyswitch).
pub const CONTROLS: [Control; 9] = [
    control("dynamics", "Dynamics", (0.0, 100.0, 73.0), "%", 99),
    control("brightness", "Brightness", (-12.0, 12.0, 0.0), "dB", 23),
    control("release", "Release", (0.0, 2.0, 1.0), "s", 72),
    control("resonance", "String resonance", (0.0, 200.0, 100.0), "%", 20),
    control("key_noise", "Key noise", (0.0, 200.0, 100.0), "%", 21),
    control("pedal_noise", "Pedal noise", (0.0, 200.0, 100.0), "%", 22),
    control("width", "Stereo width", (0.0, 100.0, 100.0), "%", 25),
    control("tune", "Tune", (-100.0, 100.0, 0.0), "ct", 24),
    Control {
        symbol: "tuning",
        name: "Tuning",
        min: 0.0,
        max: 1.0,
        default: 0.0,
        unit: "",
        choices: &["Natural", "Retuned"],
        // $NATURAL (C0) and $RETUNED (C#0)
        target: ControlTarget::Keyswitch(&[12, 13]),
    },
];

/// Shipped presets: a name and the controls that differ from their defaults.
pub const PRESETS: &[(&str, &[(&str, f32)])] = &[
    ("Concert Grand", &[]),
    ("Soft and Warm", &[("brightness", -6.0), ("dynamics", 85.0), ("resonance", 130.0), ("key_noise", 60.0), ("release", 1.4)]),
    ("Bright Pop", &[("brightness", 6.0), ("dynamics", 55.0), ("resonance", 80.0), ("width", 80.0)]),
    ("Felt", &[("brightness", -11.0), ("dynamics", 90.0), ("key_noise", 160.0), ("pedal_noise", 150.0), ("release", 0.6)]),
    ("Retuned", &[("tuning", 1.0)]),
];

/// Where the samples are downloaded: `<data dir>/live-keys/samples/salamander/`.
pub fn default_sample_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("live-keys").join("samples").join("salamander"))
}

/// Loads the piano from the Salamander sample folder (streaming its samples).
pub fn load(sample_dir: &Path) -> Result<Instrument> {
    if !sample_dir.join("Samples").is_dir() {
        anyhow::bail!(
            "the Salamander Grand Piano samples aren't installed (expected in {})",
            sample_dir.display()
        );
    }
    Instrument::from_sfz(SFZ, Some(sample_dir)).context("loading the piano")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_name_real_controls_within_range() {
        for (name, values) in PRESETS {
            for (symbol, value) in *values {
                let control = CONTROLS.iter().find(|c| c.symbol == *symbol).unwrap_or_else(|| panic!("{name}: {symbol}"));
                assert!((control.min..=control.max).contains(value), "{name}: {symbol} = {value}");
            }
        }
    }

    #[test]
    fn sfz_parses_and_its_controller_defaults_match_the_controls() {
        // Without samples no regions load, but the <control> header is read.
        let sections = live_keys_sfz::parse::parse_str(SFZ, None);
        assert!(sections.is_err(), "includes need the sample folder");
        let dir = std::env::temp_dir().join(format!("live-keys-piano-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        for name in ["tune_nat", "tune_ret", "notes", "str_res", "hammer", "pedal"] {
            std::fs::write(dir.join(format!("Data/{name}.txt")), "").unwrap();
        }
        let instrument = Instrument::from_sfz(SFZ, Some(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        let instrument = instrument.unwrap();
        for control in &CONTROLS {
            if let ControlTarget::Cc(cc) = control.target {
                let expected = control.normalized(control.default);
                assert!((instrument.initial_cc(cc) - expected).abs() < 1e-6, "{}", control.symbol);
            }
        }
    }
}
