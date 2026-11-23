//! What a slot shows besides its chain: the header photo picked from the
//! instrument's name, its panel's cells and drawbars as the UI shows them, and
//! the snapshot a cleared slot can be brought back from.

use std::path::PathBuf;

use anyhow::{Context, Result};
use live_keys_engine::{Engine, ParamInfo, ParamKind, PluginKey};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::profiles::{CellValue, Control, DrawbarColour, Panel, Target, Widget};
use crate::{DrawbarCell, PanelCell, PanelPoolRow, config, slot_presets};

/// How many pool controls a cell's context menu lists at most (besides those in
/// the row); the rest are under "More parameters…".
const MENU_POOL_MAX: usize = 16;

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
pub fn position(param: &ParamInfo, value: f32) -> f32 {
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

/// The inverse of [`position`]: the value of `param` at knob position 0..1.
pub fn knob_value(param: &ParamInfo, position: f32) -> f32 {
    let (min, max) = (param.min_value, param.max_value);
    let position = position.clamp(0.0, 1.0);
    if param.logarithmic && min > 0.0 && max > min {
        min * (max / min).powf(position)
    } else {
        min + (max - min) * position
    }
}

/// The shown controls of `panel` for the plugin with `key`, at current values,
/// then — while there's room and the pool has more — an empty cell (`widget`
/// -1, a + whose menu adds one).
pub fn panel_cells(engine: &Engine, key: PluginKey, panel: &Panel) -> Vec<PanelCell> {
    let mut cells: Vec<PanelCell> = panel
        .row
        .iter()
        .filter_map(|&index| panel.pool.get(index))
        .map(|control| panel_cell(engine, key, control))
        .collect();
    let params = engine.plugin(key).map_or(0, |meta| meta.params.len());
    if cells.len() < panel.max && panel.pool.len().max(params) > cells.len() {
        cells.push(PanelCell { widget: -1, ..Default::default() });
    }
    cells
}

/// `control`'s cell at its current value.
fn panel_cell(engine: &Engine, key: PluginKey, control: &Control) -> PanelCell {
    let value = control.read(engine, key);
    let param = param_of(engine, key, control);
    let count = control.choices.len();
    let index = choice_index(value, count);
    let position = match value {
        CellValue::Position(position) => position,
        CellValue::On(on) => f32::from(u8::from(on)),
        CellValue::Index(index) => index as f32 / (count.max(2) - 1) as f32,
    };
    // A reset goes to the parameter's default; with nothing to go by (a
    // setBfree control), it stays where it is.
    let default_position = param.map_or(position, |param| self::position(param, param.default_value));
    let default_index = param
        .and_then(|param| {
            control
                .choices
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    (a.value - param.default_value).abs().total_cmp(&(b.value - param.default_value).abs())
                })
                .map(|(index, _)| index)
        })
        .unwrap_or(index);
    // Stepped parameters step on a knob too, rather than snapping only once
    // sent (which would fight a slow drag).
    let steps = match param {
        Some(param)
            if count == 0
                && matches!(param.kind, ParamKind::Stepped | ParamKind::Note)
                && !param.logarithmic =>
        {
            param.steps as i32
        }
        _ => 0,
    };
    PanelCell {
        widget: match control.widget {
            Widget::Knob | Widget::Drawbar => 0,
            Widget::Switch => 1,
            Widget::Lever => 2,
        },
        label: control.label.as_str().into(),
        text: control.text.as_str().into(),
        position,
        default_position,
        steps,
        on: match value {
            CellValue::On(on) => on,
            CellValue::Position(position) => position >= 0.5,
            CellValue::Index(index) => index > 0,
        },
        choices: ModelRc::new(VecModel::from(
            control.choices.iter().map(|choice| SharedString::from(choice.label.as_str())).collect::<Vec<_>>(),
        )),
        index: index as i32,
        default_index: default_index as i32,
        value_text: control.value_text(engine, key).into(),
        // Filled by `MappingsUi::decorate`.
        ..Default::default()
    }
}

/// The parameter `control` drives, if it drives one.
fn param_of<'a>(engine: &'a Engine, key: PluginKey, control: &Control) -> Option<&'a ParamInfo> {
    let Target::Param(id) = control.target else {
        return None;
    };
    engine.plugin(key)?.params.iter().find(|param| param.id == id)
}

/// `value` as an index into `count` options.
fn choice_index(value: CellValue, count: usize) -> usize {
    let last = count.saturating_sub(1);
    match value {
        CellValue::Index(index) => index.min(last),
        CellValue::Position(position) => (position.clamp(0.0, 1.0) * last as f32).round() as usize,
        CellValue::On(on) => if on { last } else { 0 },
    }
}

/// The fixed section's drawbars of `panel` for the plugin with `key`.
pub fn panel_drawbars(engine: &Engine, key: PluginKey, panel: &Panel) -> Vec<DrawbarCell> {
    panel
        .drawbars
        .iter()
        .map(|control| DrawbarCell {
            label: control.label.as_str().into(),
            colour: match control.colour {
                Some(DrawbarColour::Brown) => 0,
                Some(DrawbarColour::White) | None => 1,
                Some(DrawbarColour::Black) => 2,
            },
            level: choice_index(control.read(engine, key), 9) as i32,
        })
        .collect()
}

/// The pool as a cell's context menu lists it: every control if there are few,
/// else the first [`MENU_POOL_MAX`] and those in the row.
pub fn panel_pool(panel: &Panel) -> Vec<PanelPoolRow> {
    panel
        .pool
        .iter()
        .enumerate()
        .filter_map(|(index, control)| {
            let cell = panel.row.iter().position(|&shown| shown == index);
            (index < MENU_POOL_MAX || cell.is_some()).then(|| PanelPoolRow {
                index: index as i32,
                name: control.name.as_str().into(),
                cell: cell.map_or(-1, |cell| cell as i32),
            })
        })
        .collect()
}

/// Makes `model` hold `cells` like `sync_model`, but treating cells whose
/// choices read the same as equal (each refresh makes new choice models), so
/// unchanged cells aren't touched.
pub fn sync_cells(model: &VecModel<PanelCell>, cells: Vec<PanelCell>) {
    if model.row_count() != cells.len() {
        model.set_vec(cells);
        return;
    }
    for (index, mut cell) in cells.into_iter().enumerate() {
        let Some(old) = model.row_data(index) else {
            continue;
        };
        if old.choices.iter().eq(cell.choices.iter()) {
            cell.choices = old.choices.clone();
        }
        if old != cell {
            model.set_row_data(index, cell);
        }
    }
}

/// A cleared slot, as it was: enough to put it back.
pub struct Closed {
    /// What the slot showed, for "Closed "…"".
    pub name: String,
    /// The chain, saved as a slot preset.
    pub dir: PathBuf,
    pub routing: config::SlotRouting,
}

/// Saves `slot`'s chain where [`Closed`] keeps it, before the slot is cleared.
pub fn snapshot(
    engine: &mut Engine,
    slot: usize,
    plugin_extras: impl Fn(PluginKey) -> slot_presets::PluginExtras,
) -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data folder to keep the cleared slot in")?
        .join("live-keys")
        .join("undo")
        .join(format!("slot-{}", slot + 1));
    std::fs::create_dir_all(dir.parent().context("no parent folder")?)?;
    slot_presets::replace_dir(&dir, |temp| slot_presets::write(engine, slot, "Undo", temp, plugin_extras))?;
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
