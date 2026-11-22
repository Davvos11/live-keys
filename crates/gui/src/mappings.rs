//! MIDI learn on the GUI side: which surface controls drive what, as kept (with
//! the scene — per plugin, in its slot preset's `slot.json` — or in every
//! scene, in the config), resolved against what's loaded into a setup for the
//! engine's mapping thread, and learn mode: the selected control, its targets
//! pane, and which on-screen controls are targets. See
//! `docs/design/midi-learn.md`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use live_keys_engine::{
    ControlSetup, Engine, Link, MASTER_SLOT, MAX_BPM, MIN_BPM, MapperSetup, Mode, Output, PluginKey, SLOT_COUNT,
    Takeover, TargetSpec, ToValue,
};
use live_keys_surfaces::{ControlKind, Message, Surface};
use serde::{Deserialize, Serialize};
use slint::{ModelRc, VecModel};

use crate::config::{self, AppConfig};
use crate::profiles;
use crate::{LearnControl, LearnLevel, LearnTargetRow, sync_model};

/// A control's default glide time.
const DEFAULT_GLIDE_MS: u32 = 200;

/// A surface control, as mappings name it: by its surface's id and its label,
/// which stay put when the surface file is edited.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ControlRef {
    pub surface: String,
    pub control: String,
}

impl ControlRef {
    /// How [`config::SurfacesConfig::controls`] keys it.
    pub fn key(&self) -> String {
        format!("{}/{}", self.surface, self.control)
    }
}

/// What a control does to one target (see "Modes" in the design notes), in
/// positions 0..1 along the target's control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum MappingMode {
    /// Faders, knobs, pads: the target follows the control between `from`
    /// and `to`.
    Direct { from: f32, to: f32 },
    /// As direct, the other way round: the control's low end is `to`.
    Mirrored { from: f32, to: f32 },
    /// Buttons, pads: each press steps to the next level.
    Toggle { levels: Vec<f32> },
    /// Buttons, pads: `value` while held (times the pad's velocity if
    /// `velocity`).
    Momentary {
        value: f32,
        #[serde(default)]
        velocity: bool,
    },
}

/// The UI's mode numbers (2 and 5, modulating, come later).
const DIRECT: i32 = 0;
const MIRRORED: i32 = 1;
const TOGGLE: i32 = 3;
const MOMENTARY: i32 = 4;

impl MappingMode {
    /// A new target's mode for a control of `kind`; `steps` are the target's
    /// steps' positions (empty for a continuous one).
    fn default_for(kind: ControlKind, steps: &[f32]) -> Self {
        match kind {
            ControlKind::Fader | ControlKind::Knob => Self::Direct { from: 0.0, to: 1.0 },
            ControlKind::Button => Self::Toggle { levels: default_levels(steps) },
            ControlKind::Pad => Self::Momentary { value: 1.0, velocity: false },
        }
    }

    fn index(&self) -> i32 {
        match self {
            Self::Direct { .. } => DIRECT,
            Self::Mirrored { .. } => MIRRORED,
            Self::Toggle { .. } => TOGGLE,
            Self::Momentary { .. } => MOMENTARY,
        }
    }

    /// Mode `index`, keeping what it can of this one.
    fn with_index(&self, index: i32, steps: &[f32]) -> Option<Self> {
        let (from, to) = match self {
            Self::Direct { from, to } | Self::Mirrored { from, to } => (*from, *to),
            _ => (0.0, 1.0),
        };
        Some(match index {
            DIRECT => Self::Direct { from, to },
            MIRRORED => Self::Mirrored { from, to },
            TOGGLE => Self::Toggle { levels: default_levels(steps) },
            MOMENTARY => Self::Momentary { value: 1.0, velocity: false },
            _ => return None,
        })
    }

    fn engine(&self) -> Mode {
        match self {
            Self::Direct { from, to } => Mode::Range { from: *from, to: *to },
            Self::Mirrored { from, to } => Mode::Range { from: *to, to: *from },
            Self::Toggle { levels } => Mode::Toggle { levels: levels.clone() },
            Self::Momentary { value, velocity } => Mode::Momentary { value: *value, velocity: *velocity },
        }
    }

    /// The row's chip: "Direct", "Mirrored 20–80 %", "Toggle · 3 levels".
    fn chip(&self) -> String {
        let percent = |value: f32| (value * 100.0).round();
        let range = |name: &str, from: f32, to: f32| {
            if from == 0.0 && to == 1.0 {
                name.to_string()
            } else {
                format!("{name} {}–{} %", percent(from), percent(to))
            }
        };
        match self {
            Self::Direct { from, to } => range("Direct", *from, *to),
            Self::Mirrored { from, to } => range("Mirrored", *from, *to),
            Self::Toggle { levels } => format!("Toggle · {} levels", levels.len()),
            Self::Momentary { value, velocity } => {
                format!("Momentary {} %{}", percent(*value), if *velocity { " · velocity" } else { "" })
            }
        }
    }
}

/// A toggle's levels for a target with `steps`: all of them (a lever's
/// options), or off and on.
fn default_levels(steps: &[f32]) -> Vec<f32> {
    if steps.len() >= 2 { steps.to_vec() } else { vec![0.0, 1.0] }
}

/// Which modes a control of `kind` offers.
fn allows(kind: ControlKind, mode: i32) -> bool {
    match kind {
        ControlKind::Fader | ControlKind::Knob => matches!(mode, DIRECT | MIRRORED),
        ControlKind::Button => matches!(mode, TOGGLE | MOMENTARY),
        ControlKind::Pad => matches!(mode, DIRECT | MIRRORED | TOGGLE | MOMENTARY),
    }
}

/// Something of a plugin a control can drive.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginTarget {
    /// A parameter, by its `ParamInfo::symbol`.
    Param { symbol: String },
    /// A setBfree function ("upper.drawbar16").
    Setbfree { function: String },
}

/// A target of a mapping kept in every scene.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RackTarget {
    /// The first plugin `plugin` (a `PluginInfo::id`) in slot `slot`, whenever
    /// one is there.
    Plugin { slot: usize, plugin: String, target: PluginTarget },
    /// Whatever slot `slot`'s panel shows at row position `index`.
    Panel { slot: usize, index: usize },
    /// A slot's volume, mute or transposition; slot `MASTER_SLOT` is the
    /// master's.
    Volume { slot: usize },
    Mute { slot: usize },
    Transpose { slot: usize },
    MetronomeVolume,
    MetronomePlay,
    Tempo,
}

/// A mapping kept with the scene, as one plugin's (`slot.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneMapping {
    pub control: ControlRef,
    pub target: PluginTarget,
    #[serde(flatten)]
    pub mode: MappingMode,
}

/// A mapping kept in every scene (the config).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalMapping {
    pub control: ControlRef,
    pub target: RackTarget,
    #[serde(flatten)]
    pub mode: MappingMode,
}

/// How a fader or knob takes over a target that's elsewhere, as kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TakeoverSetting {
    Jump,
    #[default]
    Glide,
    PickUp,
}

impl TakeoverSetting {
    const ALL: [Self; 3] = [Self::Jump, Self::Glide, Self::PickUp];

    fn engine(self) -> Takeover {
        match self {
            Self::Jump => Takeover::Jump,
            Self::Glide => Takeover::Glide,
            Self::PickUp => Takeover::PickUp,
        }
    }
}

/// A surface control's own settings, whatever it drives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlSettings {
    pub takeover: TakeoverSetting,
    pub glide_ms: u32,
    /// Its messages also reach the slots.
    pub pass_through: bool,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self { takeover: TakeoverSetting::default(), glide_ms: DEFAULT_GLIDE_MS, pass_through: false }
    }
}

/// What a mapping drives, resolved against what's loaded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Concrete {
    Plugin { key: PluginKey, target: PluginTarget },
    Volume(usize),
    Mute(usize),
    Transpose(usize),
    MetronomeVolume,
    MetronomePlay,
    Tempo,
}

/// What the rest of the GUI knows that mappings need.
pub struct Ctx<'a> {
    pub engine: &'a Engine,
    pub config: &'a AppConfig,
    /// Per slot: its instrument's key and panel, if it has one.
    pub panels: Vec<Option<(PluginKey, &'a profiles::Panel)>>,
    pub metronome_playing: bool,
}

impl Ctx<'_> {
    fn slot_of(&self, key: PluginKey) -> Option<usize> {
        (0..=MASTER_SLOT).find(|&slot| self.engine.slot(slot).is_some_and(|chain| chain.plugins().any(|m| m.key == key)))
    }

    /// The panel control behind `slot`'s row position `index`.
    fn panel_cell(&self, slot: usize, index: usize) -> Option<(PluginKey, &profiles::Control)> {
        let (key, panel) = self.panels.get(slot).copied().flatten()?;
        Some((key, panel.pool.get(*panel.row.get(index)?)?))
    }

    /// A plugin target of the control `control` drives.
    fn plugin_target(&self, key: PluginKey, control: &profiles::Control) -> Option<PluginTarget> {
        Some(match &control.target {
            profiles::Target::Param(id) => {
                let param = self.engine.plugin(key)?.params.iter().find(|param| param.id == *id)?;
                PluginTarget::Param { symbol: param.symbol.clone() }
            }
            profiles::Target::Setbfree(function) => PluginTarget::Setbfree { function: function.clone() },
        })
    }

    /// The control that shows `target` of plugin `key`: its panel's, if it's
    /// the instrument and its profile has one, else a plain one.
    fn control_for(&self, key: PluginKey, target: &PluginTarget) -> Option<profiles::Control> {
        let meta = self.engine.plugin(key)?;
        let wanted = match target {
            PluginTarget::Param { symbol } => {
                profiles::Target::Param(meta.params.iter().find(|param| param.symbol == *symbol)?.id)
            }
            PluginTarget::Setbfree { function } => profiles::Target::Setbfree(function.clone()),
        };
        let panel = self.panels.iter().flatten().find(|(panel_key, _)| *panel_key == key).map(|(_, panel)| *panel);
        if let Some(control) =
            panel.and_then(|panel| panel.drawbars.iter().chain(&panel.pool).find(|control| control.target == wanted))
        {
            return Some(control.clone());
        }
        match wanted {
            profiles::Target::Param(id) => {
                Some(profiles::param_control(meta.params.iter().find(|param| param.id == id)?))
            }
            profiles::Target::Setbfree(function) => Some(profiles::Control {
                id: function.clone(),
                name: function.clone(),
                label: function.clone(),
                text: String::new(),
                widget: profiles::Widget::Knob,
                target: profiles::Target::Setbfree(function),
                colour: None,
                choices: Vec::new(),
                overrides: profiles::Overrides::default(),
            }),
        }
    }

    fn resolve(&self, target: &RackTarget) -> Option<Concrete> {
        Some(match target {
            RackTarget::Plugin { slot, plugin, target } => {
                let meta = self.engine.slot(*slot)?.plugins().find(|meta| meta.id == *plugin)?;
                Concrete::Plugin { key: meta.key, target: target.clone() }
            }
            RackTarget::Panel { slot, index } => {
                let (key, control) = self.panel_cell(*slot, *index)?;
                Concrete::Plugin { key, target: self.plugin_target(key, control)? }
            }
            RackTarget::Volume { slot } => Concrete::Volume(*slot),
            RackTarget::Mute { slot } => Concrete::Mute(*slot),
            RackTarget::Transpose { slot } => Concrete::Transpose(*slot),
            RackTarget::MetronomeVolume => Concrete::MetronomeVolume,
            RackTarget::MetronomePlay => Concrete::MetronomePlay,
            RackTarget::Tempo => Concrete::Tempo,
        })
    }
}

/// A slot's volume in dB at volume-knob `position`.
pub fn volume_db(position: f32) -> f32 {
    config::MIN_VOLUME_DB + position.clamp(0.0, 1.0) * (config::MAX_VOLUME_DB - config::MIN_VOLUME_DB)
}

fn volume_position(db: f32) -> f32 {
    ((db - config::MIN_VOLUME_DB) / (config::MAX_VOLUME_DB - config::MIN_VOLUME_DB)).clamp(0.0, 1.0)
}

/// A transposition in semitones at `position`.
pub fn transpose(position: f32) -> i32 {
    let max = config::MAX_TRANSPOSE as f32;
    (-max + position.clamp(0.0, 1.0) * 2.0 * max).round() as i32
}

/// A tempo at `position`.
pub fn tempo(position: f32) -> f32 {
    (MIN_BPM + position.clamp(0.0, 1.0) * (MAX_BPM - MIN_BPM)).round()
}

/// A resolved target, with the control showing it if it's a plugin's.
pub struct Resolved {
    pub concrete: Concrete,
    control: Option<profiles::Control>,
}

impl Resolved {
    fn new(ctx: &Ctx, concrete: Concrete) -> Option<Self> {
        let control = match &concrete {
            Concrete::Plugin { key, target } => Some(ctx.control_for(*key, target)?),
            _ => None,
        };
        Some(Self { concrete, control })
    }

    /// Where it is now, 0..1.
    pub fn position(&self, ctx: &Ctx) -> f32 {
        let config = ctx.config;
        let slot_config = |slot: usize| config.slots.get(slot);
        match &self.concrete {
            Concrete::Plugin { key, .. } => self.control.as_ref().map_or(0.0, |control| control.position(ctx.engine, *key)),
            Concrete::Volume(MASTER_SLOT) => volume_position(config.master_volume_db),
            Concrete::Volume(slot) => slot_config(*slot).map_or(0.0, |slot| volume_position(slot.volume_db)),
            Concrete::Mute(MASTER_SLOT) => f32::from(u8::from(config.master_muted)),
            Concrete::Mute(slot) => slot_config(*slot).map_or(0.0, |slot| f32::from(u8::from(slot.muted))),
            Concrete::Transpose(slot) => {
                let semitones = if *slot == MASTER_SLOT {
                    config.master_transpose
                } else {
                    slot_config(*slot).map_or(0, |slot| slot.transpose)
                };
                let max = config::MAX_TRANSPOSE as f32;
                (f32::from(semitones) + max) / (2.0 * max)
            }
            Concrete::MetronomeVolume => config.metronome.volume,
            Concrete::MetronomePlay => f32::from(u8::from(ctx.metronome_playing)),
            Concrete::Tempo => (f32::from(config.metronome.bpm) - MIN_BPM) / (MAX_BPM - MIN_BPM),
        }
    }

    fn output(&self, ctx: &Ctx) -> Option<(Output, ToValue)> {
        let identity: ToValue = Arc::new(|position| position);
        Some(match &self.concrete {
            Concrete::Plugin { key, target } => {
                let to_value = self.control.as_ref()?.to_value(ctx.engine, *key)?;
                let output = match (target, &self.control.as_ref()?.target) {
                    (PluginTarget::Param { .. }, profiles::Target::Param(id)) => Output::Param { key: *key, param: *id },
                    (PluginTarget::Setbfree { function }, _) => Output::Setbfree { key: *key, function: function.as_str().into() },
                    _ => return None,
                };
                (output, to_value)
            }
            Concrete::Volume(slot) => {
                (Output::Volume { slot: *slot }, Arc::new(|position| config::gain(volume_db(position))) as ToValue)
            }
            _ => (Output::Gui, identity),
        })
    }

    /// Its steps' positions (empty if it's continuous).
    fn steps(&self) -> Vec<f32> {
        match &self.concrete {
            Concrete::Plugin { .. } => self.control.as_ref().map(profiles::Control::steps).unwrap_or_default(),
            Concrete::Mute(_) | Concrete::MetronomePlay => vec![0.0, 1.0],
            _ => Vec::new(),
        }
    }

    /// Its value at `position`, as text.
    fn text(&self, ctx: &Ctx, position: f32) -> String {
        let on_off = |on: bool| if on { "On" } else { "Off" }.to_string();
        match &self.concrete {
            Concrete::Plugin { key, .. } => {
                self.control.as_ref().map(|control| control.text_at(ctx.engine, *key, position)).unwrap_or_default()
            }
            Concrete::Volume(_) => {
                let db = volume_db(position);
                if db <= config::MIN_VOLUME_DB { "muted".into() } else { format!("{:.1} dB", db) }
            }
            Concrete::Mute(_) => if position >= 0.5 { "Muted" } else { "On" }.into(),
            Concrete::Transpose(_) => format!("{:+}", transpose(position)),
            Concrete::MetronomeVolume => format!("{} %", (position * 100.0).round()),
            Concrete::MetronomePlay => on_off(position >= 0.5),
            Concrete::Tempo => format!("{} BPM", tempo(position)),
        }
    }

    fn name(&self) -> String {
        match &self.concrete {
            Concrete::Plugin { target, .. } => match (&self.control, target) {
                // The profile's name for it ("Decay"), else the plugin's.
                (Some(control), _) => control.name.clone(),
                (None, PluginTarget::Param { symbol }) => symbol.clone(),
                (None, PluginTarget::Setbfree { function }) => function.clone(),
            },
            Concrete::Volume(_) => "Volume".into(),
            Concrete::Mute(_) => "Mute".into(),
            Concrete::Transpose(_) => "Transpose".into(),
            Concrete::MetronomeVolume => "Click volume".into(),
            Concrete::MetronomePlay => "Play/stop".into(),
            Concrete::Tempo => "Tempo".into(),
        }
    }

    fn place(&self, ctx: &Ctx) -> String {
        match &self.concrete {
            Concrete::Plugin { key, .. } => {
                let plugin = ctx.engine.plugin(*key).map(|meta| meta.name.clone()).unwrap_or_default();
                format!("{} · {plugin}", slot_name(ctx.slot_of(*key).unwrap_or(0)))
            }
            Concrete::Volume(slot) | Concrete::Mute(slot) | Concrete::Transpose(slot) => slot_name(*slot),
            Concrete::MetronomeVolume | Concrete::MetronomePlay | Concrete::Tempo => "Metronome".into(),
        }
    }
}

fn slot_name(slot: usize) -> String {
    if slot == MASTER_SLOT { "Master".into() } else { format!("Slot {}", slot + 1) }
}

/// One mapping, wherever it's kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    /// The `index`th of plugin `key`'s scene mappings.
    Scene { key: PluginKey, index: usize },
    /// The `index`th of the config's.
    Global { index: usize },
}

/// What clicking a learn-mode target (an on-screen control) would map.
#[derive(Clone, Debug, PartialEq)]
enum Candidate {
    /// A plugin's parameter or setBfree function; `panel`: the panel row
    /// position it's shown at (slot, index), if it is.
    Plugin { key: PluginKey, target: PluginTarget, panel: Option<(usize, usize)> },
    Rack(RackTarget),
}

/// Where a new mapping is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    /// With the scene: the plugin's.
    Scene,
    /// In every scene, as this target.
    Global,
    /// In every scene, as the panel position the target is at.
    Panel,
}

impl Keep {
    fn of(global: bool, panel: bool) -> Self {
        if panel {
            Self::Panel
        } else if global {
            Self::Global
        } else {
            Self::Scene
        }
    }
}

/// The selected surface control.
#[derive(Clone)]
struct Selected {
    device: String,
    control: usize,
    surface: Arc<Surface>,
    reference: ControlRef,
}

impl Selected {
    fn kind(&self) -> ControlKind {
        self.surface.controls[self.control].kind
    }
}

/// The right-click popup's target.
#[derive(Clone)]
enum Popup {
    Assigned(Entry),
    /// Not assigned yet: its settings so far, kept with the scene or not.
    Draft { candidate: Candidate, mode: MappingMode, keep: Keep },
}

/// The kinds of learn-mode targets, as the UI numbers them.
pub mod kinds {
    pub const CELL: i32 = 0;
    pub const DRAWBAR: i32 = 1;
    pub const VOLUME: i32 = 2;
    pub const MUTE: i32 = 3;
    pub const TRANSPOSE: i32 = 4;
    pub const METRONOME_VOLUME: i32 = 5;
    pub const METRONOME_PLAY: i32 = 6;
    pub const TEMPO: i32 = 7;
}

/// Learn-mode states of the on-screen targets: 0 not a target, 1 a possible
/// one, 2 one of the selected control's.
#[derive(Default, PartialEq)]
pub struct LearnFlags {
    /// Per slot, per row cell and per drawbar.
    pub cells: Vec<Vec<i32>>,
    pub drawbars: Vec<Vec<i32>>,
    /// Per slot and the master (`MASTER_SLOT`).
    pub volume: Vec<i32>,
    pub mute: Vec<i32>,
    pub transpose: Vec<i32>,
    pub metronome_volume: i32,
    pub metronome_play: i32,
    pub tempo: i32,
}

/// What [`MappingsUi::install`] takes on with a setup.
pub struct Installed {
    targets: Vec<SetupTarget>,
    mapped: HashSet<(String, usize)>,
    signature: Signature,
}

/// What a setup does: per mapped control (device, control, its settings),
/// its targets and modes. A setup that does the same as the last isn't
/// handed over again, so the mapping thread keeps its takeover state.
type Signature = Vec<(String, usize, ControlSettings, Vec<(Concrete, MappingMode)>)>;

/// The current setup's targets, by `TargetId`.
struct SetupTarget {
    resolved: Resolved,
    /// Where it was when last seen (here or reported by the mapping thread).
    seen: f32,
}

/// Every mapping and learn mode.
pub struct MappingsUi {
    /// Scene mappings by plugin; saved with its slot preset.
    pub scene: RefCell<HashMap<PluginKey, Vec<SceneMapping>>>,
    /// The mappings kept in every scene and the controls' settings: the
    /// config's, which the caller writes back when a change says so
    /// ([`MappingsUi::store`]).
    global: RefCell<Vec<GlobalMapping>>,
    controls: RefCell<BTreeMap<String, ControlSettings>>,
    learning: Cell<bool>,
    selected: RefCell<Option<Selected>>,
    /// The targets pane's rows, as entries.
    entries: RefCell<Vec<Entry>>,
    expanded: Cell<Option<usize>>,
    popup: RefCell<Option<Popup>>,
    generation: Cell<u64>,
    targets: RefCell<Vec<SetupTarget>>,
    signature: RefCell<Option<Signature>>,
    /// The setup's controls, as (device, control), for the panel's "mapped".
    mapped: RefCell<HashSet<(String, usize)>>,
    /// The setup needs building again (mappings, surfaces, plugins changed).
    pub setup_dirty: Cell<bool>,
    /// The targets pane and learn flags need refreshing.
    pub view_dirty: Cell<bool>,
    /// `AppState.learn-rows`.
    pub rows: Rc<VecModel<LearnTargetRow>>,
}

impl MappingsUi {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            scene: RefCell::default(),
            global: RefCell::new(config.surfaces.mappings.clone()),
            controls: RefCell::new(config.surfaces.controls.clone()),
            learning: Cell::new(false),
            selected: RefCell::default(),
            entries: RefCell::default(),
            expanded: Cell::new(None),
            popup: RefCell::default(),
            generation: Cell::new(0),
            targets: RefCell::default(),
            signature: RefCell::default(),
            mapped: RefCell::default(),
            setup_dirty: Cell::new(true),
            view_dirty: Cell::new(true),
            rows: Rc::new(VecModel::default()),
        }
    }

    /// Writes the mappings kept in every scene and the controls' settings
    /// into `config`, for saving.
    pub fn store(&self, config: &mut AppConfig) {
        config.surfaces.mappings = self.global.borrow().clone();
        config.surfaces.controls = self.controls.borrow().clone();
    }

    pub fn learning(&self) -> bool {
        self.learning.get()
    }

    pub fn set_learning(&self, learning: bool) {
        self.learning.set(learning);
        if !learning {
            *self.selected.borrow_mut() = None;
            *self.popup.borrow_mut() = None;
            self.expanded.set(None);
        }
        self.view_dirty.set(true);
    }

    /// Selects `control` of `device`'s `surface` (and starts learning).
    pub fn select(&self, device: &str, control: usize, surface: Arc<Surface>) {
        let Some(label) = surface.controls.get(control).map(|control| control.label.clone()) else { return };
        let same = self.selected.borrow().as_ref().is_some_and(|s| s.device == device && s.control == control);
        self.learning.set(true);
        if same {
            return;
        }
        let reference = ControlRef { surface: surface.id.clone(), control: label };
        *self.selected.borrow_mut() = Some(Selected { device: device.to_string(), control, surface, reference });
        self.expanded.set(None);
        self.view_dirty.set(true);
    }

    /// The selected control, as (device, control).
    pub fn selected(&self) -> Option<(String, usize)> {
        self.selected.borrow().as_ref().map(|s| (s.device.clone(), s.control))
    }

    /// The surface controls that have mappings, as (device, control).
    pub fn mapped(&self) -> HashSet<(String, usize)> {
        self.mapped.borrow().clone()
    }

    /// Forgets the scene mappings of plugins no longer loaded.
    pub fn forget_missing(&self, engine: &Engine) {
        self.scene.borrow_mut().retain(|key, _| engine.plugin(*key).is_some());
    }

    /// Replaces plugin `key`'s scene mappings (a slot preset was loaded into it).
    pub fn set_scene(&self, key: PluginKey, mappings: Vec<SceneMapping>) {
        let mut scene = self.scene.borrow_mut();
        if mappings.is_empty() {
            scene.remove(&key);
        } else {
            scene.insert(key, mappings);
        }
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
    }

    // ---- The setup ------------------------------------------------------

    /// Builds the mapping thread's setup from every mapping that applies to
    /// what's loaded, for `surfaces` (the shown devices with a surface): for
    /// `Engine::set_mappings`, then [`MappingsUi::install`] with the rest.
    pub fn build_setup(&self, ctx: &Ctx, surfaces: &[(Arc<str>, Arc<Surface>)]) -> (MapperSetup, Installed) {
        let mut targets: Vec<SetupTarget> = Vec::new();
        let mut by_concrete: HashMap<Concrete, usize> = HashMap::new();
        let mut specs: Vec<TargetSpec> = Vec::new();
        let mut controls: Vec<ControlSetup> = Vec::new();
        let mut mapped = HashSet::new();
        let mut signature = Signature::new();
        let all = self.all(ctx);
        for (surface_index, (device, surface)) in surfaces.iter().enumerate() {
            for (control_index, control) in surface.controls.iter().enumerate() {
                let reference = ControlRef { surface: surface.id.clone(), control: control.label.clone() };
                let mut links = Vec::new();
                let mut signed = Vec::new();
                for (_, _, concrete, mode) in all.iter().filter(|(_, c, _, _)| *c == reference) {
                    let Some(concrete) = concrete else { continue };
                    let target = match by_concrete.get(concrete) {
                        Some(&target) => target,
                        None => {
                            let Some(resolved) = Resolved::new(ctx, concrete.clone()) else { continue };
                            let Some((output, to_value)) = resolved.output(ctx) else { continue };
                            let position = resolved.position(ctx);
                            specs.push(TargetSpec { output, to_value, position });
                            targets.push(SetupTarget { resolved, seen: position });
                            by_concrete.insert(concrete.clone(), targets.len() - 1);
                            targets.len() - 1
                        }
                    };
                    links.push(Link { target, mode: mode.engine() });
                    signed.push((concrete.clone(), mode.clone()));
                }
                if links.is_empty() {
                    continue;
                }
                let settings = self.controls.borrow().get(&reference.key()).cloned().unwrap_or_default();
                mapped.insert((device.to_string(), control_index));
                signature.push((device.to_string(), control_index, settings.clone(), signed));
                controls.push(ControlSetup {
                    surface: surface_index,
                    control: control_index,
                    takeover: settings.takeover.engine(),
                    glide: Duration::from_millis(u64::from(settings.glide_ms)),
                    pass_through: settings.pass_through,
                    release_to_zero: control.kind == ControlKind::Pad,
                    links,
                });
            }
        }
        let setup = MapperSetup { surfaces: surfaces.to_vec(), controls, targets: specs };
        (setup, Installed { targets, mapped, signature })
    }

    /// Whether `installed` does what the current setup does (then there's
    /// no need to hand it over).
    pub fn unchanged(&self, installed: &Installed) -> bool {
        self.signature.borrow().as_ref() == Some(&installed.signature)
    }

    /// Keeps the current setup after all.
    pub fn keep_setup(&self) {
        self.setup_dirty.set(false);
    }

    /// Takes on a setup the engine was given, as setup `generation`.
    pub fn install(&self, installed: Installed, generation: u64) {
        self.generation.set(generation);
        *self.targets.borrow_mut() = installed.targets;
        *self.mapped.borrow_mut() = installed.mapped;
        *self.signature.borrow_mut() = Some(installed.signature);
        self.setup_dirty.set(false);
    }

    /// Every mapping: where it's kept, its control, what it drives now (if
    /// anything) and its mode.
    fn all(&self, ctx: &Ctx) -> Vec<(Entry, ControlRef, Option<Concrete>, MappingMode)> {
        let mut all = Vec::new();
        let scene = self.scene.borrow();
        let mut keys: Vec<&PluginKey> = scene.keys().collect();
        keys.sort();
        for key in keys {
            for (index, mapping) in scene[key].iter().enumerate() {
                let concrete = Concrete::Plugin { key: *key, target: mapping.target.clone() };
                all.push((Entry::Scene { key: *key, index }, mapping.control.clone(), Some(concrete), mapping.mode.clone()));
            }
        }
        for (index, mapping) in self.global.borrow().iter().enumerate() {
            let concrete = ctx.resolve(&mapping.target);
            all.push((Entry::Global { index }, mapping.control.clone(), concrete, mapping.mode.clone()));
        }
        all
    }

    /// What target `target` of setup `generation` is, unless the setup is old
    /// (a mapping moved it: for the caller to show or apply, then
    /// [`MappingsUi::saw`]).
    pub fn concrete(&self, generation: u64, target: usize) -> Option<Concrete> {
        if generation != self.generation.get() {
            return None;
        }
        Some(self.targets.borrow().get(target)?.resolved.concrete.clone())
    }

    /// Notes where target `target` is now, after a mapping moved it, so
    /// [`MappingsUi::sync_positions`] doesn't take that for a move from
    /// elsewhere.
    pub fn saw(&self, ctx: &Ctx, target: usize) {
        if let Some(setup_target) = self.targets.borrow_mut().get_mut(target) {
            setup_target.seen = setup_target.resolved.position(ctx);
        }
    }

    /// Tells the mapping thread about targets moved elsewhere since last seen
    /// (the mouse, a preset, a scene…). Call after showing what it applied.
    pub fn sync_positions(&self, ctx: &Ctx) {
        for (index, target) in self.targets.borrow_mut().iter_mut().enumerate() {
            let position = target.resolved.position(ctx);
            if (position - target.seen).abs() > 1e-4 {
                target.seen = position;
                ctx.engine.set_mapped_position(self.generation.get(), index, position);
            }
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    // ---- Learn mode -----------------------------------------------------

    /// What `(kind, slot, index)` (an on-screen learn target) maps.
    fn candidate(&self, ctx: &Ctx, kind: i32, slot: usize, index: usize) -> Option<Candidate> {
        let rack = |target| Some(Candidate::Rack(target));
        match kind {
            kinds::CELL => {
                let (key, control) = ctx.panel_cell(slot, index)?;
                Some(Candidate::Plugin { key, target: ctx.plugin_target(key, control)?, panel: Some((slot, index)) })
            }
            kinds::DRAWBAR => {
                let (key, panel) = ctx.panels.get(slot).copied().flatten()?;
                let control = panel.drawbars.get(index)?;
                Some(Candidate::Plugin { key, target: ctx.plugin_target(key, control)?, panel: None })
            }
            kinds::VOLUME => rack(RackTarget::Volume { slot }),
            kinds::MUTE => rack(RackTarget::Mute { slot }),
            kinds::TRANSPOSE => rack(RackTarget::Transpose { slot }),
            kinds::METRONOME_VOLUME => rack(RackTarget::MetronomeVolume),
            kinds::METRONOME_PLAY => rack(RackTarget::MetronomePlay),
            kinds::TEMPO => rack(RackTarget::Tempo),
            _ => None,
        }
    }

    fn candidate_concrete(ctx: &Ctx, candidate: &Candidate) -> Option<Concrete> {
        match candidate {
            Candidate::Plugin { key, target, .. } => Some(Concrete::Plugin { key: *key, target: target.clone() }),
            Candidate::Rack(target) => ctx.resolve(target),
        }
    }

    /// The selected control's mapping of `candidate`, if it has one.
    fn entry_of(&self, ctx: &Ctx, candidate: &Candidate) -> Option<Entry> {
        let selected = self.selected.borrow();
        let reference = &selected.as_ref()?.reference;
        let concrete = Self::candidate_concrete(ctx, candidate);
        let panel = match candidate {
            Candidate::Plugin { panel: Some((slot, index)), .. } => Some(RackTarget::Panel { slot: *slot, index: *index }),
            _ => None,
        };
        self.all(ctx).into_iter().find_map(|(entry, control, entry_concrete, _)| {
            if control != *reference {
                return None;
            }
            let is_panel = matches!(entry, Entry::Global { index } if Some(&self.global.borrow()[index].target) == panel.as_ref());
            (is_panel || (entry_concrete.is_some() && entry_concrete == concrete)).then_some(entry)
        })
    }

    /// A click on learn target `(kind, slot, index)`: maps it to the selected
    /// control, or unmaps it if it was. Returns whether the config changed
    /// (and needs saving), and the slot whose scene mappings changed.
    pub fn click(&self, ctx: &Ctx, kind: i32, slot: usize, index: usize) -> (bool, Option<usize>) {
        let Some(candidate) = self.candidate(ctx, kind, slot, index) else { return (false, None) };
        let Some(selected) = self.selected.borrow().clone() else { return (false, None) };
        let changed = match self.entry_of(ctx, &candidate) {
            Some(entry) => self.remove(ctx, entry),
            None => {
                let steps = Resolved::new(ctx, Self::candidate_concrete(ctx, &candidate).unwrap_or(Concrete::Tempo))
                    .map(|resolved| resolved.steps())
                    .unwrap_or_default();
                let mode = MappingMode::default_for(selected.kind(), &steps);
                let keep = if matches!(candidate, Candidate::Rack(_)) { Keep::Global } else { Keep::Scene };
                self.add(ctx, &selected.reference, candidate, mode, keep)
            }
        };
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        changed
    }

    fn add(
        &self,
        ctx: &Ctx,
        control: &ControlRef,
        candidate: Candidate,
        mode: MappingMode,
        keep: Keep,
    ) -> (bool, Option<usize>) {
        match candidate {
            Candidate::Plugin { key, target, .. } if keep == Keep::Scene => {
                let slot = ctx.slot_of(key);
                self.scene.borrow_mut().entry(key).or_default().push(SceneMapping { control: control.clone(), target, mode });
                (false, slot)
            }
            Candidate::Plugin { key, target, panel } => {
                let rack_target = match panel.filter(|_| keep == Keep::Panel) {
                    Some((slot, index)) => RackTarget::Panel { slot, index },
                    None => {
                        let Some(slot) = ctx.slot_of(key) else { return (false, None) };
                        let Some(meta) = ctx.engine.plugin(key) else { return (false, None) };
                        RackTarget::Plugin { slot, plugin: meta.id.clone(), target }
                    }
                };
                self.global.borrow_mut().push(GlobalMapping { control: control.clone(), target: rack_target, mode });
                (true, None)
            }
            Candidate::Rack(target) => {
                self.global.borrow_mut().push(GlobalMapping { control: control.clone(), target, mode });
                (true, None)
            }
        }
    }

    fn remove(&self, ctx: &Ctx, entry: Entry) -> (bool, Option<usize>) {
        match entry {
            Entry::Scene { key, index } => {
                let mut scene = self.scene.borrow_mut();
                if let Some(mappings) = scene.get_mut(&key)
                    && index < mappings.len()
                {
                    mappings.remove(index);
                    if mappings.is_empty() {
                        scene.remove(&key);
                    }
                }
                (false, ctx.slot_of(key))
            }
            Entry::Global { index } => {
                if index < self.global.borrow_mut().len() {
                    self.global.borrow_mut().remove(index);
                }
                (true, None)
            }
        }
    }

    /// Opens the right-click popup for learn target `(kind, slot, index)`.
    pub fn open_popup(&self, ctx: &Ctx, kind: i32, slot: usize, index: usize) -> bool {
        let Some(candidate) = self.candidate(ctx, kind, slot, index) else { return false };
        let Some(selected) = self.selected.borrow().clone() else { return false };
        let popup = match self.entry_of(ctx, &candidate) {
            Some(entry) => Popup::Assigned(entry),
            None => {
                let steps = Self::candidate_concrete(ctx, &candidate)
                    .and_then(|concrete| Resolved::new(ctx, concrete))
                    .map(|resolved| resolved.steps())
                    .unwrap_or_default();
                let keep = if matches!(candidate, Candidate::Rack(_)) { Keep::Global } else { Keep::Scene };
                Popup::Draft { candidate, mode: MappingMode::default_for(selected.kind(), &steps), keep }
            }
        };
        *self.popup.borrow_mut() = Some(popup);
        true
    }

    /// Assigns the popup's draft.
    pub fn assign_popup(&self, ctx: &Ctx) -> (bool, Option<usize>) {
        let Some(selected) = self.selected.borrow().clone() else { return (false, None) };
        let Some(Popup::Draft { candidate, mode, keep }) = self.popup.borrow().clone() else { return (false, None) };
        let result = self.add(ctx, &selected.reference, candidate.clone(), mode, keep);
        *self.popup.borrow_mut() = self.entry_of(ctx, &candidate).map(Popup::Assigned);
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        result
    }

    /// Expands row `row` of the targets pane (or collapses it).
    pub fn expand(&self, row: i32) {
        let row = usize::try_from(row).ok();
        self.expanded.set(if self.expanded.get() == row { None } else { row });
        self.view_dirty.set(true);
    }

    /// The entry behind row `row` (-1: the popup's, if assigned).
    fn row_entry(&self, row: i32) -> Option<Entry> {
        if row < 0 {
            return match &*self.popup.borrow() {
                Some(Popup::Assigned(entry)) => Some(*entry),
                _ => None,
            };
        }
        self.entries.borrow().get(row as usize).copied()
    }

    /// Removes row `row`'s mapping (-1: the popup's).
    pub fn remove_row(&self, ctx: &Ctx, row: i32) -> (bool, Option<usize>) {
        let Some(entry) = self.row_entry(row) else { return (false, None) };
        if row < 0 {
            *self.popup.borrow_mut() = None;
        }
        self.expanded.set(None);
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        self.remove(ctx, entry)
    }

    /// Removes every mapping of the selected control.
    pub fn remove_all(&self, ctx: &Ctx) -> (bool, Vec<usize>) {
        let Some(selected) = self.selected.borrow().clone() else { return (false, Vec::new()) };
        let mut slots = Vec::new();
        for (key, mappings) in self.scene.borrow_mut().iter_mut() {
            let before = mappings.len();
            mappings.retain(|mapping| mapping.control != selected.reference);
            if mappings.len() != before
                && let Some(slot) = ctx.slot_of(*key)
            {
                slots.push(slot);
            }
        }
        self.scene.borrow_mut().retain(|_, mappings| !mappings.is_empty());
        let before = self.global.borrow_mut().len();
        self.global.borrow_mut().retain(|mapping| mapping.control != selected.reference);
        self.expanded.set(None);
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        (self.global.borrow_mut().len() != before, slots)
    }

    /// Changes row `row`'s mode settings (-1: the popup's) with `change`.
    pub fn edit_mode(
        &self,
        ctx: &Ctx,
        row: i32,
        change: impl FnOnce(&mut MappingMode, &[f32]),
    ) -> (bool, Option<usize>) {
        if row < 0
            && let Some(Popup::Draft { candidate, mode, .. }) = &mut *self.popup.borrow_mut()
        {
            let steps = Self::candidate_concrete(ctx, candidate)
                .and_then(|concrete| Resolved::new(ctx, concrete))
                .map(|resolved| resolved.steps())
                .unwrap_or_default();
            change(mode, &steps);
            self.view_dirty.set(true);
            return (false, None);
        }
        let Some(entry) = self.row_entry(row) else { return (false, None) };
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        match entry {
            Entry::Scene { key, index } => {
                let steps = Resolved::new(ctx, self.scene.borrow()[&key][index].target.clone().into_concrete(key))
                    .map(|resolved| resolved.steps())
                    .unwrap_or_default();
                if let Some(mapping) = self.scene.borrow_mut().get_mut(&key).and_then(|m| m.get_mut(index)) {
                    change(&mut mapping.mode, &steps);
                }
                (false, ctx.slot_of(key))
            }
            Entry::Global { index } => {
                let steps = ctx
                    .resolve(&self.global.borrow_mut()[index].target)
                    .and_then(|concrete| Resolved::new(ctx, concrete))
                    .map(|resolved| resolved.steps())
                    .unwrap_or_default();
                if let Some(mapping) = self.global.borrow_mut().get_mut(index) {
                    change(&mut mapping.mode, &steps);
                }
                (true, None)
            }
        }
    }

    /// Sets row `row`'s mode to UI mode `mode`, if the control allows it.
    pub fn set_mode(&self, ctx: &Ctx, row: i32, mode: i32) -> (bool, Option<usize>) {
        let Some(kind) = self.selected.borrow().as_ref().map(Selected::kind) else { return (false, None) };
        if !allows(kind, mode) {
            return (false, None);
        }
        self.edit_mode(ctx, row, |current, steps| {
            if current.index() != mode
                && let Some(new) = current.with_index(mode, steps)
            {
                *current = new;
            }
        })
    }

    /// Moves row `row`'s mapping between the scene (`global` false) and every
    /// scene, and between a parameter and its panel position (`panel`).
    pub fn set_kept(
        &self,
        ctx: &Ctx,
        row: i32,
        global: bool,
        panel: bool,
    ) -> (bool, Option<usize>) {
        if row < 0
            && let Some(Popup::Draft { candidate, keep, .. }) = &mut *self.popup.borrow_mut()
        {
            *keep = match candidate {
                Candidate::Rack(_) => Keep::Global,
                Candidate::Plugin { panel: None, .. } => Keep::of(global, false),
                Candidate::Plugin { .. } => Keep::of(global, panel),
            };
            self.view_dirty.set(true);
            return (false, None);
        }
        let Some(entry) = self.row_entry(row) else { return (false, None) };
        let Some(selected) = self.selected.borrow().clone() else { return (false, None) };
        let (key, target, mode, current_panel) = match entry {
            Entry::Scene { key, index } => {
                let Some(mapping) = self.scene.borrow().get(&key).and_then(|m| m.get(index)).cloned() else {
                    return (false, None);
                };
                (key, mapping.target, mapping.mode, None)
            }
            Entry::Global { index } => {
                let Some(mapping) = self.global.borrow_mut().get(index).cloned() else { return (false, None) };
                match mapping.target {
                    RackTarget::Panel { slot, index } => {
                        let Some((key, control)) = ctx.panel_cell(slot, index) else { return (false, None) };
                        let Some(target) = ctx.plugin_target(key, control) else { return (false, None) };
                        (key, target, mapping.mode, Some((slot, index)))
                    }
                    RackTarget::Plugin { .. } => {
                        let Some(Concrete::Plugin { key, target }) = ctx.resolve(&mapping.target) else {
                            return (false, None);
                        };
                        (key, target, mapping.mode, None)
                    }
                    // The rack's own targets are kept in every scene.
                    _ => return (false, None),
                }
            }
        };
        // A panel position is kept in every scene; a parameter wherever chosen.
        let panel_position = current_panel.or_else(|| self.panel_position_of(ctx, key, &target));
        if panel && panel_position.is_none() {
            return (false, None);
        }
        let candidate = Candidate::Plugin { key, target, panel: panel_position };
        let (mut config_changed, mut slot) = self.remove(ctx, entry);
        let (added_config, added_slot) =
            self.add(ctx, &selected.reference, candidate.clone(), mode, Keep::of(global, panel));
        config_changed |= added_config;
        slot = slot.or(added_slot);
        let moved = self.entry_of(ctx, &candidate);
        if row >= 0 {
            // The moved row stays expanded.
            let rows = self.rows_of(ctx, &selected.reference);
            self.expanded.set(moved.and_then(|moved| rows.iter().position(|entry| *entry == moved)));
        } else {
            *self.popup.borrow_mut() = moved.map(Popup::Assigned);
        }
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
        (config_changed, slot)
    }

    /// The row position of `key`'s panel (as the instrument) showing `target`.
    fn panel_position_of(&self, ctx: &Ctx, key: PluginKey, target: &PluginTarget) -> Option<(usize, usize)> {
        let slot = ctx.panels.iter().position(|panel| panel.is_some_and(|(panel_key, _)| panel_key == key))?;
        let (_, panel) = ctx.panels[slot]?;
        let index = panel.row.iter().position(|&pool| {
            panel.pool.get(pool).is_some_and(|control| ctx.plugin_target(key, control).as_ref() == Some(target))
        })?;
        Some((slot, index))
    }

    /// The entries of `control`'s mappings, in the pane's order.
    fn rows_of(&self, ctx: &Ctx, control: &ControlRef) -> Vec<Entry> {
        self.all(ctx).into_iter().filter(|(_, c, _, _)| c == control).map(|(entry, ..)| entry).collect()
    }

    // ---- Showing it -----------------------------------------------------

    /// The selected control, for the pane's header.
    pub fn control_view(&self) -> LearnControl {
        let Some(selected) = self.selected.borrow().clone() else { return LearnControl::default() };
        let control = &selected.surface.controls[selected.control];
        let message = match control.message {
            Message::Cc(number) => format!("CC {number}"),
            Message::Note(number) => format!("Note {}", live_keys_engine::note_name(i32::from(number))),
            Message::PitchBend => "Pitch bend".into(),
            Message::ChannelPressure => "Pressure".into(),
        };
        let kind = match control.kind {
            ControlKind::Fader => "Fader",
            ControlKind::Knob => "Knob",
            ControlKind::Pad => "Pad",
            ControlKind::Button => "Button",
        };
        let settings = self.controls.borrow_mut().get(&selected.reference.key()).cloned().unwrap_or_default();
        LearnControl {
            selected: true,
            label: control.label.as_str().into(),
            detail: format!("{kind} · {} · {message}, channel {}", selected.surface.name, control.channel + 1).into(),
            kind: control.kind as i32,
            takeover: TakeoverSetting::ALL.iter().position(|t| *t == settings.takeover).unwrap_or(1) as i32,
            pass_through: settings.pass_through,
        }
    }

    /// Changes the selected control's settings.
    pub fn edit_control(&self, change: impl FnOnce(&mut ControlSettings)) {
        let Some(reference) = self.selected.borrow().as_ref().map(|s| s.reference.clone()) else { return };
        let mut controls = self.controls.borrow_mut();
        let settings = controls.entry(reference.key()).or_default();
        change(settings);
        if *settings == ControlSettings::default() {
            controls.remove(&reference.key());
        }
        drop(controls);
        self.setup_dirty.set(true);
        self.view_dirty.set(true);
    }

    pub fn set_takeover(&self, index: i32) {
        if let Some(takeover) = usize::try_from(index).ok().and_then(|index| TakeoverSetting::ALL.get(index)) {
            self.edit_control(|settings| settings.takeover = *takeover);
        }
    }

    /// The targets pane's rows, the popup's row (and whether it's assigned),
    /// and the expanded row; refreshes `rows`.
    pub fn refresh_view(&self, ctx: &Ctx) -> (Option<(LearnTargetRow, bool)>, i32) {
        self.view_dirty.set(false);
        let Some(selected) = self.selected.borrow().clone() else {
            self.entries.borrow_mut().clear();
            sync_model(&self.rows, Vec::new());
            return (None, -1);
        };
        let all = self.all(ctx);
        let mut entries = Vec::new();
        let mut rows = Vec::new();
        for (entry, control, concrete, mode) in &all {
            if *control != selected.reference {
                continue;
            }
            entries.push(*entry);
            rows.push(self.row(ctx, *entry, concrete.as_ref(), mode));
        }
        *self.entries.borrow_mut() = entries;
        if self.expanded.get().is_some_and(|row| row >= rows.len()) {
            self.expanded.set(None);
        }
        sync_model(&self.rows, rows);
        let popup = match self.popup.borrow().clone() {
            Some(Popup::Assigned(entry)) => all
                .iter()
                .find(|(e, ..)| *e == entry)
                .map(|(entry, _, concrete, mode)| (self.row(ctx, *entry, concrete.as_ref(), mode), true)),
            Some(Popup::Draft { candidate, mode, keep }) => {
                let concrete = Self::candidate_concrete(ctx, &candidate);
                let mut row = self.view_row(ctx, concrete.as_ref(), &mode);
                row.kept = i32::from(keep != Keep::Scene);
                row.can_keep_with_scene = matches!(candidate, Candidate::Plugin { .. });
                row.panel = match candidate {
                    _ if keep == Keep::Panel => 1,
                    Candidate::Plugin { panel: Some(_), .. } => 0,
                    _ => -1,
                };
                Some((row, false))
            }
            None => None,
        };
        (popup, self.expanded.get().map_or(-1, |row| row as i32))
    }

    fn row(&self, ctx: &Ctx, entry: Entry, concrete: Option<&Concrete>, mode: &MappingMode) -> LearnTargetRow {
        let mut row = self.view_row(ctx, concrete, mode);
        match entry {
            Entry::Scene { key, index } => {
                row.kept = 0;
                row.can_keep_with_scene = true;
                let target = self.scene.borrow().get(&key).and_then(|m| m.get(index)).map(|m| m.target.clone());
                row.panel = match target {
                    Some(target) if self.panel_position_of(ctx, key, &target).is_some() => 0,
                    _ => -1,
                };
            }
            Entry::Global { index } => {
                row.kept = 1;
                let target = &self.global.borrow()[index].target;
                row.can_keep_with_scene = matches!(target, RackTarget::Plugin { .. } | RackTarget::Panel { .. });
                row.panel = match target {
                    RackTarget::Panel { .. } => 1,
                    RackTarget::Plugin { .. } => match concrete {
                        Some(Concrete::Plugin { key, target }) if self.panel_position_of(ctx, *key, target).is_some() => 0,
                        _ => -1,
                    },
                    _ => -1,
                };
                if concrete.is_none() {
                    row.greyed = true;
                    let (name, place) = match target {
                        RackTarget::Plugin { slot, plugin, target } => (
                            match target {
                                PluginTarget::Param { symbol } => symbol.clone(),
                                PluginTarget::Setbfree { function } => function.clone(),
                            },
                            format!("{} · {plugin} (not loaded)", slot_name(*slot)),
                        ),
                        RackTarget::Panel { slot, index } => {
                            (format!("Panel position {}", index + 1), format!("{} (nothing there)", slot_name(*slot)))
                        }
                        _ => (String::new(), String::new()),
                    };
                    row.name = name.into();
                    row.place = place.into();
                } else if let RackTarget::Panel { index, .. } = target {
                    row.name = format!("Panel position {}: {}", index + 1, row.name).into();
                }
            }
        }
        row
    }

    /// A row's fields that don't depend on where the mapping is kept.
    fn view_row(&self, ctx: &Ctx, concrete: Option<&Concrete>, mode: &MappingMode) -> LearnTargetRow {
        let resolved = concrete.and_then(|concrete| Resolved::new(ctx, concrete.clone()));
        let text = |position: f32| -> slint::SharedString {
            resolved.as_ref().map_or_else(|| format!("{} %", (position * 100.0).round()), |r| r.text(ctx, position)).into()
        };
        let mut row = LearnTargetRow {
            name: resolved.as_ref().map(|r| r.name()).unwrap_or_default().into(),
            place: resolved.as_ref().map(|r| r.place(ctx)).unwrap_or_default().into(),
            chip: mode.chip().into(),
            greyed: resolved.is_none(),
            mode: mode.index(),
            ..Default::default()
        };
        match mode {
            MappingMode::Direct { from, to } | MappingMode::Mirrored { from, to } => {
                row.from = *from;
                row.to = *to;
                row.from_text = text(*from);
                row.to_text = text(*to);
            }
            MappingMode::Toggle { levels } => {
                let levels: Vec<LearnLevel> =
                    levels.iter().map(|&position| LearnLevel { position, text: text(position) }).collect();
                row.levels = ModelRc::new(VecModel::from(levels));
            }
            MappingMode::Momentary { value, velocity } => {
                row.value = *value;
                row.value_text = text(*value);
                row.velocity = *velocity;
            }
        }
        row
    }

    /// Which on-screen controls are learn targets, and which the selected
    /// control's; all 0 outside learn mode.
    pub fn flags(&self, ctx: &Ctx) -> LearnFlags {
        let mut flags = LearnFlags {
            cells: vec![Vec::new(); SLOT_COUNT],
            drawbars: vec![Vec::new(); SLOT_COUNT],
            volume: vec![0; MASTER_SLOT + 1],
            mute: vec![0; MASTER_SLOT + 1],
            transpose: vec![0; MASTER_SLOT + 1],
            ..Default::default()
        };
        if !self.learning.get() {
            return flags;
        }
        let selected = self.selected.borrow().clone();
        let mut concretes = HashSet::new();
        let mut panels = HashSet::new();
        if let Some(selected) = &selected {
            for (entry, control, concrete, _) in self.all(ctx) {
                if control != selected.reference {
                    continue;
                }
                if let Entry::Global { index } = entry
                    && let RackTarget::Panel { slot, index } = self.global.borrow()[index].target
                {
                    panels.insert((slot, index));
                } else if let Some(concrete) = concrete {
                    concretes.insert(concrete);
                }
            }
        }
        let state = |concrete: Option<Concrete>| match concrete {
            Some(concrete) if concretes.contains(&concrete) => 2,
            Some(_) => 1,
            None => 0,
        };
        for slot in 0..SLOT_COUNT {
            if let Some((key, panel)) = ctx.panels[slot] {
                let plugin = |control: &profiles::Control| {
                    ctx.plugin_target(key, control).map(|target| Concrete::Plugin { key, target })
                };
                flags.cells[slot] = panel
                    .row
                    .iter()
                    .enumerate()
                    .map(|(index, &pool)| {
                        if panels.contains(&(slot, index)) {
                            return 2;
                        }
                        state(panel.pool.get(pool).and_then(plugin))
                    })
                    .collect();
                flags.drawbars[slot] = panel.drawbars.iter().map(|control| state(plugin(control))).collect();
            }
        }
        for slot in 0..=MASTER_SLOT {
            flags.volume[slot] = state(Some(Concrete::Volume(slot)));
            flags.mute[slot] = state(Some(Concrete::Mute(slot)));
            flags.transpose[slot] = state(Some(Concrete::Transpose(slot)));
        }
        flags.metronome_volume = state(Some(Concrete::MetronomeVolume));
        flags.metronome_play = state(Some(Concrete::MetronomePlay));
        flags.tempo = state(Some(Concrete::Tempo));
        flags
    }
}

impl PluginTarget {
    fn into_concrete(self, key: PluginKey) -> Concrete {
        Concrete::Plugin { key, target: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mappings_round_trip_through_json() {
        let mapping = GlobalMapping {
            control: ControlRef { surface: "akai-lpd8".into(), control: "K1".into() },
            target: RackTarget::Plugin {
                slot: 1,
                plugin: "builtin:piano".into(),
                target: PluginTarget::Param { symbol: "brightness".into() },
            },
            mode: MappingMode::Mirrored { from: 0.2, to: 0.8 },
        };
        let json = serde_json::to_string(&mapping).unwrap();
        assert_eq!(serde_json::from_str::<GlobalMapping>(&json).unwrap(), mapping);
        let scene = SceneMapping {
            control: ControlRef { surface: "s".into(), control: "P1".into() },
            target: PluginTarget::Setbfree { function: "upper.drawbar16".into() },
            mode: MappingMode::Toggle { levels: vec![0.0, 0.5, 1.0] },
        };
        let json = serde_json::to_string(&scene).unwrap();
        assert_eq!(serde_json::from_str::<SceneMapping>(&json).unwrap(), scene);
    }
}
