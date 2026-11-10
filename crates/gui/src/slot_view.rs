//! What a slot shows besides its chain: the header photo picked from the
//! instrument's name, the instrument parameters shown as knobs, and the
//! snapshot a cleared slot can be brought back from.

use std::path::PathBuf;

use anyhow::{Context, Result};
use live_keys_engine::{Engine, LoadedPluginMeta, ParamInfo, ParamKind, PluginKey, PresetInfo};

use crate::{KnobRow, config, slot_presets};

/// How many of the instrument's parameters show as knobs.
pub const KNOB_COUNT: usize = 5;

/// The header photo for an instrument, as numbered by the UI's `Artwork`:
/// 0 generic, 1 piano, 2 electric piano, 3 organ, 4 synth, 5 strings, 6 pad.
/// Matched on words in its name; the first match wins, so the more specific
/// ones come first.
pub fn artwork(instrument_name: Option<&str>) -> i32 {
    const KINDS: [(i32, &[&str]); 6] = [
        (2, &["electric piano", "epiano", "e-piano", "rhodes", "wurli", "mark i", "suitcase"]),
        (3, &["organ", "hammond", "setbfree", "tonewheel", "drawbar", "b3"]),
        (5, &["string", "violin", "viola", "cello", "orchestra", "ensemble"]),
        (6, &["pad", "ambient", "atmos", "drone"]),
        (1, &["piano", "grand", "upright", "concert"]),
        (4, &["synth", "helm", "surge", "vital", "dexed", "odin", "zyn", "juno", "moog", "analog", "bass", "lead"]),
    ];
    let Some(name) = instrument_name else {
        return 0;
    };
    let name = name.to_lowercase();
    KINDS
        .iter()
        .find(|(_, words)| words.iter().any(|word| name.contains(word)))
        .map_or(0, |(artwork, _)| *artwork)
}

/// The curve an effect's FX square shows, from its name: 1 a compressor's
/// transfer curve, 2 a resonant filter's response, 0 none.
pub fn graph(effect_name: &str) -> i32 {
    let name = effect_name.to_lowercase();
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).collect();
    let has = |prefixes: &[&str]| words.iter().any(|word| prefixes.iter().any(|prefix| word.starts_with(prefix)));
    if has(&["compress", "comp", "limit", "leveler"]) {
        1
    } else if has(&["filter", "eq", "lpf", "hpf", "reson", "wah"]) {
        2
    } else {
        0
    }
}

/// A value of `param` as a knob position: 0..1 across its range, on a log scale
/// for logarithmic parameters.
fn position(param: &ParamInfo, value: f32) -> f32 {
    let (min, max) = (param.min_value, param.max_value);
    let position = if param.logarithmic && min > 0.0 && max > min {
        (value / min).ln() / (max / min).ln()
    } else if max > min {
        (value - min) / (max - min)
    } else {
        0.0
    };
    position.clamp(0.0, 1.0)
}

/// The knobs for `instrument`: its first [`KNOB_COUNT`] continuous parameters,
/// at their current values.
pub fn knob_rows(engine: &Engine, instrument: &LoadedPluginMeta) -> Vec<KnobRow> {
    instrument
        .params
        .iter()
        .filter(|param| param.kind == ParamKind::Continuous)
        .take(KNOB_COUNT)
        .map(|param| knob_row(engine, instrument.key, param))
        .collect()
}

pub fn knob_row(engine: &Engine, key: PluginKey, param: &ParamInfo) -> KnobRow {
    let value = engine.param_value(key, param.id).unwrap_or(param.default_value);
    KnobRow {
        key: key as i32,
        param_id: param.id as i32,
        name: param.name.as_str().into(),
        position: position(param, value),
        default_position: position(param, param.default_value),
        min_value: param.min_value,
        max_value: param.max_value,
        logarithmic: param.logarithmic,
        text: engine.param_text(key, param.id, value).into(),
    }
}

/// A cleared slot, as it was: enough to put it back.
pub struct Closed {
    /// What the slot showed, for "Closed "…"".
    pub name: String,
    /// The chain, saved as a slot preset.
    pub dir: PathBuf,
    pub routing: config::SlotRouting,
    /// The library slot preset it showed, and whether it had changed since.
    pub slot_preset: Option<(PathBuf, String, bool)>,
}

/// Saves `slot`'s chain where [`Closed`] keeps it, before the slot is cleared.
pub fn snapshot(
    engine: &mut Engine,
    slot: usize,
    plugin_preset: impl Fn(PluginKey) -> (Option<PresetInfo>, bool),
) -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data folder to keep the cleared slot in")?
        .join("live-keys")
        .join("undo")
        .join(format!("slot-{}", slot + 1));
    std::fs::create_dir_all(dir.parent().context("no parent folder")?)?;
    slot_presets::replace_dir(&dir, |temp| slot_presets::write(engine, slot, "Undo", temp, plugin_preset))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_prefers_the_more_specific_match() {
        assert_eq!(artwork(Some("mda EPiano")), 2);
        assert_eq!(artwork(Some("Electric Piano")), 2);
        assert_eq!(artwork(Some("setBfree DSP Tonewheel Organ")), 3);
        assert_eq!(artwork(Some("Grand Concert Piano")), 1);
        assert_eq!(artwork(Some("Surge XT")), 4);
        assert_eq!(artwork(Some("Something Else")), 0);
        assert_eq!(artwork(None), 0);
    }

    #[test]
    fn graph_matches_whole_words() {
        assert_eq!(graph("x42 Compressor"), 1);
        assert_eq!(graph("LSP Parametric EQ x8"), 2);
        assert_eq!(graph("a-Filter"), 2);
        assert_eq!(graph("Step Sequencer"), 0);
        assert_eq!(graph("Reverb"), 0);
    }
}
