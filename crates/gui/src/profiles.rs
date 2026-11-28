//! Panel profiles: what an instrument's slot shows on its first tab — an
//! optional fixed section (setBfree's drawbars), then up to [`MAX_CONTROLS`]
//! controls ([`MAX_CONTROLS_WITH_DRAWBARS`] under drawbars), each a knob,
//! switch or lever chosen from the profile's pool of controls. See "Panel profiles" in `docs/design/plugins.md`.
//!
//! This is the contract between the profile/engine side ("backend": loading
//! shipped and user profiles, resolving targets, reading and writing values)
//! and the slot UI ("frontend": drawing cells, the context menu). The UI only
//! uses the items below and [`CellValue`]s; every mapping between a control's
//! value and its target (ranges, log scales, setBfree's 0–127, inverted
//! drawbars, choice values) lives here.
//!
//! **Files.** Shipped profiles are `crates/gui/profiles/*.json`, compiled in
//! ([`SHIPPED`]). The user's live in `<data dir>/live-keys/profiles/`: any
//! `*.json` there is a profile (same format, the plugin ids inside), replacing
//! the shipped one for those plugins wholesale — except `rows.json`, which holds
//! the user's row per plugin ([`UserRow`]: the shown controls' ids, plus
//! parameters added through "More parameters…"), written by
//! [`Profiles::add_control`], [`Profiles::remove_control`] and
//! [`Profiles::reset_row`].
//!
//! **Effects** get a panel too ([`PanelKind::Effect`]): at most
//! [`MAX_EFFECT_CONTROLS`] controls, shown as small sliders on the effect's
//! square — its profile's row if it has one (no drawbars), else its first
//! [`DEFAULT_EFFECT_CONTROLS`] continuous parameters. Kept in the same `rows.json`, by plugin id.
//!
//! **Profile format** (JSON, unknown fields rejected): `plugins` (the
//! `PluginInfo::id`s it applies to — one, or variants sharing parameter
//! symbols, like LSP's mono and stereo builds), `fixed` (`"drawbars"` to show `drawbars`), `drawbars` and
//! `controls` (the pool, in menu order) as [`ControlSpec`]s, and `row` (the
//! default row's control ids). A control's `target` is `{ "param": "<symbol>" }`
//! (`ParamInfo::symbol`: an LV2 port symbol, a VST3/CLAP id in decimal) or
//! `{ "setbfree": "<function>" }`; a control whose parameter the plugin doesn't
//! have is left out.

#![allow(dead_code)] // until the UI uses all of it

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use live_keys_engine::{Engine, ParamInfo, ParamKind, PluginKey, PortEvent, ToValue};
use serde::{Deserialize, Serialize};

/// How many controls a panel shows at most (the UI lays them out zigzag, in
/// two rows).
pub const MAX_CONTROLS: usize = 10;
/// How many under drawbars (one row).
pub const MAX_CONTROLS_WITH_DRAWBARS: usize = 5;
/// How many an effect's square shows at most (sliders from its bottom up,
/// over its name or inline display).
pub const MAX_EFFECT_CONTROLS: usize = 4;
/// How many it shows by default without a profile.
pub const DEFAULT_EFFECT_CONTROLS: usize = 2;

/// Whose panel: an instrument's (the slot's first tab) or an effect's (its
/// square's sliders).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Instrument,
    Effect,
}

/// The shipped profiles, compiled in.
const SHIPPED: &[(&str, &str)] = &[
    ("setbfree.json", include_str!("../profiles/setbfree.json")),
    ("lsp-compressor.json", include_str!("../profiles/lsp-compressor.json")),
    ("mda-epiano.json", include_str!("../profiles/mda-epiano.json")),
    ("helm.json", include_str!("../profiles/helm.json")),
    ("surge-xt.json", include_str!("../profiles/surge-xt.json")),
    ("vital.json", include_str!("../profiles/vital.json")),
    ("piano.json", include_str!("../profiles/piano.json")),
];

/// The user's rows, in the user's profile folder.
const ROWS_FILE: &str = "rows.json";

/// setBfree values run 0..=127.
const SETBFREE_MAX: f32 = 127.0;
/// A drawbar's highest level.
const DRAWBAR_LEVELS: usize = 8;

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

/// What a profile changes about how a control maps onto its target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    /// The low end of a parameter's range as the control covers it (a cutoff
    /// knob covering only the useful part), in the parameter's own terms.
    pub min: Option<f32>,
    /// The high end, likewise.
    pub max: Option<f32>,
    /// Steps across the range, for parameters a plugin reports as continuous
    /// (JUCE ones): the knob snaps to them.
    pub steps: Option<u32>,
    /// The control runs the other way (a knob's 0 is the target's max, a
    /// switch is on at the low end).
    pub invert: bool,
    /// The target's value while it isn't known: setBfree only reports
    /// functions that have been set (otherwise 0 is assumed).
    pub default: Option<f32>,
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
    /// The profile's range/steps/direction/default changes; none for a
    /// [`param_control`].
    pub overrides: Overrides,
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
    /// The controls shown, in order: indices into `pool`, at most `max`. The
    /// user's choice if they made one, else the profile's default.
    pub row: Vec<usize>,
    /// How many controls the panel can show: [`MAX_CONTROLS`], or
    /// [`MAX_CONTROLS_WITH_DRAWBARS`] if it has drawbars.
    pub max: usize,
}

impl Control {
    /// The current value of this control on the plugin with `key`.
    pub fn read(&self, engine: &Engine, key: PluginKey) -> CellValue {
        match &self.target {
            Target::Param(id) => {
                let Some(param) = self.param(engine, key, *id) else { return self.unknown_cell() };
                let value = engine.param_value(key, *id).unwrap_or(param.default_value);
                self.param_cell(&param, value)
            }
            Target::Setbfree(function) => self.setbfree_cell(self.setbfree_value(engine, key, function)),
        }
    }

    /// Sets this control on the plugin with `key` (a host-side edit: recorded,
    /// and a VST3 editor follows). For a setBfree target, returns the messages to
    /// pass to the plugin's open LV2 GUI so it shows the change (an LV2 GUI
    /// following a parameter is the caller's business, as for any parameter).
    pub fn write(&self, engine: &mut Engine, key: PluginKey, value: CellValue) -> Vec<PortEvent> {
        match &self.target {
            Target::Param(id) => {
                if let Some(param) = self.param(engine, key, *id) {
                    engine.set_param(key, *id, self.param_value(&param, value));
                }
                Vec::new()
            }
            Target::Setbfree(function) => engine.send_setbfree(key, function, self.setbfree_write(value)),
        }
    }

    /// The current value as text ("-6.0 dB", "Fast", "C3"), for a knob's hover
    /// text.
    pub fn value_text(&self, engine: &Engine, key: PluginKey) -> String {
        match &self.target {
            Target::Param(id) => {
                let Some(param) = self.param(engine, key, *id) else { return String::new() };
                let value = engine.param_value(key, *id).unwrap_or(param.default_value);
                match self.param_cell(&param, value) {
                    CellValue::On(on) => on_off(on),
                    CellValue::Index(index) if !self.choices.is_empty() => self.choice_label(index),
                    CellValue::Index(level) => level.to_string(),
                    CellValue::Position(_) => engine.param_text(key, *id, value),
                }
            }
            Target::Setbfree(function) => {
                let value = self.setbfree_value(engine, key, function);
                match self.setbfree_cell(value) {
                    CellValue::On(on) => on_off(on),
                    CellValue::Index(index) if !self.choices.is_empty() => self.choice_label(index),
                    CellValue::Index(level) => level.to_string(),
                    CellValue::Position(position) => format!("{}%", (position * 100.0).round()),
                }
            }
        }
    }

    /// The control's position, 0..1 along it, for its current value: what
    /// MIDI learn moves (a switch is 0 or 1, a lever or drawbar steps evenly).
    pub fn position(&self, engine: &Engine, key: PluginKey) -> f32 {
        self.position_of(self.read(engine, key))
    }

    /// The position of the target's default value (what a double-click resets
    /// an effect's slider to).
    pub fn default_position(&self, engine: &Engine, key: PluginKey) -> f32 {
        let cell = match &self.target {
            Target::Param(id) => match self.param(engine, key, *id) {
                Some(param) => self.param_cell(&param, param.default_value),
                None => self.unknown_cell(),
            },
            Target::Setbfree(_) => {
                self.setbfree_cell(self.overrides.default.unwrap_or(0.0).clamp(0.0, SETBFREE_MAX) as u8)
            }
        };
        self.position_of(cell)
    }

    fn position_of(&self, cell: CellValue) -> f32 {
        match cell {
            CellValue::Position(position) => position,
            CellValue::On(on) => if on { 1.0 } else { 0.0 },
            CellValue::Index(index) => index as f32 / self.last_index().max(1) as f32,
        }
    }

    /// The cell a position stands for.
    pub fn cell_at(&self, position: f32) -> CellValue {
        let position = position.clamp(0.0, 1.0);
        match self.widget {
            Widget::Switch => CellValue::On(position >= 0.5),
            Widget::Drawbar => CellValue::Index((position * DRAWBAR_LEVELS as f32).round() as usize),
            _ if !self.choices.is_empty() => CellValue::Index((position * self.last_index() as f32).round() as usize),
            Widget::Knob | Widget::Lever => CellValue::Position(position),
        }
    }

    /// The highest [`CellValue::Index`] (0 for controls without steps).
    fn last_index(&self) -> usize {
        match self.widget {
            Widget::Drawbar => DRAWBAR_LEVELS,
            _ => self.choices.len().saturating_sub(1),
        }
    }

    /// Positions of the control's steps (a switch's off and on, a lever's
    /// options), for a MIDI-learn toggle's default levels; empty for a knob
    /// without choices.
    pub fn steps(&self) -> Vec<f32> {
        let last = match self.widget {
            Widget::Switch => 1,
            _ if !self.choices.is_empty() => self.last_index(),
            _ => return Vec::new(),
        };
        (0..=last).map(|index| index as f32 / last.max(1) as f32).collect()
    }

    /// Turns positions into the target's values (a parameter value, or
    /// setBfree's 0..=127), for the mapping thread; `None` if the plugin lacks
    /// the parameter.
    pub fn to_value(&self, engine: &Engine, key: PluginKey) -> Option<ToValue> {
        let control = self.clone();
        match &self.target {
            Target::Param(id) => {
                let param = self.param(engine, key, *id)?;
                Some(std::sync::Arc::new(move |position| control.param_value(&param, control.cell_at(position))))
            }
            Target::Setbfree(_) => {
                Some(std::sync::Arc::new(move |position| f32::from(control.setbfree_write(control.cell_at(position)))))
            }
        }
    }

    /// The value at `position` as text ("-6.0 dB", "Fast", "On").
    pub fn text_at(&self, engine: &Engine, key: PluginKey, position: f32) -> String {
        match self.cell_at(position) {
            CellValue::On(on) => on_off(on),
            CellValue::Index(index) if !self.choices.is_empty() => self.choice_label(index),
            CellValue::Index(level) => level.to_string(),
            CellValue::Position(position) => match &self.target {
                Target::Param(id) => match self.param(engine, key, *id) {
                    Some(param) => engine.param_text(key, *id, self.param_value(&param, CellValue::Position(position))),
                    None => String::new(),
                },
                Target::Setbfree(_) => format!("{}%", (position * 100.0).round()),
            },
        }
    }

    /// The parameter as this control covers it: the plugin's, with the
    /// profile's overrides applied.
    fn param(&self, engine: &Engine, key: PluginKey, id: u32) -> Option<ParamInfo> {
        let param = engine.plugin(key)?.params.iter().find(|p| p.id == id)?;
        Some(self.overrides.apply(param))
    }

    fn unknown_cell(&self) -> CellValue {
        match self.widget {
            Widget::Switch => CellValue::On(false),
            Widget::Drawbar | Widget::Lever => CellValue::Index(0),
            Widget::Knob if !self.choices.is_empty() => CellValue::Index(0),
            Widget::Knob => CellValue::Position(0.0),
        }
    }

    fn choice_label(&self, index: usize) -> String {
        self.choices.get(index).map(|choice| choice.label.clone()).unwrap_or_default()
    }

    /// The choice nearest `value`.
    fn nearest_choice(&self, value: f32) -> usize {
        self.choices
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (a.value - value).abs().total_cmp(&(b.value - value).abs()))
            .map_or(0, |(index, _)| index)
    }

    /// `param` (overrides applied) at `value` as this control shows it.
    fn param_cell(&self, param: &ParamInfo, value: f32) -> CellValue {
        let span = param.max_value - param.min_value;
        let fraction = if span > 0.0 { ((value - param.min_value) / span).clamp(0.0, 1.0) } else { 0.0 };
        let fraction = if self.overrides.invert { 1.0 - fraction } else { fraction };
        match self.widget {
            Widget::Switch => CellValue::On(fraction >= 0.5),
            Widget::Drawbar => CellValue::Index((fraction * DRAWBAR_LEVELS as f32).round() as usize),
            Widget::Knob | Widget::Lever if !self.choices.is_empty() => CellValue::Index(self.nearest_choice(value)),
            Widget::Knob | Widget::Lever => {
                let position = crate::slot_view::position(param, value);
                CellValue::Position(if self.overrides.invert { 1.0 - position } else { position })
            }
        }
    }

    /// The value of `param` (overrides applied) that `cell` stands for.
    fn param_value(&self, param: &ParamInfo, cell: CellValue) -> f32 {
        let at = |fraction: f32| {
            let fraction = if self.overrides.invert { 1.0 - fraction } else { fraction };
            param.min_value + (param.max_value - param.min_value) * fraction
        };
        let value = match cell {
            CellValue::On(on) => at(if on { 1.0 } else { 0.0 }),
            CellValue::Index(index) if !self.choices.is_empty() => {
                self.choices.get(index).map_or(param.default_value, |choice| choice.value)
            }
            CellValue::Index(level) => at(level.min(DRAWBAR_LEVELS) as f32 / DRAWBAR_LEVELS as f32),
            CellValue::Position(position) => {
                let position = if self.overrides.invert { 1.0 - position } else { position };
                crate::slot_view::knob_value(param, position)
            }
        };
        param.snap(value)
    }

    fn setbfree_value(&self, engine: &Engine, key: PluginKey, function: &str) -> u8 {
        engine
            .setbfree_value(key, function)
            .unwrap_or_else(|| self.overrides.default.unwrap_or(0.0).clamp(0.0, SETBFREE_MAX) as u8)
    }

    /// A setBfree value (0..=127) as this control shows it.
    fn setbfree_cell(&self, value: u8) -> CellValue {
        let fraction = f32::from(value) / SETBFREE_MAX;
        let fraction = if self.overrides.invert { 1.0 - fraction } else { fraction };
        match self.widget {
            // setBfree's drawbars are inverted: 0 is all the way out.
            Widget::Drawbar => CellValue::Index(drawbar_level(value)),
            Widget::Switch => CellValue::On(fraction >= 0.5),
            _ if !self.choices.is_empty() => CellValue::Index(self.nearest_choice(f32::from(value))),
            Widget::Knob | Widget::Lever => CellValue::Position(fraction),
        }
    }

    /// The setBfree value (0..=127) `cell` stands for.
    fn setbfree_write(&self, cell: CellValue) -> u8 {
        let at = |fraction: f32| {
            let fraction = if self.overrides.invert { 1.0 - fraction } else { fraction };
            (fraction.clamp(0.0, 1.0) * SETBFREE_MAX).round() as u8
        };
        match cell {
            CellValue::Index(level) if self.widget == Widget::Drawbar => drawbar_value(level),
            CellValue::Index(index) => {
                self.choices.get(index).map_or(0, |choice| choice.value.clamp(0.0, SETBFREE_MAX).round() as u8)
            }
            CellValue::On(on) => at(if on { 1.0 } else { 0.0 }),
            CellValue::Position(position) => at(position),
        }
    }
}

impl Overrides {
    fn apply(&self, param: &ParamInfo) -> ParamInfo {
        let mut param = param.clone();
        if let Some(min) = self.min {
            param.min_value = min;
        }
        if let Some(max) = self.max {
            param.max_value = max;
        }
        if param.logarithmic && param.min_value <= 0.0 {
            param.logarithmic = false;
        }
        if let Some(steps) = self.steps {
            param.steps = steps;
            if param.kind == ParamKind::Continuous {
                param.kind = ParamKind::Stepped;
                param.logarithmic = false;
            }
        }
        param.default_value = self.default.unwrap_or(param.default_value).clamp(param.min_value, param.max_value);
        param
    }
}

fn on_off(on: bool) -> String {
    if on { "On" } else { "Off" }.to_string()
}

/// A setBfree drawbar value as a level: 127 is all the way in (0), 0 all the
/// way out (8).
fn drawbar_level(value: u8) -> usize {
    ((SETBFREE_MAX - f32::from(value.min(127))) * DRAWBAR_LEVELS as f32 / SETBFREE_MAX).round() as usize
}

/// The inverse of [`drawbar_level`].
fn drawbar_value(level: usize) -> u8 {
    let level = level.min(DRAWBAR_LEVELS) as f32;
    ((DRAWBAR_LEVELS as f32 - level) * SETBFREE_MAX / DRAWBAR_LEVELS as f32).round() as u8
}

// ---- The profile files ----

/// A profile file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    /// The `PluginInfo::id`s it applies to.
    plugins: Vec<String>,
    #[serde(default)]
    fixed: Option<Fixed>,
    #[serde(default)]
    drawbars: Vec<ControlSpec>,
    /// The pool, in menu order.
    #[serde(default)]
    controls: Vec<ControlSpec>,
    /// The default row, as control ids.
    #[serde(default)]
    row: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Fixed {
    Drawbars,
}

/// One control in a profile file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlSpec {
    id: String,
    /// The label if absent.
    #[serde(default)]
    name: Option<String>,
    /// The name if absent.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    text: String,
    /// Default: a knob (a drawbar in `drawbars`).
    #[serde(default)]
    widget: Option<WidgetSpec>,
    target: TargetSpec,
    /// Labels with values in the target's terms, or bare labels spread evenly
    /// across the target's range. Default for a parameter: the plugin's own
    /// choices, if it's a choice parameter.
    #[serde(default)]
    choices: Vec<ChoiceSpec>,
    #[serde(default)]
    colour: Option<ColourSpec>,
    #[serde(default)]
    min: Option<f32>,
    #[serde(default)]
    max: Option<f32>,
    #[serde(default)]
    steps: Option<u32>,
    #[serde(default)]
    invert: bool,
    #[serde(default)]
    default: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum WidgetSpec {
    Knob,
    Switch,
    Lever,
    Drawbar,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ColourSpec {
    Brown,
    White,
    Black,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum TargetSpec {
    /// A parameter's `ParamInfo::symbol`.
    Param(String),
    Setbfree(String),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ChoiceSpec {
    Label(String),
    Full { label: String, value: f32 },
}

impl ControlSpec {
    /// The control for a plugin with `params`; `None` if it targets a
    /// parameter the plugin doesn't have.
    fn resolve(&self, params: &[ParamInfo], default_widget: Widget) -> Option<Control> {
        let (target, param) = match &self.target {
            TargetSpec::Param(symbol) => {
                let param = params.iter().find(|p| p.symbol == *symbol)?;
                (Target::Param(param.id), Some(param))
            }
            TargetSpec::Setbfree(function) => (Target::Setbfree(function.clone()), None),
        };
        let overrides = Overrides {
            min: self.min,
            max: self.max,
            steps: self.steps,
            invert: self.invert,
            default: self.default,
        };
        // The range bare choice labels are spread across.
        let (low, high) = match param {
            Some(param) => {
                let param = overrides.apply(param);
                (param.min_value, param.max_value)
            }
            None => (0.0, SETBFREE_MAX),
        };
        let count = self.choices.len();
        let mut choices: Vec<Choice> = self
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| match choice {
                ChoiceSpec::Full { label, value } => Choice { label: label.clone(), value: *value },
                ChoiceSpec::Label(label) => Choice {
                    label: label.clone(),
                    value: if count > 1 { low + (high - low) * index as f32 / (count - 1) as f32 } else { low },
                },
            })
            .collect();
        if choices.is_empty()
            && let Some(param) = param.filter(|p| p.kind == ParamKind::Choice)
        {
            choices = param.choices.iter().map(|c| Choice { label: c.label.clone(), value: c.value }).collect();
        }
        let widget = match self.widget {
            None => default_widget,
            Some(WidgetSpec::Knob) => Widget::Knob,
            Some(WidgetSpec::Switch) => Widget::Switch,
            Some(WidgetSpec::Lever) => Widget::Lever,
            Some(WidgetSpec::Drawbar) => Widget::Drawbar,
        };
        let label = self.label.clone().or_else(|| self.name.clone()).unwrap_or_else(|| self.id.clone());
        Some(Control {
            id: self.id.clone(),
            name: self.name.clone().unwrap_or_else(|| label.clone()),
            label,
            text: self.text.clone(),
            widget,
            target,
            colour: self.colour.map(|colour| match colour {
                ColourSpec::Brown => DrawbarColour::Brown,
                ColourSpec::White => DrawbarColour::White,
                ColourSpec::Black => DrawbarColour::Black,
            }),
            choices,
            overrides,
        })
    }
}

/// The user's row for one plugin, in `rows.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserRow {
    /// The shown controls' ids, in order.
    pub row: Vec<String>,
    /// `param:<id>` controls added through "More parameters…", which stay in
    /// the pool (after the profile's) until the row is reset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

/// Shipped profiles plus the user's own (and their row choices).
#[derive(Default)]
pub struct Profiles {
    /// By plugin id; a user profile has replaced a shipped one.
    profiles: HashMap<String, Rc<ProfileFile>>,
    /// The user's rows, by plugin id.
    rows: BTreeMap<String, UserRow>,
    /// The user's profile folder; `None` if there's no data dir (nothing is
    /// saved then).
    dir: Option<PathBuf>,
}

impl Profiles {
    /// The shipped profiles (compiled in) and the user's, from
    /// `<data dir>/live-keys/profiles/`.
    pub fn load() -> Self {
        Self::load_from(dirs::data_dir().map(|dir| dir.join("live-keys").join("profiles")))
    }

    fn load_from(dir: Option<PathBuf>) -> Self {
        let mut profiles = HashMap::new();
        for (name, json) in SHIPPED {
            match serde_json::from_str::<ProfileFile>(json) {
                Ok(profile) => insert(&mut profiles, profile),
                // A test catches this; never fatal.
                Err(err) => log::error!("shipped profile {name}: {err}"),
            }
        }
        let mut rows = BTreeMap::new();
        if let Some(dir) = &dir {
            for (path, profile) in read_user_profiles(dir) {
                log::info!("Panel profile for {} from {}", profile.plugins.join(", "), path.display());
                insert(&mut profiles, profile);
            }
            match std::fs::read_to_string(dir.join(ROWS_FILE)) {
                Ok(json) => match serde_json::from_str(&json) {
                    Ok(read) => rows = read,
                    Err(err) => log::warn!("ignoring {}: {err}", dir.join(ROWS_FILE).display()),
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::warn!("couldn't read {}: {err}", dir.join(ROWS_FILE).display()),
            }
        }
        Self { profiles, rows, dir }
    }

    /// The panel for an instrument with this plugin id and these parameters.
    pub fn panel(&self, plugin_id: &str, params: &[ParamInfo]) -> Panel {
        self.panel_of(PanelKind::Instrument, plugin_id, params)
    }

    /// The panel for an effect: its square's sliders.
    pub fn effect_panel(&self, plugin_id: &str, params: &[ParamInfo]) -> Panel {
        self.panel_of(PanelKind::Effect, plugin_id, params)
    }

    /// The panel of `kind` for a plugin with this id and these parameters.
    pub fn panel_of(&self, kind: PanelKind, plugin_id: &str, params: &[ParamInfo]) -> Panel {
        let user = self.rows.get(plugin_id);
        let (most, fallback) = match kind {
            PanelKind::Instrument => (MAX_CONTROLS, MAX_CONTROLS),
            PanelKind::Effect => (MAX_EFFECT_CONTROLS, DEFAULT_EFFECT_CONTROLS),
        };
        let (drawbars, mut pool, default_row) = match self.profiles.get(plugin_id) {
            Some(profile) => {
                let drawbars = match profile.fixed {
                    Some(Fixed::Drawbars) if kind == PanelKind::Instrument => {
                        profile.drawbars.iter().filter_map(|spec| spec.resolve(params, Widget::Drawbar)).collect()
                    }
                    _ => Vec::new(),
                };
                let pool: Vec<Control> =
                    profile.controls.iter().filter_map(|spec| spec.resolve(params, Widget::Knob)).collect();
                (drawbars, pool, profile.row.clone())
            }
            None => {
                let default_row = params
                    .iter()
                    .filter(|p| p.kind == ParamKind::Continuous)
                    .take(fallback)
                    .map(|p| format!("param:{}", p.id))
                    .collect();
                (Vec::new(), params.iter().map(param_control).collect(), default_row)
            }
        };
        // Parameters the user picked through "More parameters…".
        if let Some(user) = user {
            for id in user.extra.iter().chain(&user.row) {
                if !pool.iter().any(|control| control.id == *id)
                    && let Some(param) = param_by_control_id(params, id)
                {
                    pool.push(param_control(param));
                }
            }
        }
        let ids: Vec<&str> = pool.iter().map(|control| control.id.as_str()).collect();
        let max = if drawbars.is_empty() { most } else { MAX_CONTROLS_WITH_DRAWBARS };
        let row = resolve_row(&ids, user.map(|user| user.row.as_slice()), &default_row, max);
        Panel { drawbars, pool, row, max }
    }

    /// Shows pool control `control_id` on this plugin's panel, and saves that
    /// as the user's choice (per plugin): after the shown ones, or — when the
    /// panel is full — instead of the one at `replace` (an index into the shown
    /// ones). Nothing if it's shown already. `control_id` may also be a
    /// parameter's [`param_control`] id that isn't in the pool ("More
    /// parameters…"): it's added to this plugin's pool for good (until
    /// [`Profiles::reset_row`]).
    pub fn add_control(
        &mut self,
        kind: PanelKind,
        plugin_id: &str,
        params: &[ParamInfo],
        control_id: &str,
        replace: usize,
    ) -> Result<()> {
        let panel = self.panel_of(kind, plugin_id, params);
        let in_pool = panel.pool.iter().any(|control| control.id == control_id);
        if !in_pool && param_by_control_id(params, control_id).is_none() {
            bail!("no control '{control_id}' for {plugin_id}");
        }
        let mut row = row_ids(&panel);
        add(&mut row, control_id, panel.max, replace)?;
        let user = self.rows.entry(plugin_id.to_string()).or_default();
        user.row = row;
        if !in_pool && !user.extra.iter().any(|id| id == control_id) {
            user.extra.push(control_id.to_string());
        }
        self.save_rows()
    }

    /// Takes control `control_id` off this plugin's panel (the ones after it
    /// move up), and saves that as the user's choice.
    pub fn remove_control(&mut self, kind: PanelKind, plugin_id: &str, params: &[ParamInfo], control_id: &str) -> Result<()> {
        let mut row = row_ids(&self.panel_of(kind, plugin_id, params));
        row.retain(|id| id != control_id);
        self.rows.entry(plugin_id.to_string()).or_default().row = row;
        self.save_rows()
    }

    /// Forgets the user's row for this plugin: back to the profile's default.
    pub fn reset_row(&mut self, plugin_id: &str) -> Result<()> {
        if self.rows.remove(plugin_id).is_some() {
            self.save_rows()?;
        }
        Ok(())
    }

    fn save_rows(&self) -> Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
        let path = dir.join(ROWS_FILE);
        let temporary = dir.join(format!("{ROWS_FILE}.tmp"));
        std::fs::write(&temporary, serde_json::to_string_pretty(&self.rows)?)
            .with_context(|| format!("couldn't write {}", temporary.display()))?;
        std::fs::rename(&temporary, &path).with_context(|| format!("couldn't write {}", path.display()))
    }
}

/// Every user profile in `dir` (all `*.json` but [`ROWS_FILE`]); unreadable
/// ones are logged and skipped. Sorted by file name, so if two name the same
/// plugin the last one wins, predictably.
/// Adds `profile` under each plugin id it names.
fn insert(profiles: &mut HashMap<String, Rc<ProfileFile>>, profile: ProfileFile) {
    let profile = Rc::new(profile);
    for plugin in &profile.plugins {
        profiles.insert(plugin.clone(), profile.clone());
    }
}

fn read_user_profiles(dir: &Path) -> Vec<(PathBuf, ProfileFile)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && path.file_name().is_some_and(|name| name != ROWS_FILE)
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let parsed = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|json| Ok(serde_json::from_str::<ProfileFile>(&json)?));
            match parsed {
                Ok(profile) => Some((path, profile)),
                Err(err) => {
                    log::warn!("ignoring panel profile {}: {err}", path.display());
                    None
                }
            }
        })
        .collect()
}

/// The parameter a [`param_control`] id (`param:<ParamInfo::id>`) stands for.
fn param_by_control_id<'a>(params: &'a [ParamInfo], control_id: &str) -> Option<&'a ParamInfo> {
    let id: u32 = control_id.strip_prefix("param:")?.parse().ok()?;
    params.iter().find(|param| param.id == id)
}

/// `panel`'s shown controls as ids.
fn row_ids(panel: &Panel) -> Vec<String> {
    panel.row.iter().map(|&index| panel.pool[index].id.clone()).collect()
}

/// The shown controls as pool indices, at most `max`: the user's ids if they
/// chose, else the default ids, leaving out ones not in the pool (`pool_ids`)
/// and repeats. A default that comes out short of what it names (controls
/// whose parameter is gone) is filled from the pool; a user's choice isn't.
fn resolve_row(pool_ids: &[&str], user: Option<&[String]>, default: &[String], max: usize) -> Vec<usize> {
    let mut row: Vec<usize> = Vec::with_capacity(max);
    let add = |id: &str, row: &mut Vec<usize>| {
        if row.len() < max
            && let Some(index) = pool_ids.iter().position(|pool_id| *pool_id == id)
            && !row.contains(&index)
        {
            row.push(index);
        }
    };
    let chosen = user.unwrap_or(default);
    for id in chosen {
        add(id, &mut row);
    }
    let wanted = default.len().min(max);
    if user.is_none() && row.len() < wanted {
        for id in pool_ids {
            if row.len() == wanted {
                break;
            }
            add(id, &mut row);
        }
    }
    row
}

/// Adds `control_id` to `row` (shown control ids, at most `max`): at the end,
/// or instead of `row[replace]` when it's full. Nothing if it's there already.
fn add(row: &mut Vec<String>, control_id: &str, max: usize, replace: usize) -> Result<()> {
    if row.iter().any(|id| id == control_id) {
        return Ok(());
    }
    if row.len() < max {
        row.push(control_id.to_string());
    } else if let Some(id) = row.get_mut(replace) {
        *id = control_id.to_string();
    } else {
        bail!("the panel is full");
    }
    Ok(())
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
        overrides: Overrides::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(widget: Widget, choices: &[(&str, f32)]) -> Control {
        Control {
            id: "c".into(),
            name: String::new(),
            label: String::new(),
            text: String::new(),
            widget,
            target: Target::Setbfree("f".into()),
            colour: None,
            choices: choices.iter().map(|(label, value)| Choice { label: label.to_string(), value: *value }).collect(),
            overrides: Overrides::default(),
        }
    }

    #[test]
    fn shipped_profiles_parse() {
        for (name, json) in SHIPPED {
            if let Err(err) = serde_json::from_str::<ProfileFile>(json) {
                panic!("{name}: {err}");
            }
        }
    }

    #[test]
    fn drawbars_are_inverted_and_round_trip() {
        assert_eq!(drawbar_level(0), 8);
        assert_eq!(drawbar_level(127), 0);
        assert_eq!(drawbar_level(32), 6);
        for level in 0..=8 {
            assert_eq!(drawbar_level(drawbar_value(level)), level);
        }
        let drawbar = control(Widget::Drawbar, &[]);
        assert_eq!(drawbar.setbfree_cell(0), CellValue::Index(8));
        assert_eq!(drawbar.setbfree_write(CellValue::Index(8)), 0);
        assert_eq!(drawbar.setbfree_write(CellValue::Index(0)), 127);
    }

    #[test]
    fn setbfree_switches_levers_and_knobs() {
        let switch = control(Widget::Switch, &[]);
        assert_eq!(switch.setbfree_cell(63), CellValue::On(false));
        assert_eq!(switch.setbfree_cell(64), CellValue::On(true));
        assert_eq!(switch.setbfree_write(CellValue::On(true)), 127);
        assert_eq!(switch.setbfree_write(CellValue::On(false)), 0);

        let leslie = control(Widget::Lever, &[("Slow", 0.0), ("Stop", 64.0), ("Fast", 127.0)]);
        assert_eq!(leslie.setbfree_cell(127), CellValue::Index(2));
        assert_eq!(leslie.setbfree_cell(60), CellValue::Index(1));
        assert_eq!(leslie.setbfree_write(CellValue::Index(1)), 64);

        let knob = control(Widget::Knob, &[]);
        assert_eq!(knob.setbfree_write(CellValue::Position(1.0)), 127);
        assert_eq!(knob.setbfree_cell(127), CellValue::Position(1.0));
    }

    #[test]
    fn param_values_follow_overrides() {
        let param = ParamInfo::continuous(3, "Cutoff".into(), 0.5, 0.0, 1.0);
        let mut knob = control(Widget::Knob, &[]);
        knob.target = Target::Param(3);
        knob.overrides = Overrides { min: Some(0.2), max: Some(0.6), invert: true, ..Overrides::default() };
        let covered = knob.overrides.apply(&param);
        assert_eq!(knob.param_cell(&covered, 0.6), CellValue::Position(0.0));
        assert!((knob.param_value(&covered, CellValue::Position(0.75)) - 0.3).abs() < 1e-6);

        // A JUCE "switch" reported continuous, given steps.
        let mut switch = control(Widget::Switch, &[]);
        switch.overrides.steps = Some(1);
        let covered = switch.overrides.apply(&param);
        assert_eq!(switch.param_cell(&covered, 0.7), CellValue::On(true));
        assert_eq!(switch.param_value(&covered, CellValue::On(false)), 0.0);

        // Bare choice labels spread across the range.
        let spec: ControlSpec = serde_json::from_str(
            r#"{ "id": "t", "target": { "param": "3" }, "widget": "lever", "choices": ["A", "B", "C"] }"#,
        )
        .unwrap();
        let lever = spec.resolve(std::slice::from_ref(&param), Widget::Knob).unwrap();
        assert_eq!(lever.param_value(&param, CellValue::Index(1)), 0.5);
        assert_eq!(lever.param_cell(&param, 0.9), CellValue::Index(2));
    }

    #[test]
    fn rows_fill_gaps_and_add() {
        let pool = ["a", "b", "c", "d", "e", "f"];
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        // A default naming a control the plugin lacks is filled from the pool.
        assert_eq!(resolve_row(&pool, None, &ids(&["a", "x", "c", "d", "e"]), 10), [0, 2, 3, 4, 1]);
        // At most `max`.
        assert_eq!(resolve_row(&pool, None, &ids(&["a", "b", "c"]), 2), [0, 1]);
        // The user's choice wins, gone controls just left out.
        assert_eq!(resolve_row(&pool, Some(&ids(&["f", "x", "c", "f"])), &ids(&["a"]), 10), [5, 2]);
        // A short fallback default (few continuous parameters) stays short.
        assert_eq!(resolve_row(&pool, None, &ids(&["b", "c"]), 10), [1, 2]);

        let mut row = ids(&["a", "b"]);
        add(&mut row, "c", 3, 0).unwrap();
        assert_eq!(row, ids(&["a", "b", "c"]));
        add(&mut row, "b", 3, 0).unwrap();
        assert_eq!(row, ids(&["a", "b", "c"]));
        // Full: replaces.
        add(&mut row, "d", 3, 1).unwrap();
        assert_eq!(row, ids(&["a", "d", "c"]));
        assert!(add(&mut row, "e", 3, 3).is_err());
    }

    #[test]
    fn add_control_keeps_more_parameters_in_the_pool() {
        let params: Vec<ParamInfo> =
            (0..8).map(|id| ParamInfo::continuous(id, format!("P{id}"), 0.0, 0.0, 1.0)).collect();
        let plugin = "http://example.org/plugin";
        let mut profiles = Profiles::load_from(None);
        let profile: ProfileFile = serde_json::from_str(&format!(
            r#"{{ "plugins": ["{plugin}"], "row": ["a", "b"], "controls": [
                {{ "id": "a", "target": {{ "param": "0" }} }}, {{ "id": "b", "target": {{ "param": "1" }} }},
                {{ "id": "gone", "target": {{ "param": "missing" }} }} ] }}"#
        ))
        .unwrap();
        insert(&mut profiles.profiles, profile);
        let row = |profiles: &Profiles| {
            let panel = profiles.panel(plugin, &params);
            panel.row.iter().map(|&index| panel.pool[index].id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(profiles.panel(plugin, &params).pool.len(), 2);

        profiles.add_control(PanelKind::Instrument, plugin, &params, "param:7", 0).unwrap();
        assert_eq!(row(&profiles), ["a", "b", "param:7"]);
        // Taken off again, it stays in the pool.
        profiles.remove_control(PanelKind::Instrument, plugin, &params, "param:7").unwrap();
        profiles.remove_control(PanelKind::Instrument, plugin, &params, "a").unwrap();
        assert_eq!(row(&profiles), ["b"]);
        assert_eq!(profiles.panel(plugin, &params).pool.len(), 3);

        assert!(profiles.add_control(PanelKind::Instrument, plugin, &params, "param:99", 0).is_err());
        profiles.reset_row(plugin).unwrap();
        assert_eq!(row(&profiles), ["a", "b"]);
        assert_eq!(profiles.panel(plugin, &params).pool.len(), 2);
    }

    #[test]
    fn effect_panels_default_to_two_without_drawbars() {
        let mut params: Vec<ParamInfo> =
            (0..5).map(|id| ParamInfo::continuous(id, format!("P{id}"), 0.0, 0.0, 1.0)).collect();
        params[0].kind = ParamKind::Toggle;
        let mut profiles = Profiles::load_from(None);
        let row = |panel: &Panel| panel.row.iter().map(|&index| panel.pool[index].id.clone()).collect::<Vec<_>>();
        // No profile: the first two continuous parameters.
        let panel = profiles.effect_panel("x", &params);
        assert_eq!((row(&panel), panel.max), (vec!["param:1".to_string(), "param:2".to_string()], MAX_EFFECT_CONTROLS));
        // A profile's row; its drawbars left out.
        let profile: ProfileFile = serde_json::from_str(
            r#"{ "plugins": ["p"], "fixed": "drawbars", "row": ["c", "a", "b"],
                "drawbars": [{ "id": "d", "target": { "param": "4" } }],
                "controls": [{ "id": "a", "target": { "param": "0" } }, { "id": "b", "target": { "param": "1" } },
                    { "id": "c", "target": { "param": "2" } }] }"#,
        )
        .unwrap();
        insert(&mut profiles.profiles, profile);
        let panel = profiles.effect_panel("p", &params);
        assert!(panel.drawbars.is_empty());
        assert_eq!(row(&panel), ["c", "a", "b"]);
        // Added after them, up to the most; then instead of the one given.
        profiles.add_control(PanelKind::Effect, "p", &params, "param:3", 0).unwrap();
        profiles.add_control(PanelKind::Effect, "p", &params, "param:4", 1).unwrap();
        assert_eq!(row(&profiles.effect_panel("p", &params)), ["c", "param:4", "b", "param:3"]);
    }
}
