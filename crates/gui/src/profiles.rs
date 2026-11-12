//! Panel profiles: what an instrument's slot shows on its first tab — an
//! optional fixed section (setBfree's drawbars), then one row of
//! [`ROW_LEN`] cells, each a knob, switch or lever chosen from the profile's
//! pool of controls. See "Panel profiles" in `docs/design/plugins.md`.
//!
//! This is the contract between the profile/engine side ("backend": loading
//! shipped and user profiles, resolving targets, reading and writing values)
//! and the slot UI ("frontend": drawing cells, the context menu). The UI only
//! uses the items below and [`CellValue`]s; every mapping between a control's
//! value and its target (ranges, log scales, setBfree's 0–127, inverted
//! drawbars, choice values) lives here.
//!
//! Stub state: [`Profiles::load`] knows no profiles yet, so every plugin gets
//! the fallback panel (its first five continuous parameters as knobs, pool =
//! every parameter), and setBfree targets do nothing.

#![allow(dead_code)] // until the UI uses all of it

use anyhow::Result;
use live_keys_engine::{Engine, ParamInfo, ParamKind, PluginKey};

/// How many cells the row has.
pub const ROW_LEN: usize = 5;

/// How a control is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Widget {
    /// A knob; snaps to steps and shows option labels if the control has
    /// [`Control::choices`] or steps (a "stepped knob").
    Knob,
    /// The design's `Toggle`: [`Control::text`] on the button, lit when on.
    Switch,
    /// The design's `Lever`: 2–3 positions ([`Control::choices`]).
    Lever,
    /// One of the fixed section's drawbars, 0..=8.
    Drawbar,
}

/// A drawbar's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawbarColour {
    Brown,
    White,
    Black,
}

/// What a control drives.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A plugin parameter, by its `ParamInfo::id` (resolved from the profile's
    /// stable reference when the panel is built).
    Param(u32),
    /// A setBfree control function ("upper.drawbar16"), value 0..=127.
    Setbfree(String),
}

/// One option of a lever or stepped knob.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    /// In the target's own terms (a parameter value, or setBfree's 0..=127).
    pub value: f32,
}

/// One control a panel can show.
#[derive(Clone, Debug)]
pub struct Control {
    /// Stable within the profile ("perc", "param:cutoff"); what a saved row and
    /// later MIDI-learn bindings point at.
    pub id: String,
    /// What the context menu lists ("Perc volume").
    pub name: String,
    /// The caption under the control ("Volume", "16'").
    pub label: String,
    /// A switch's word on the button ("Soft"); empty otherwise.
    pub text: String,
    pub widget: Widget,
    pub target: Target,
    /// For drawbars only.
    pub colour: Option<DrawbarColour>,
    /// Options of a lever or stepped knob, in order; empty for others.
    pub choices: Vec<Choice>,
}

/// A control's value as the UI shows and edits it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellValue {
    /// A knob without choices: 0..1 across its range (log-scaled if the
    /// parameter is logarithmic).
    Position(f32),
    /// A switch.
    On(bool),
    /// A lever or stepped knob: an index into [`Control::choices`]; a drawbar:
    /// its level, 0..=8.
    Index(usize),
}

/// What one instrument's slot shows.
#[derive(Clone, Debug, Default)]
pub struct Panel {
    /// The fixed section's drawbars, left to right; empty for most plugins.
    pub drawbars: Vec<Control>,
    /// Every control the row can show, in menu order. For a plugin without a
    /// profile, one knob per parameter (switch for toggles, stepped knob for
    /// choices).
    pub pool: Vec<Control>,
    /// The row: indices into `pool`, at most [`ROW_LEN`] (fewer if the pool is
    /// smaller). The user's choice if they made one, else the profile's default.
    pub row: Vec<usize>,
}

impl Control {
    /// The current value of this control on the plugin with `key`.
    pub fn read(&self, engine: &Engine, key: PluginKey) -> CellValue {
        match &self.target {
            Target::Param(id) => {
                let value = engine.param_value(key, *id).unwrap_or(0.0);
                let param = engine.plugin(key).and_then(|meta| meta.params.iter().find(|p| p.id == *id));
                match (self.widget, param) {
                    (Widget::Switch, _) => CellValue::On(value >= 0.5),
                    (_, Some(param)) => CellValue::Position(crate::slot_view::position(param, value)),
                    _ => CellValue::Position(0.0),
                }
            }
            Target::Setbfree(_) => CellValue::Position(0.0),
        }
    }

    /// Sets this control on the plugin with `key` (a host-side edit: recorded,
    /// and an open plugin GUI follows where the format allows).
    pub fn write(&self, engine: &mut Engine, key: PluginKey, value: CellValue) {
        if let Target::Param(id) = &self.target {
            let Some(param) = engine.plugin(key).and_then(|meta| meta.params.iter().find(|p| p.id == *id)).cloned()
            else {
                return;
            };
            let value = match value {
                CellValue::Position(position) => crate::slot_view::knob_value(&param, position),
                CellValue::On(on) => if on { param.max_value } else { param.min_value },
                CellValue::Index(index) => self.choices.get(index).map_or(param.default_value, |c| c.value),
            };
            engine.set_param(key, *id, param.snap(value));
        }
    }

    /// The current value as text ("-6.0 dB", "Fast", "C3"), for a knob's hover
    /// text.
    pub fn value_text(&self, engine: &Engine, key: PluginKey) -> String {
        match &self.target {
            Target::Param(id) => engine.param_text(key, *id, engine.param_value(key, *id).unwrap_or(0.0)),
            Target::Setbfree(_) => String::new(),
        }
    }
}

/// Shipped profiles plus the user's own (and their row choices).
#[derive(Default)]
pub struct Profiles {}

impl Profiles {
    /// The shipped profiles (compiled in) and the user's, from
    /// `<data dir>/live-keys/profiles/`.
    pub fn load() -> Self {
        Self::default()
    }

    /// The panel for an instrument with this plugin id and these parameters.
    pub fn panel(&self, _plugin_id: &str, params: &[ParamInfo]) -> Panel {
        let pool: Vec<Control> = params.iter().map(param_control).collect();
        let row = params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == ParamKind::Continuous)
            .map(|(index, _)| index)
            .take(ROW_LEN)
            .collect();
        Panel { drawbars: Vec::new(), pool, row }
    }

    /// Shows pool control `control_id` in cell `cell` (0-based) of this plugin's
    /// row, and saves that choice as the user's (per plugin). If the control was
    /// already in another cell, the two swap.
    pub fn set_cell(&mut self, _plugin_id: &str, _params: &[ParamInfo], _cell: usize, _control_id: &str) -> Result<()> {
        Ok(())
    }

    /// Forgets the user's row for this plugin: back to the profile's default.
    pub fn reset_row(&mut self, _plugin_id: &str) -> Result<()> {
        Ok(())
    }
}

/// The generic control for a parameter (a plugin without a profile, or "More
/// parameters…"): its id is `param:<ParamInfo::id>`.
pub fn param_control(param: &ParamInfo) -> Control {
    Control {
        id: format!("param:{}", param.id),
        name: param.name.clone(),
        label: param.name.clone(),
        text: if param.kind == ParamKind::Toggle { "On".into() } else { String::new() },
        widget: if param.kind == ParamKind::Toggle { Widget::Switch } else { Widget::Knob },
        target: Target::Param(param.id),
        colour: None,
        choices: param
            .choices
            .iter()
            .filter(|_| param.kind == ParamKind::Choice)
            .map(|c| Choice { label: c.label.clone(), value: c.value })
            .collect(),
    }
}
