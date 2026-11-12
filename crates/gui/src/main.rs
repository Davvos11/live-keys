slint::include_modules!();

mod config;
mod midi_inputs;
mod mock_data;
mod scenes;
mod session;
mod slot_presets;
mod slot_view;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use live_keys_engine::{
    Engine, LoadedPluginMeta, MAX_EFFECTS, MAX_MIDI_EFFECTS, ParamInfo, ParamKind, PluginEditor,
    PluginInfo, PluginKey, PluginKind, PluginScan, PresetInfo, SLOT_COUNT, Section, SlotChain,
    MAX_MIDI_INPUTS, default_plugin_search_paths, list_logical_output_devices,
};
use live_keys_lv2_ui::{
    LaunchUiRequest, PluginUiHandle, PortMessage, UiKind, best_supported_ui, launch_ui,
};
use live_keys_vst3_ui::Vst3Editor;
use live_keys_clap_ui::ClapEditor;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

const MAX_BLOCK_SIZE: usize = 8192;
/// How often the plugin-GUI-closed check and param-change forwarding run, and VST3
/// editors, CLAP GUIs and CLAP plugins' main-thread work get pumped (see
/// `Vst3Editor::idle`, `Engine::idle_plugins`).
const UI_POLL_INTERVAL: Duration = Duration::from_millis(16);
/// How often the plugin scan gets a turn, and for how long: VST3 plugins are
/// scanned on this thread, so in slices that leave the window time to draw.
const SCAN_TICK: Duration = Duration::from_millis(4);
const SCAN_SLICE: Duration = Duration::from_millis(12);
/// How often MIDI devices are looked for, so one plugged in (or unplugged) while
/// the app runs is connected and shows up in Settings. Listing opens an ALSA
/// sequencer client, so not every UI tick.
const MIDI_PORT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often the autosave timer checks whether to save the session.
const AUTOSAVE_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long after a change the session is saved at the earliest, so a run of
/// edits is saved once.
const AUTOSAVE_DELAY: Duration = Duration::from_secs(2);
/// How long every slot must have been silent before the session is saved.
const AUTOSAVE_QUIET: Duration = Duration::from_secs(3);
/// A slot peak at or below this (about -80 dB) counts as silence.
const SILENCE: f32 = 1e-4;
/// Sentinel entry always shown first in the audio device list, mapping to
/// `AppConfig.audio_device: None` — lets the OS pick the output device (e.g.
/// PipeWire's current default sink) instead of a specific one pinned by name.
/// Without this there's no way back to "no preference" once a specific device
/// has been selected once.
const SYSTEM_DEFAULT_LABEL: &str = "System Default";

/// What a plugin's own GUI wrote, on its way to the poll timer (GUIs call back
/// from their own threads).
enum GuiEdit {
    Param { key: PluginKey, param_id: u32, value: f32 },
    /// An atom for an LV2 plugin's atom input (`LV2_Atom` header, then body).
    Atom { key: PluginKey, port: u32, atom: Vec<u8> },
}

/// A plugin's open GUI. LV2 GUIs run on a thread of their own; VST3 editors and
/// CLAP GUIs live on this (the main) thread and need pumping.
enum OpenEditor {
    Lv2(PluginUiHandle),
    Vst3(Vst3Editor),
    Clap(ClapEditor),
}

impl OpenEditor {
    fn is_closed(&self) -> bool {
        match self {
            OpenEditor::Lv2(handle) => handle.is_closed(),
            OpenEditor::Vst3(editor) => editor.is_closed(),
            OpenEditor::Clap(editor) => editor.is_closed(),
        }
    }

    fn close(&mut self) {
        match self {
            OpenEditor::Lv2(handle) => handle.request_close(),
            OpenEditor::Vst3(editor) => editor.close(),
            OpenEditor::Clap(editor) => editor.close(),
        }
    }
}

/// Whether this host can show `meta`'s own GUI. For VST3 that's only known by
/// trying to create the editor; a failure shows up as the open error.
fn gui_supported(meta: &LoadedPluginMeta) -> bool {
    match &meta.editor {
        PluginEditor::Lv2 { plugin, .. } => {
            matches!(best_supported_ui(plugin), Some((_, UiKind::ExternalUi | UiKind::X11Ui)))
        }
        PluginEditor::Vst3(_) => true,
        PluginEditor::Clap(controller) => controller.has_gui(),
    }
}

/// `AppLogic`'s `section` argument: 0 for MIDI effects, else audio effects.
fn section_from(section: i32) -> Section {
    if section == 0 { Section::Midi } else { Section::Audio }
}

/// The plugin picker's `category` argument (see `Category` in globals.slint).
fn kind_from(category: i32) -> PluginKind {
    match category {
        0 => PluginKind::Instrument,
        1 => PluginKind::MidiEffect,
        _ => PluginKind::AudioEffect,
    }
}

/// Backs the plugin picker: every discovered plugin, and the model its list shows
/// for the current category and search.
struct Picker {
    /// Sorted by name, so a plugin installed in more than one format lists its
    /// formats side by side.
    plugins: RefCell<Vec<PluginInfo>>,
    rows: Rc<VecModel<PickerRow>>,
    /// The last `filter` arguments, to refilter with after pinning.
    filter: RefCell<(PluginKind, String)>,
}

impl Picker {
    fn new() -> Self {
        Self {
            plugins: RefCell::new(Vec::new()),
            rows: Rc::new(VecModel::default()),
            filter: RefCell::new((PluginKind::Instrument, String::new())),
        }
    }

    fn set_plugins(&self, mut plugins: Vec<PluginInfo>) {
        plugins.sort_by_key(|plugin| plugin.name.to_lowercase());
        *self.plugins.borrow_mut() = plugins;
    }

    /// Shows `kind`'s plugins whose name and format contain every word of
    /// `query` (case-insensitively), pinned ones first.
    fn filter(&self, kind: PluginKind, query: &str, pinned: &[String]) {
        *self.filter.borrow_mut() = (kind, query.to_string());
        let plugins = self.plugins.borrow();
        let matches = plugins.iter().filter(|plugin| {
            plugin.kind == kind
                && matches_query(&format!("{} {}", plugin.name, plugin.format.label()), query)
        });
        let (pinned_rows, other_rows): (Vec<_>, Vec<_>) =
            matches.partition(|plugin| pinned.contains(&plugin.id));
        // The first unpinned row gets a divider, if anything is pinned above it.
        let divider_at = (!pinned_rows.is_empty()).then_some(pinned_rows.len());
        let rows = pinned_rows
            .into_iter()
            .chain(other_rows)
            .enumerate()
            .map(|(index, plugin)| PickerRow {
                id: plugin.id.clone().into(),
                name: plugin.name.clone().into(),
                format: plugin.format.label().into(),
                pinned: divider_at.is_some_and(|divider| index < divider),
                after_pinned: divider_at == Some(index),
            })
            .collect::<Vec<_>>();
        self.rows.set_vec(rows);
    }

    fn refilter(&self, pinned: &[String]) {
        let (kind, query) = self.filter.borrow().clone();
        self.filter(kind, &query, pinned);
    }
}

/// Whether `text` contains every word of `query`, case-insensitively.
fn matches_query(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query.split_whitespace().all(|word| text.contains(&word.to_lowercase()))
}

/// Toggles `id`'s membership of `pinned`.
fn toggle_pinned(pinned: &mut Vec<String>, id: &str) {
    match pinned.iter().position(|pinned| pinned == id) {
        Some(index) => {
            pinned.remove(index);
        }
        None => pinned.push(id.to_string()),
    }
}

/// The parameter editor's row for `param` of plugin `key` at `value`.
fn param_row(engine: &Engine, key: PluginKey, param: &ParamInfo, value: f32) -> ParamRow {
    let kind = match param.kind {
        ParamKind::Continuous => 0,
        ParamKind::Toggle => 1,
        ParamKind::Stepped => 2,
        ParamKind::Choice => 3,
        ParamKind::Note => 4,
    };
    let is_choice = param.kind == ParamKind::Choice;
    let labels: Vec<slint::SharedString> = if is_choice {
        param.choices.iter().map(|choice| choice.label.as_str().into()).collect()
    } else {
        Vec::new()
    };
    let values: Vec<f32> = if is_choice {
        param.choices.iter().map(|choice| choice.value).collect()
    } else {
        Vec::new()
    };
    ParamRow {
        param_id: param.id as i32,
        name: param.name.as_str().into(),
        kind,
        min_value: param.min_value,
        max_value: param.max_value,
        steps: param.steps as i32,
        logarithmic: param.logarithmic,
        value,
        text: engine.param_text(key, param.id, value).into(),
        choices: ModelRc::new(VecModel::from(labels)),
        choice_values: ModelRc::new(VecModel::from(values)),
        choice_index: param.choice_index(value).map_or(-1, |index| index as i32),
    }
}

/// A plugin's open parameter window and the model its list shows.
struct ParamWindow {
    window: ParamsWindow,
    rows: Rc<VecModel<ParamRow>>,
}

/// The preset a plugin last loaded or saved, and whether a parameter changed
/// since.
struct CurrentPreset {
    preset: PresetInfo,
    changed: bool,
}

/// The open preset browser: whose presets, all of them, the search, and the
/// ones shown for it (in the same order as `AppState.preset-rows`).
struct PresetBrowser {
    key: PluginKey,
    presets: Vec<PresetInfo>,
    query: String,
    shown: Vec<PresetInfo>,
}

/// The slot preset a slot last loaded or saved, and whether its chain or a
/// parameter changed since.
struct CurrentSlotPreset {
    dir: std::path::PathBuf,
    name: String,
    changed: bool,
}

/// The scene last recalled or saved.
struct CurrentScene {
    dir: std::path::PathBuf,
    name: String,
    /// The slots it restores or clears.
    slots: Vec<usize>,
    changed: bool,
}

impl CurrentScene {
    fn of(stored: &scenes::Stored, changed: bool) -> Self {
        Self {
            dir: stored.dir.clone(),
            name: stored.scene.name.clone(),
            slots: stored.scene.affected_slots(),
            changed,
        }
    }
}

/// What a slot's slot-preset browser shows: every instrument's folder, or one
/// folder — the presets of slots with that instrument (`None`: without one).
#[derive(Clone, PartialEq)]
enum SlotFolder {
    All,
    Of(Option<String>),
}

/// A row of a slot-preset browser and what choosing it does.
enum SlotEntry {
    Folder { instrument: Option<String>, label: String, count: usize },
    /// An index into `Rack::slot_store`.
    Preset(usize),
}

/// Makes `model` hold `rows`, changing only the rows that differ, so Slint keeps
/// the other rows' elements (and their local state, like a half-typed preset
/// name) instead of rebuilding them all.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (index, row) in rows.into_iter().enumerate() {
        if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
}

/// Runs `action` with the main window's `AppLogic`, if the window still exists.
fn with_logic(app: &slint::Weak<AppWindow>, action: impl FnOnce(&AppLogic<'_>)) {
    if let Some(app) = app.upgrade() {
        action(&app.global::<AppLogic>());
    }
}

/// Parameter editor rows for every parameter of plugin `key`, at their current
/// values.
fn param_rows(engine: &Engine, key: PluginKey) -> Vec<ParamRow> {
    let Some(meta) = engine.plugin(key) else {
        return Vec::new();
    };
    meta.params
        .iter()
        .map(|param| {
            let value = engine.param_value(key, param.id).unwrap_or(param.default_value);
            param_row(engine, key, param, value)
        })
        .collect()
}

/// Which slot `key` is loaded in, if any.
fn slot_of(engine: &Engine, key: PluginKey) -> Option<usize> {
    (0..SLOT_COUNT).find(|&slot| {
        engine
            .slot(slot)
            .is_some_and(|chain| chain.plugins().any(|meta| meta.key == key))
    })
}

/// GUI-side state of the Live tab's slots: the models backing `AppState.slots`,
/// every open plugin GUI and every open parameter window.
struct Rack {
    slots: Rc<VecModel<SlotState>>,
    /// One per slot, kept for the app's lifetime and refilled in place, so
    /// refreshing a slot's other fields doesn't hand Slint a new model (and
    /// rebuild that slot's rows).
    midi_effects: Vec<Rc<VecModel<EffectRow>>>,
    audio_effects: Vec<Rc<VecModel<EffectRow>>>,
    /// Per slot, like the effect models: the instrument parameters shown as knobs.
    knobs: Vec<Rc<VecModel<KnobRow>>>,
    /// Per slot: what it was before its ✕ cleared it, until Undo, Dismiss or
    /// anything else loaded into it.
    closed: RefCell<Vec<Option<slot_view::Closed>>>,
    open_uis: RefCell<HashMap<PluginKey, OpenEditor>>,
    param_windows: RefCell<HashMap<PluginKey, ParamWindow>>,
    /// The preset last loaded or saved per plugin.
    current_presets: RefCell<HashMap<PluginKey, CurrentPreset>>,
    browser: RefCell<Option<PresetBrowser>>,
    /// Backs `AppState.preset-rows`.
    preset_rows: Rc<VecModel<PresetRow>>,
    /// Per slot, like the effect models: its slot-preset browser's rows.
    slot_preset_rows: Vec<Rc<VecModel<SlotPresetRow>>>,
    /// Every slot preset on disk.
    slot_store: RefCell<Vec<slot_presets::Stored>>,
    current_slot_presets: RefCell<Vec<Option<CurrentSlotPreset>>>,
    slot_folders: RefCell<Vec<SlotFolder>>,
    /// Each slot's instrument id when last refreshed, to notice it changing (and
    /// open the new instrument's folder).
    last_instruments: RefCell<Vec<Option<String>>>,
    /// Every scene on disk.
    scene_store: RefCell<Vec<scenes::Stored>>,
    scene_query: RefCell<String>,
    /// Backs `AppState.scene-rows`: the scenes matching `scene_query`.
    scene_rows: Rc<VecModel<SceneRow>>,
    current_scene: RefCell<Option<CurrentScene>>,
    /// Backs `AppState.scene-form-slots`.
    scene_form_slots: Rc<VecModel<SceneSlotChoice>>,
    scene_status: RefCell<String>,
    /// The scene being edited or deleted (its folder), and the name the form
    /// starts from (`AppState.scene-target-name`).
    scene_target: RefCell<Option<std::path::PathBuf>>,
    scene_target_name: RefCell<String>,
    /// Since when something has changed that the saved session doesn't have
    /// yet; `None` when it's up to date.
    session_changed_since: Cell<Option<Instant>>,
    /// Set when the scene pane needs refreshing; the poll timer does it (most
    /// changes happen where there's no window handle).
    scenes_dirty: Cell<bool>,
}

impl Rack {
    fn new() -> Self {
        fn models<T: 'static>() -> Vec<Rc<VecModel<T>>> {
            (0..SLOT_COUNT).map(|_| Rc::new(VecModel::default())).collect()
        }
        let (midi_effects, audio_effects) = (models(), models());
        let slot_preset_rows = models();
        let knobs = models();
        let slots = (0..SLOT_COUNT)
            .map(|slot| SlotState {
                knobs: ModelRc::from(knobs[slot].clone()),
                midi_effects: ModelRc::from(midi_effects[slot].clone()),
                audio_effects: ModelRc::from(audio_effects[slot].clone()),
                slot_preset_rows: ModelRc::from(slot_preset_rows[slot].clone()),
                instrument: EffectRow {
                    key: -1,
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Self {
            slots: Rc::new(VecModel::from(slots)),
            midi_effects,
            audio_effects,
            knobs,
            closed: RefCell::new((0..SLOT_COUNT).map(|_| None).collect()),
            open_uis: RefCell::new(HashMap::new()),
            param_windows: RefCell::new(HashMap::new()),
            current_presets: RefCell::new(HashMap::new()),
            browser: RefCell::new(None),
            preset_rows: Rc::new(VecModel::default()),
            slot_preset_rows,
            slot_store: RefCell::new(slot_presets::list()),
            current_slot_presets: RefCell::new((0..SLOT_COUNT).map(|_| None).collect()),
            slot_folders: RefCell::new(vec![SlotFolder::All; SLOT_COUNT]),
            last_instruments: RefCell::new(vec![None; SLOT_COUNT]),
            scene_store: RefCell::new(scenes::list()),
            scene_query: RefCell::default(),
            scene_rows: Rc::new(VecModel::default()),
            current_scene: RefCell::new(None),
            scene_form_slots: Rc::new(VecModel::from(vec![SceneSlotChoice::default(); SLOT_COUNT])),
            scene_status: RefCell::default(),
            scene_target: RefCell::default(),
            scene_target_name: RefCell::default(),
            session_changed_since: Cell::new(None),
            scenes_dirty: Cell::new(true),
        }
    }

    /// Recomputes `slot`'s chain and its plugins' GUI and parameter window state
    /// from the engine. `error`, if given, is shown as the slot's status line.
    fn refresh_slot(&self, engine: &Engine, slot: usize, error: Option<String>) {
        let empty = SlotChain::default();
        let chain = engine.slot(slot).unwrap_or(&empty);

        let effect_rows = |section: Section| -> Vec<EffectRow> {
            chain.effects(section).iter().map(|meta| self.plugin_row(meta)).collect()
        };
        sync_model(&self.midi_effects[slot], effect_rows(Section::Midi));
        sync_model(&self.audio_effects[slot], effect_rows(Section::Audio));

        let mut row = self.slots.row_data(slot).unwrap();
        row.instrument = chain.instrument.as_ref().map_or(
            EffectRow {
                key: -1,
                ..Default::default()
            },
            |meta| self.plugin_row(meta),
        );
        row.status = error.unwrap_or_default().into();
        row.artwork = slot_view::artwork(chain.instrument.as_ref().map(|meta| meta.name.as_str()));
        sync_model(
            &self.knobs[slot],
            chain.instrument.as_ref().map(|meta| slot_view::knob_rows(engine, meta)).unwrap_or_default(),
        );
        // Anything loaded into a cleared slot replaces what Undo would bring back.
        if chain.plugins().next().is_some() {
            self.closed.borrow_mut()[slot] = None;
        }
        row.closed_name = self.closed.borrow()[slot].as_ref().map(|c| c.name.as_str()).unwrap_or_default().into();

        // A newly loaded instrument opens its folder of slot presets; without
        // one, all folders show.
        let instrument_id = chain.instrument.as_ref().map(|meta| meta.id.clone());
        if self.last_instruments.borrow()[slot] != instrument_id {
            self.slot_folders.borrow_mut()[slot] = match &instrument_id {
                Some(id) => SlotFolder::Of(Some(id.clone())),
                None => SlotFolder::All,
            };
            self.last_instruments.borrow_mut()[slot] = instrument_id;
        }
        {
            let current = self.current_slot_presets.borrow();
            let current = current[slot].as_ref();
            row.slot_preset_name = current.map(|c| c.name.as_str()).unwrap_or_default().into();
            row.slot_preset_changed = current.is_some_and(|c| c.changed);
            let store = self.slot_store.borrow();
            row.slot_preset_folder = match &self.slot_folders.borrow()[slot] {
                SlotFolder::All => "".into(),
                SlotFolder::Of(None) => slot_presets::NO_INSTRUMENT.into(),
                SlotFolder::Of(Some(id)) => store
                    .iter()
                    .find(|stored| stored.preset.instrument_id() == Some(id.as_str()))
                    .map(|stored| stored.preset.instrument_name())
                    .or(chain.instrument.as_ref().map(|meta| meta.name.as_str()))
                    .unwrap_or(id.as_str())
                    .into(),
            };
            let rows = self
                .slot_entries(slot)
                .into_iter()
                .map(|entry| match entry {
                    SlotEntry::Folder { label, count, .. } => SlotPresetRow {
                        name: label.into(),
                        folder: true,
                        count: count as i32,
                        current: false,
                    },
                    SlotEntry::Preset(index) => SlotPresetRow {
                        name: store[index].preset.name.as_str().into(),
                        folder: false,
                        count: 0,
                        current: current.is_some_and(|c| c.dir == store[index].dir),
                    },
                })
                .collect();
            sync_model(&self.slot_preset_rows[slot], rows);
        }
        self.slots.set_row_data(slot, row);
    }

    /// What `slot`'s slot-preset browser lists, in order: the instrument folders
    /// (by name), or the open folder's presets (by name).
    fn slot_entries(&self, slot: usize) -> Vec<SlotEntry> {
        let store = self.slot_store.borrow();
        match &self.slot_folders.borrow()[slot] {
            SlotFolder::All => {
                let mut folders: Vec<SlotEntry> = Vec::new();
                for stored in store.iter() {
                    let instrument = stored.preset.instrument_id().map(str::to_string);
                    let existing = folders.iter_mut().find(|entry| {
                        matches!(entry, SlotEntry::Folder { instrument: other, .. } if *other == instrument)
                    });
                    match existing {
                        Some(SlotEntry::Folder { count, .. }) => *count += 1,
                        _ => folders.push(SlotEntry::Folder {
                            instrument,
                            label: stored.preset.instrument_name().to_string(),
                            count: 1,
                        }),
                    }
                }
                folders.sort_by_key(|entry| match entry {
                    SlotEntry::Folder { label, .. } => label.to_lowercase(),
                    SlotEntry::Preset(_) => String::new(),
                });
                folders
            }
            SlotFolder::Of(instrument) => store
                .iter()
                .enumerate()
                .filter(|(_, stored)| stored.preset.instrument_id() == instrument.as_deref())
                .map(|(index, _)| SlotEntry::Preset(index))
                .collect(),
        }
    }

    fn show_slot_folder(&self, slot: usize, folder: SlotFolder) {
        self.slot_folders.borrow_mut()[slot] = folder;
    }

    /// Notes that the session needs saving.
    fn session_changed(&self) {
        if self.session_changed_since.get().is_none() {
            self.session_changed_since.set(Some(Instant::now()));
        }
    }

    fn set_current_slot_preset(&self, slot: usize, current: Option<CurrentSlotPreset>) {
        self.session_changed();
        self.current_slot_presets.borrow_mut()[slot] = current;
    }

    /// Notes that `slot`'s chain changed, so its current slot preset (if any)
    /// shows as changed. Returns whether that's news.
    fn mark_slot_changed(&self, slot: usize) -> bool {
        self.session_changed();
        self.mark_scene_changed(slot);
        match self.current_slot_presets.borrow_mut()[slot].as_mut() {
            Some(current) if !current.changed => {
                current.changed = true;
                true
            }
            _ => false,
        }
    }

    /// Notes that `slot`'s chain, settings or routing changed, so the current
    /// scene shows as changed if it includes that slot.
    fn mark_scene_changed(&self, slot: usize) {
        self.session_changed();
        if let Some(current) = self.current_scene.borrow_mut().as_mut()
            && !current.changed
            && current.slots.contains(&slot)
        {
            current.changed = true;
            self.scenes_dirty.set(true);
        }
    }

    /// The scenes matching the search, as indices into `scene_store`: every
    /// word of the query must appear in the name.
    fn shown_scenes(&self) -> Vec<usize> {
        let query = self.scene_query.borrow().to_lowercase();
        let store = self.scene_store.borrow();
        (0..store.len())
            .filter(|&index| {
                let name = store[index].scene.name.to_lowercase();
                query.split_whitespace().all(|word| name.contains(word))
            })
            .collect()
    }

    /// Pushes the scene pane's state into the window.
    fn refresh_scenes(&self, app: &AppWindow) {
        let state = app.global::<AppState>();
        let current = self.current_scene.borrow();
        let store = self.scene_store.borrow();
        let rows = self
            .shown_scenes()
            .into_iter()
            .map(|index| {
                let scene = &store[index].scene;
                let modes: Vec<i32> = (0..SLOT_COUNT).map(|slot| scene.mode(slot).index()).collect();
                SceneRow {
                    name: scene.name.as_str().into(),
                    current: current.as_ref().is_some_and(|c| c.dir == store[index].dir),
                    modes: ModelRc::new(VecModel::from(modes)),
                }
            })
            .collect();
        sync_model(&self.scene_rows, rows);
        state.set_scene_count(store.len() as i32);
        state.set_current_scene(current.as_ref().map(|c| c.name.as_str()).unwrap_or_default().into());
        state.set_current_scene_changed(current.as_ref().is_some_and(|c| c.changed));
        state.set_scene_status(self.scene_status.borrow().as_str().into());
        state.set_scene_target_name(self.scene_target_name.borrow().as_str().into());
    }

    /// The scene in row `index` of the list, or the current scene for -1.
    fn scene_at(&self, index: i32) -> Option<scenes::Stored> {
        if index < 0 {
            return self.current_scene_stored();
        }
        let shown = self.shown_scenes();
        let &index = shown.get(index as usize)?;
        self.scene_store.borrow().get(index).cloned()
    }

    fn stored_scene(&self, dir: &std::path::Path) -> Option<scenes::Stored> {
        self.scene_store.borrow().iter().find(|stored| stored.dir == dir).cloned()
    }

    /// Makes `stored` the scene to edit or delete.
    fn target_scene(&self, stored: &scenes::Stored) {
        *self.scene_target.borrow_mut() = Some(stored.dir.clone());
        *self.scene_target_name.borrow_mut() = stored.scene.name.clone();
        self.scene_status.borrow_mut().clear();
        self.scenes_dirty.set(true);
    }

    /// Re-reads the scenes on disk, after one was saved, changed or deleted, and
    /// shows `result`'s error, if any.
    fn after_scene_change(&self, action: &str, name: &str, result: anyhow::Result<()>) {
        *self.scene_status.borrow_mut() = match result {
            Ok(()) => String::new(),
            Err(err) => {
                log::error!("{action} scene '{name}' failed: {err:#}");
                format!("{action} scene \"{name}\" failed: {err:#}")
            }
        };
        *self.scene_store.borrow_mut() = scenes::list();
        self.scenes_dirty.set(true);
        self.session_changed();
    }

    /// The current scene as stored.
    fn current_scene_stored(&self) -> Option<scenes::Stored> {
        let dir = self.current_scene.borrow().as_ref()?.dir.clone();
        self.stored_scene(&dir)
    }

    /// Re-reads the slot presets on disk, after one was saved or deleted.
    fn reload_slot_store(&self) {
        *self.slot_store.borrow_mut() = slot_presets::list();
    }

    /// `key`'s current plugin preset and whether it changed since, for saving in
    /// a slot preset.
    fn plugin_preset_state(&self, key: PluginKey) -> (Option<PresetInfo>, bool) {
        match self.current_presets.borrow().get(&key) {
            Some(current) => (Some(current.preset.clone()), current.changed),
            None => (None, false),
        }
    }

    /// The chain row of a loaded plugin.
    fn plugin_row(&self, meta: &LoadedPluginMeta) -> EffectRow {
        let current_presets = self.current_presets.borrow();
        let current = current_presets.get(&meta.key);
        EffectRow {
            key: meta.key as i32,
            name: meta.name.as_str().into(),
            gui_supported: gui_supported(meta),
            gui_open: self.open_uis.borrow().contains_key(&meta.key),
            params_open: self.param_windows.borrow().contains_key(&meta.key),
            graph: slot_view::graph(&meta.name),
            preset_name: current.map(|c| c.preset.name.as_str()).unwrap_or_default().into(),
            preset_user: current.is_some_and(|c| c.preset.user),
            preset_changed: current.is_some_and(|c| c.changed),
        }
    }

    /// Opens a window listing `key`'s parameters at their current values. Its
    /// edits go to `AppLogic.set-param`, like any other host-side edit.
    fn open_params(&self, app: &AppWindow, engine: &Engine, key: PluginKey) -> anyhow::Result<()> {
        if self.param_windows.borrow().contains_key(&key) {
            return Ok(());
        }
        let Some(meta) = engine.plugin(key) else {
            return Ok(());
        };
        let rows = Rc::new(VecModel::from(param_rows(engine, key)));

        let window = ParamsWindow::new()?;
        window.set_plugin_name(meta.name.as_str().into());
        window.set_params(ModelRc::from(rows.clone()));
        // Every action goes through the main window's `AppLogic`, like the rest
        // of the UI; this window can't reach it itself.
        let app_weak = app.as_weak();
        window.on_set_param(move |param_id, value| {
            with_logic(&app_weak, |logic| logic.invoke_set_param(key as i32, param_id, value));
        });
        // Closing it by its title bar only hides it; `poll_uis` notices and
        // drops it.
        window.show()?;
        self.param_windows.borrow_mut().insert(key, ParamWindow { window, rows });
        Ok(())
    }

    /// Opens `key`'s preset browser (closing any other), listing all its presets.
    fn open_browser(&self, app: &AppWindow, engine: &Engine, key: PluginKey, pinned: &[String]) {
        *self.browser.borrow_mut() = Some(PresetBrowser {
            key,
            presets: engine.presets(key),
            query: String::new(),
            shown: Vec::new(),
        });
        self.show_browser(app, pinned);
    }

    fn close_browser(&self, app: &AppWindow) {
        *self.browser.borrow_mut() = None;
        self.show_browser(app, &[]);
    }

    /// Shows only the browser's presets matching `query`.
    fn filter_browser(&self, app: &AppWindow, query: &str, pinned: &[String]) {
        if let Some(browser) = self.browser.borrow_mut().as_mut() {
            browser.query = query.to_string();
        }
        self.show_browser(app, pinned);
    }

    /// Re-reads the browser's presets, e.g. after one was saved or deleted.
    fn reread_browser(&self, app: &AppWindow, engine: &Engine, pinned: &[String]) {
        if let Some(browser) = self.browser.borrow_mut().as_mut() {
            browser.presets = engine.presets(browser.key);
        }
        self.show_browser(app, pinned);
    }

    /// Fills `AppState`'s browser properties: the presets matching the search,
    /// favourites first, or nothing if no browser is open.
    fn show_browser(&self, app: &AppWindow, pinned: &[String]) {
        let state = app.global::<AppState>();
        let mut browser = self.browser.borrow_mut();
        let Some(browser) = browser.as_mut() else {
            state.set_preset_browser_key(-1);
            self.preset_rows.set_vec(Vec::new());
            return;
        };
        let (pinned_presets, others): (Vec<_>, Vec<_>) = browser
            .presets
            .iter()
            .filter(|preset| matches_query(&preset.name, &browser.query))
            .cloned()
            .partition(|preset| pinned.contains(&preset.id));
        let divider_at = (!pinned_presets.is_empty()).then_some(pinned_presets.len());
        browser.shown = pinned_presets.into_iter().chain(others).collect();
        let current_presets = self.current_presets.borrow();
        let current = current_presets.get(&browser.key).map(|c| c.preset.id.as_str());
        let rows = browser
            .shown
            .iter()
            .enumerate()
            .map(|(index, preset)| PresetRow {
                name: preset.name.as_str().into(),
                user: preset.user,
                pinned: divider_at.is_some_and(|divider| index < divider),
                after_pinned: divider_at == Some(index),
                current: current == Some(preset.id.as_str()),
            })
            .collect::<Vec<_>>();
        self.preset_rows.set_vec(rows);
        state.set_preset_browser_key(browser.key as i32);
        state.set_preset_count(browser.presets.len() as i32);
        state.set_preset_query(browser.query.as_str().into());
    }

    /// The open browser's plugin and its preset at `index` in the shown rows.
    fn browser_preset(&self, index: i32) -> Option<(PluginKey, PresetInfo)> {
        let browser = self.browser.borrow();
        let browser = browser.as_ref()?;
        Some((browser.key, browser.shown.get(usize::try_from(index).ok()?)?.clone()))
    }

    fn current_preset(&self, key: PluginKey) -> Option<PresetInfo> {
        self.current_presets.borrow().get(&key).map(|c| c.preset.clone())
    }

    /// Notes that one of `key`'s parameters moved, so its current preset (if
    /// any) shows as changed.
    fn mark_changed(&self, engine: &Engine, key: PluginKey) {
        let newly_changed = match self.current_presets.borrow_mut().get_mut(&key) {
            Some(current) if !current.changed => {
                current.changed = true;
                true
            }
            _ => false,
        };
        let Some(slot) = slot_of(engine, key) else {
            return;
        };
        if self.mark_slot_changed(slot) || newly_changed {
            self.refresh_slot(engine, slot, None);
        }
    }

    fn set_current_preset(&self, key: PluginKey, preset: Option<PresetInfo>) {
        self.set_current_preset_changed(key, preset, false);
    }

    fn set_current_preset_changed(&self, key: PluginKey, preset: Option<PresetInfo>, changed: bool) {
        self.session_changed();
        let mut current = self.current_presets.borrow_mut();
        match preset {
            Some(preset) => current.insert(key, CurrentPreset { preset, changed }),
            None => current.remove(&key),
        };
    }

    /// Rebuilds every row of `key`'s parameter window, e.g. after a preset changed
    /// them all.
    /// Its open LV2 GUI is told every value too, since it can't ask (a VST3
    /// editor follows its controller by itself).
    fn refresh_param_rows(&self, engine: &Engine, key: PluginKey) {
        if let Some(open) = self.param_windows.borrow().get(&key) {
            open.rows.set_vec(param_rows(engine, key));
        }
        if let Some(slot) = slot_of(engine, key)
            && let Some(instrument) = engine.slot(slot).and_then(|chain| chain.instrument.as_ref())
            && instrument.key == key
        {
            sync_model(&self.knobs[slot], slot_view::knob_rows(engine, instrument));
        }
        if let (Some(OpenEditor::Lv2(handle)), Some(meta)) = (self.open_uis.borrow().get(&key), engine.plugin(key)) {
            for param in &meta.params {
                let value = engine.param_value(key, param.id).unwrap_or(param.default_value);
                handle.notify(PortMessage::Control { port: param.id, value });
            }
        }
    }

    /// Tells `key`'s open LV2 GUI about a host-side parameter change.
    fn notify_ui(&self, key: PluginKey, param_id: u32, value: f32) {
        if let Some(OpenEditor::Lv2(handle)) = self.open_uis.borrow().get(&key) {
            handle.notify(PortMessage::Control { port: param_id, value });
        }
    }

    fn close_params(&self, key: PluginKey) {
        let closed = self.param_windows.borrow_mut().remove(&key);
        if let Some(closed) = closed
            && let Err(err) = closed.window.hide()
        {
            log::warn!("failed to hide a parameter window: {err}");
        }
    }

    /// Pushes `slot`'s MIDI filter and volume into its row. The widgets edit these
    /// directly; this keeps the row in step so a rebuilt row shows the same values.
    /// Shows `config`'s filter and volume in slot `slot`, with a button for
    /// each of the `input_count` MIDI inputs.
    fn set_routing(&self, slot: usize, config: &config::SlotConfig, input_count: usize) {
        let mut row = self.slots.row_data(slot).unwrap();
        let listening: Vec<bool> = (0..input_count).map(|input| config.midi_inputs & 1 << input != 0).collect();
        row.midi_inputs = ModelRc::new(VecModel::from(listening));
        row.midi_channel = config.midi_channel.map_or(0, |channel| i32::from(channel) + 1);
        row.note_low = config.note_low.into();
        row.note_high = config.note_high.into();
        row.velocity_low = config.velocity_low.into();
        row.velocity_high = config.velocity_high.into();
        row.volume_db = config.volume_db;
        row.muted = config.muted;
        row.transpose = config.transpose.into();
        self.slots.set_row_data(slot, row);
    }

    fn set_all_routing(&self, config: &config::AppConfig) {
        for (slot, slot_config) in config.slots.iter().enumerate() {
            self.set_routing(slot, slot_config, config.midi_inputs.len());
        }
    }

    fn refresh_all(&self, engine: &Engine) {
        for slot in 0..SLOT_COUNT {
            self.refresh_slot(engine, slot, None);
        }
    }

    /// Shows a param value that was just sent to the engine in the plugin's
    /// parameter window, if open.
    fn show_param_value(&self, engine: &Engine, key: PluginKey, param_id: u32, value: f32) {
        self.show_knob_value(engine, key, param_id);
        let param_windows = self.param_windows.borrow();
        let Some(rows) = param_windows.get(&key).map(|open| &open.rows) else {
            return;
        };
        let Some(info) = engine
            .plugin(key)
            .and_then(|meta| meta.params.iter().find(|param| param.id == param_id))
        else {
            return;
        };
        if let Some(row) = rows.iter().position(|param| param.param_id as u32 == param_id) {
            rows.set_row_data(row, param_row(engine, key, info, value));
        }
    }

    /// Updates the knob showing `key`'s parameter `param_id`, if there is one.
    fn show_knob_value(&self, engine: &Engine, key: PluginKey, param_id: u32) {
        let Some(slot) = slot_of(engine, key) else {
            return;
        };
        let knobs = &self.knobs[slot];
        let Some(index) = knobs.iter().position(|knob| knob.key == key as i32 && knob.param_id as u32 == param_id)
        else {
            return;
        };
        if let Some(param) = engine.plugin(key).and_then(|meta| meta.params.iter().find(|p| p.id == param_id)) {
            knobs.set_row_data(index, slot_view::knob_row(engine, key, param));
        }
    }

    /// Closes `key`'s plugin GUI, if open. For an LV2 GUI this is only a
    /// best-effort signal in case its hosting thread is still responsive; either
    /// way, our own bookkeeping is cleared unconditionally so the user can try a
    /// fresh session even if the previous one's plugin code has wedged and will
    /// never confirm closing itself (observed with Helm's own UI). The old thread,
    /// if genuinely stuck, is abandoned — it leaks until the app exits, which beats
    /// leaving the button permanently disabled. A VST3 editor closes synchronously.
    fn close_ui(&self, key: PluginKey) {
        let editor = self.open_uis.borrow_mut().remove(&key);
        if let Some(mut editor) = editor {
            editor.close();
        }
    }

    /// Lets go of everything kept about `key`, which is about to be unloaded: its
    /// GUI (which holds the instance's raw handle), its parameter window and its
    /// current preset.
    fn forget(&self, key: PluginKey) {
        self.close_ui(key);
        self.close_params(key);
        self.set_current_preset(key, None);
        let mut browser = self.browser.borrow_mut();
        if browser.as_ref().is_some_and(|browser| browser.key == key) {
            // Its rows stay in `AppState` until the next browser opens, but no
            // chain row shows them once the key is gone.
            *browser = None;
        }
    }

    /// Lets go of every plugin, for when the engine is about to reload or drop
    /// them all (keys don't survive that).
    fn forget_all(&self) {
        let editors: Vec<OpenEditor> = self.open_uis.borrow_mut().drain().map(|(_, e)| e).collect();
        for mut editor in editors {
            editor.close();
        }
        let keys: Vec<PluginKey> = self.param_windows.borrow().keys().copied().collect();
        for key in keys {
            self.close_params(key);
        }
        self.current_presets.borrow_mut().clear();
        *self.browser.borrow_mut() = None;
        // The plugins come back at their defaults, if at all.
        for slot in 0..SLOT_COUNT {
            self.mark_slot_changed(slot);
        }
    }

    /// Pumps open VST3 editors and CLAP GUIs, and forgets any GUI or parameter window that has
    /// closed, refreshing its slot.
    fn poll_uis(&self, engine: &Engine) {
        let mut closed = Vec::new();
        for (&key, editor) in self.open_uis.borrow_mut().iter_mut() {
            match editor {
                OpenEditor::Vst3(editor) => editor.idle(),
                OpenEditor::Clap(editor) => editor.idle(),
                OpenEditor::Lv2(_) => {}
            }
            if editor.is_closed() {
                closed.push(key);
            }
        }
        for key in &closed {
            self.open_uis.borrow_mut().remove(key);
        }
        self.param_windows.borrow_mut().retain(|&key, open| {
            let visible = open.window.window().is_visible();
            if !visible {
                closed.push(key);
            }
            visible
        });
        for key in closed {
            if let Some(slot) = slot_of(engine, key) {
                self.refresh_slot(engine, slot, None);
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut config = config::load();
    config.slots.resize_with(SLOT_COUNT, Default::default);
    let config = Rc::new(RefCell::new(config));
    // Set by edits that can fire continuously (a dragged fader), and saved by the
    // poll timer instead of writing the file on every step.
    let config_dirty = Rc::new(Cell::new(false));
    // Plugins are scanned once the window is up (see `Scanner`), so it shows at once.
    let engine = Rc::new(RefCell::new(Engine::unscanned(MAX_BLOCK_SIZE)));
    for (slot, slot_config) in config.borrow().slots.iter().enumerate() {
        let mut engine = engine.borrow_mut();
        engine.set_slot_filter(slot, slot_config.filter());
        engine.set_slot_gain(slot, slot_config.gain());
    }
    let midi_inputs = Rc::new(midi_inputs::MidiInputsUi::new());
    {
        let ports = engine.borrow_mut().set_midi_inputs(midi_input_devices(&config.borrow()));
        midi_inputs.set_ports(&ports, &config.borrow());
    }
    // Param changes from a plugin's own open GUI arrive as `(key, param_id, value)`
    // — on its hosting thread for LV2, on this thread for VST3; they're forwarded
    // here and applied to the engine from this (the control) thread by the poll
    // timer below, since only one thread may drive the audio engine's command
    // queue.
    let (edit_tx, edit_rx) = mpsc::channel::<GuiEdit>();
    let rack = Rc::new(Rack::new());
    let picker = Rc::new(Picker::new());

    let app = AppWindow::new()?;
    let state = app.global::<AppState>();

    let audio_devices: Vec<String> = std::iter::once(SYSTEM_DEFAULT_LABEL.to_string())
        .chain(list_logical_output_devices().unwrap_or_default())
        .collect();
    state.set_selected_audio_index(match config.borrow().audio_device.as_deref() {
        None => 0,
        Some(selected) => index_of(&audio_devices, Some(selected)).max(0),
    });
    state.set_audio_devices(string_model(audio_devices));

    state.set_midi_inputs(ModelRc::from(midi_inputs.rows.clone()));
    state.set_scene_rows(ModelRc::from(rack.scene_rows.clone()));
    state.set_scene_form_slots(ModelRc::from(rack.scene_form_slots.clone()));
    state.set_max_midi_inputs(MAX_MIDI_INPUTS as i32);

    state.set_default_plugin_folders(string_model(default_plugin_search_paths()));
    state.set_slots(ModelRc::from(rack.slots.clone()));
    state.set_max_midi_effects(MAX_MIDI_EFFECTS as i32);
    state.set_max_effects(MAX_EFFECTS as i32);
    state.set_picker_rows(ModelRc::from(picker.rows.clone()));
    state.set_preset_rows(ModelRc::from(rack.preset_rows.clone()));
    refresh_plugin_folders(&app, &config.borrow());
    refresh_plugin_list(&app, &engine.borrow(), &picker);
    rack.refresh_all(&engine.borrow());
    rack.set_all_routing(&config.borrow());
    state.set_min_volume_db(config::MIN_VOLUME_DB);
    state.set_max_volume_db(config::MAX_VOLUME_DB);
    state.set_max_transpose(config::MAX_TRANSPOSE);

    let logic = app.global::<AppLogic>();

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let app_weak = app.as_weak();
        logic.on_select_audio_device(move |value| {
            config.borrow_mut().audio_device = if value == SYSTEM_DEFAULT_LABEL {
                None
            } else {
                Some(value.to_string())
            };
            log_on_err(config::save(&config.borrow()));
            if let Some(app) = app_weak.upgrade() {
                restart_engine(&engine, &config, &rack, &app.global::<AppLogic>());
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let midi_inputs = midi_inputs.clone();
        logic.on_add_midi_input(move || {
            let mut config = config.borrow_mut();
            if config.midi_inputs.len() >= MAX_MIDI_INPUTS {
                return;
            }
            config.midi_inputs.push(Default::default());
            // Nothing listens to a new input until it's switched on in a slot.
            let bit = 1 << (config.midi_inputs.len() - 1);
            for slot_config in &mut config.slots {
                slot_config.midi_inputs &= !bit;
            }
            apply_midi_inputs(&mut engine.borrow_mut(), &config, &rack, &midi_inputs);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let midi_inputs = midi_inputs.clone();
        logic.on_remove_midi_input(move |input| {
            let input = input as usize;
            let mut config = config.borrow_mut();
            // The first input always stays.
            if input == 0 || input >= config.midi_inputs.len() {
                return;
            }
            config.midi_inputs.remove(input);
            for (slot, slot_config) in config.slots.iter_mut().enumerate() {
                let mask = midi_inputs::remove_input_bit(slot_config.midi_inputs, input);
                if mask != slot_config.midi_inputs {
                    rack.mark_scene_changed(slot);
                }
                slot_config.midi_inputs = mask;
            }
            midi_inputs.remove(input, &config);
            apply_midi_inputs(&mut engine.borrow_mut(), &config, &rack, &midi_inputs);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let midi_inputs = midi_inputs.clone();
        logic.on_toggle_midi_device(move |input, device| {
            let mut config = config.borrow_mut();
            let Some(input) = config.midi_inputs.get_mut(input as usize) else {
                return;
            };
            match input.devices.iter().position(|name| *name == device.as_str()) {
                Some(index) => {
                    input.devices.remove(index);
                }
                None => input.devices.push(device.to_string()),
            }
            apply_midi_inputs(&mut engine.borrow_mut(), &config, &rack, &midi_inputs);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_toggle_slot_input(move |slot, input| {
            let mut config = config.borrow_mut();
            let input_count = config.midi_inputs.len();
            let Some(slot_config) = config.slots.get_mut(slot as usize) else {
                return;
            };
            if !(0..input_count as i32).contains(&input) {
                return;
            }
            slot_config.midi_inputs ^= 1 << input;
            rack.mark_scene_changed(slot as usize);
            engine.borrow_mut().set_slot_filter(slot as usize, slot_config.filter());
            rack.set_routing(slot as usize, slot_config, input_count);
            log_on_err(config::save(&config));
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        logic.on_add_plugin_folder(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                return;
            };
            config.borrow_mut().extra_plugin_dirs.push(folder);
            log_on_err(config::save(&config.borrow()));
            refresh_plugin_folders(&app, &config.borrow());
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        logic.on_remove_plugin_folder(move |folder| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            config
                .borrow_mut()
                .extra_plugin_dirs
                .retain(|dir| dir.display().to_string() != folder.as_str());
            log_on_err(config::save(&config.borrow()));
            refresh_plugin_folders(&app, &config.borrow());
        });
    }

    // Whether startup is done: the first scan installed and the last session
    // restored (or not, see `restored`). Until then the rack is empty, and saving
    // the session would overwrite the last one with that.
    let started = Rc::new(Cell::new(false));
    // Whether the last session was restored, which happens once the first scan
    // is done (it needs the plugins).
    let restored = Rc::new(Cell::new(false));
    let scanner = Rc::new(Scanner::default());
    {
        let scanner_weak = Rc::downgrade(&scanner);
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let picker = picker.clone();
        let restored = restored.clone();
        let started = started.clone();
        scanner.timer.start(slint::TimerMode::Repeated, SCAN_TICK, move || {
            let (Some(scanner), Some(app)) = (scanner_weak.upgrade(), app_weak.upgrade()) else {
                return;
            };
            let deadline = Instant::now() + SCAN_SLICE;
            let done = match scanner.scan.borrow_mut().as_mut() {
                Some(scan) => loop {
                    if scan.step() {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                },
                None => true,
            };
            if !done {
                return;
            }
            scanner.timer.stop();
            let Some(scan) = scanner.scan.take() else { return };
            // Installing unloads every plugin, invalidating any open plugin GUI's
            // instance handle.
            rack.forget_all();
            engine.borrow_mut().install_plugins(scan);
            refresh_plugin_list(&app, &engine.borrow(), &picker);
            // The picker may be open, waiting for the list.
            picker.refilter(&config.borrow().pinned_plugins);
            rack.refresh_all(&engine.borrow());
            app.global::<AppState>().set_scanning(false);
            if !started.get() {
                restored.set(restore_session(&engine, &config, &rack, &app.global::<AppLogic>()));
                started.set(true);
                app.global::<AppState>().set_starting(false);
            }
        });
    }
    scanner.start(&app, &config.borrow().extra_plugin_dirs);
    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let scanner = scanner.clone();
        logic.on_rescan_plugins(move || {
            if let Some(app) = app_weak.upgrade() {
                scanner.start(&app, &config.borrow().extra_plugin_dirs);
            }
        });
    }

    {
        let config = config.clone();
        let picker = picker.clone();
        logic.on_filter_picker(move |category, query| {
            picker.filter(kind_from(category), &query, &config.borrow().pinned_plugins);
        });
    }

    {
        let config = config.clone();
        let picker = picker.clone();
        logic.on_toggle_pin(move |plugin_id| {
            let mut config = config.borrow_mut();
            toggle_pinned(&mut config.pinned_plugins, &plugin_id);
            picker.refilter(&config.pinned_plugins);
            log_on_err(config::save(&config));
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_pick_plugin(move |slot, category, plugin_id| {
            let slot = slot as usize;
            let kind = kind_from(category);
            if kind == PluginKind::Instrument {
                // The slot's current instrument is about to be replaced, and an open
                // GUI holds its raw instance handle.
                let old_key = engine
                    .borrow()
                    .slot(slot)
                    .and_then(|chain| chain.instrument.as_ref())
                    .map(|meta| meta.key);
                if let Some(old_key) = old_key {
                    rack.forget(old_key);
                }
            }
            let result = with_running_engine(&engine, &config.borrow(), |engine| match kind {
                PluginKind::Instrument => engine.load_instrument(slot, &plugin_id),
                PluginKind::MidiEffect => engine.add_effect(slot, Section::Midi, &plugin_id),
                PluginKind::AudioEffect => engine.add_effect(slot, Section::Audio, &plugin_id),
            });
            let error = match result {
                Ok(_) => {
                    rack.session_changed();
                    rack.mark_slot_changed(slot);
                    None
                }
                Err(err) => {
                    log::error!("failed to load {} '{plugin_id}' into slot {slot}: {err:#}", kind.label());
                    Some(format!("loading failed: {err}"))
                }
            };
            rack.refresh_slot(&engine.borrow(), slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_unload_plugin(move |slot| {
            let slot = slot as usize;
            let key = engine
                .borrow()
                .slot(slot)
                .and_then(|chain| chain.instrument.as_ref())
                .map(|meta| meta.key);
            if let Some(key) = key {
                rack.forget(key);
            }
            engine.borrow_mut().unload_instrument(slot);
            rack.session_changed();
            rack.mark_slot_changed(slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_remove_effect(move |slot, section, index| {
            let (slot, section, index) = (slot as usize, section_from(section), index as usize);
            let key = engine
                .borrow()
                .slot(slot)
                .and_then(|chain| chain.effects(section).get(index))
                .map(|meta| meta.key);
            if let Some(key) = key {
                rack.forget(key);
            }
            engine.borrow_mut().remove_effect(slot, section, index);
            rack.session_changed();
            rack.mark_slot_changed(slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_move_effect(move |slot, section, from, to| {
            let slot = slot as usize;
            engine
                .borrow_mut()
                .move_effect(slot, section_from(section), from as usize, to as usize);
            rack.session_changed();
            rack.mark_slot_changed(slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_toggle_params(move |key| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let key = key as PluginKey;
            let engine = engine.borrow();
            let Some(slot) = slot_of(&engine, key) else {
                return;
            };
            let mut error = None;
            if rack.param_windows.borrow().contains_key(&key) {
                rack.close_params(key);
            } else if let Err(err) = rack.open_params(&app, &engine, key) {
                log::error!("failed to open a parameter window: {err:#}");
                error = Some(format!("failed to open the parameters: {err}"));
            }
            rack.refresh_slot(&engine, slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_plugin_gui(move |key| {
            let key = key as PluginKey;
            if rack.open_uis.borrow().contains_key(&key) {
                return;
            }
            let engine = engine.borrow();
            let (Some(meta), Some(slot)) = (engine.plugin(key), slot_of(&engine, key)) else {
                return;
            };
            // Sends fail only once the poll timer's receiver is gone, i.e. the app
            // is already shutting down — nothing useful to do about it, and this
            // can fire rapidly (e.g. a dragged knob), so it isn't logged.
            let edit_tx_vst3 = edit_tx.clone();
            let forward_edit = move |param_id: u32, value: f32| {
                let _ = edit_tx_vst3.send(GuiEdit::Param { key, param_id, value });
            };
            let edit_tx_lv2 = edit_tx.clone();
            let forward_port_write = move |message: PortMessage| {
                let edit = match message {
                    PortMessage::Control { port, value } => GuiEdit::Param { key, param_id: port, value },
                    PortMessage::Atom { port, atom } => GuiEdit::Atom { key, port, atom },
                };
                let _ = edit_tx_lv2.send(edit);
            };
            let opened = match &meta.editor {
                PluginEditor::Lv2 {
                    plugin,
                    features,
                    raw_handle,
                    gui_leases,
                    ..
                } => launch_ui(LaunchUiRequest {
                    plugin,
                    features: features.clone(),
                    instance_handle: *raw_handle,
                    port_write: Box::new(forward_port_write),
                    keep_alive: gui_leases.lease().map(|lease| Box::new(lease) as Box<dyn Send>),
                })
                .map(|handle| {
                    // Hosts tell a new LV2 GUI every control port's value; a GUI
                    // without instance access has no other way to learn them.
                    for param in &meta.params {
                        let value = engine.param_value(key, param.id).unwrap_or(param.default_value);
                        handle.notify(PortMessage::Control { port: param.id, value });
                    }
                    OpenEditor::Lv2(handle)
                }),
                PluginEditor::Vst3(controller) => {
                    Vst3Editor::open(controller, &meta.name, Box::new(forward_edit))
                        .map(OpenEditor::Vst3)
                }
                // Its edits come through `Engine::idle_plugins`.
                PluginEditor::Clap(controller) => ClapEditor::open(controller, &meta.name).map(OpenEditor::Clap),
            };
            let error = match opened {
                Ok(editor) => {
                    rack.open_uis.borrow_mut().insert(key, editor);
                    None
                }
                Err(err) => {
                    log::error!("failed to open GUI of '{}': {err:#}", meta.name);
                    Some(format!("failed to open {}'s GUI: {err}", meta.name))
                }
            };
            rack.refresh_slot(&engine, slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_close_plugin_gui(move |key| {
            let key = key as PluginKey;
            rack.close_ui(key);
            let engine = engine.borrow();
            if let Some(slot) = slot_of(&engine, key) {
                rack.refresh_slot(&engine, slot, None);
            }
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_param(move |key, param_id, value| {
            let (key, param_id) = (key as PluginKey, param_id as u32);
            // Stepped kinds snap here, so a slider dragged between steps only sends
            // when it crosses into the next one.
            let value = engine
                .borrow()
                .plugin(key)
                .and_then(|meta| meta.params.iter().find(|param| param.id == param_id))
                .map_or(value, |param| param.snap(value));
            if engine.borrow().param_value(key, param_id) == Some(value) {
                return;
            }
            engine.borrow_mut().set_param(key, param_id, value);
            rack.show_param_value(&engine.borrow(), key, param_id, value);
            rack.notify_ui(key, param_id, value);
            rack.mark_changed(&engine.borrow(), key);
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_preset_browser(move |key| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let pinned = &config.borrow().pinned_presets;
            rack.open_browser(&app, &engine.borrow(), key as PluginKey, pinned);
        });
    }

    {
        let app_weak = app.as_weak();
        let rack = rack.clone();
        logic.on_close_preset_browser(move || {
            if let Some(app) = app_weak.upgrade() {
                rack.close_browser(&app);
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let rack = rack.clone();
        logic.on_filter_presets(move |query| {
            if let Some(app) = app_weak.upgrade() {
                rack.filter_browser(&app, &query, &config.borrow().pinned_presets);
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let rack = rack.clone();
        logic.on_toggle_preset_pin(move |index| {
            let (Some(app), Some((_, preset))) = (app_weak.upgrade(), rack.browser_preset(index))
            else {
                return;
            };
            let mut config = config.borrow_mut();
            toggle_pinned(&mut config.pinned_presets, &preset.id);
            rack.show_browser(&app, &config.pinned_presets);
            log_on_err(config::save(&config));
        });
    }

    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_load_preset(move |index| {
            let (Some(app), Some((key, preset))) = (app_weak.upgrade(), rack.browser_preset(index))
            else {
                return;
            };
            rack.close_browser(&app);
            let result = engine.borrow_mut().load_preset(key, &preset.id);
            let engine = engine.borrow();
            let error = match result {
                Ok(()) => {
                    rack.set_current_preset(key, Some(preset));
                    rack.refresh_param_rows(&engine, key);
                    if let Some(slot) = slot_of(&engine, key) {
                        rack.mark_slot_changed(slot);
                    }
                    None
                }
                Err(err) => {
                    log::error!("failed to load preset '{}': {err:#}", preset.name);
                    Some(format!("Loading \"{}\" failed: {err}", preset.name))
                }
            };
            if let Some(slot) = slot_of(&engine, key) {
                rack.refresh_slot(&engine, slot, error);
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_save_preset(move |key, name| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let key = key as PluginKey;
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let result = engine.borrow_mut().save_preset(key, name);
            let engine = engine.borrow();
            let error = match result {
                Ok(preset) => {
                    rack.set_current_preset(key, Some(preset));
                    None
                }
                Err(err) => {
                    log::error!("failed to save preset '{name}': {err:#}");
                    Some(format!("Saving preset \"{name}\" failed: {err}"))
                }
            };
            rack.reread_browser(&app, &engine, &config.borrow().pinned_presets);
            if let Some(slot) = slot_of(&engine, key) {
                rack.refresh_slot(&engine, slot, error);
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_delete_preset(move |key| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let key = key as PluginKey;
            let Some(preset) = rack.current_preset(key) else {
                return;
            };
            let result = engine.borrow_mut().delete_preset(key, &preset.id);
            let engine = engine.borrow();
            let error = match result {
                Ok(()) => {
                    rack.set_current_preset(key, None);
                    let mut config = config.borrow_mut();
                    if config.pinned_presets.contains(&preset.id) {
                        config.pinned_presets.retain(|id| *id != preset.id);
                        log_on_err(config::save(&config));
                    }
                    None
                }
                Err(err) => {
                    log::error!("failed to delete preset '{}': {err:#}", preset.name);
                    Some(format!("Deleting \"{}\" failed: {err}", preset.name))
                }
            };
            rack.reread_browser(&app, &engine, &config.borrow().pinned_presets);
            if let Some(slot) = slot_of(&engine, key) {
                rack.refresh_slot(&engine, slot, error);
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_choose_slot_preset(move |slot, index| {
            let slot = slot as usize;
            let entry = usize::try_from(index)
                .ok()
                .and_then(|index| rack.slot_entries(slot).into_iter().nth(index));
            let error = match entry {
                Some(SlotEntry::Folder { instrument, .. }) => {
                    rack.show_slot_folder(slot, SlotFolder::Of(instrument));
                    None
                }
                Some(SlotEntry::Preset(index)) => {
                    let stored = rack.slot_store.borrow()[index].clone();
                    let errors = load_slot_preset(&engine, &config, &rack, slot, &stored);
                    rack.mark_scene_changed(slot);
                    (!errors.is_empty()).then(|| errors.join("\n"))
                }
                None => None,
            };
            rack.refresh_slot(&engine.borrow(), slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_show_slot_preset_folders(move |slot| {
            rack.show_slot_folder(slot as usize, SlotFolder::All);
            rack.refresh_slot(&engine.borrow(), slot as usize, None);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_save_slot_preset(move |slot, name| {
            let slot = slot as usize;
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let result = slot_presets::save(&mut engine.borrow_mut(), slot, name, |key| {
                rack.plugin_preset_state(key)
            });
            let error = match result {
                Ok(stored) => {
                    rack.set_current_slot_preset(
                        slot,
                        Some(CurrentSlotPreset {
                            dir: stored.dir,
                            name: stored.preset.name,
                            changed: false,
                        }),
                    );
                    None
                }
                Err(err) => {
                    log::error!("failed to save slot preset '{name}': {err:#}");
                    Some(format!("Saving slot preset \"{name}\" failed: {err:#}"))
                }
            };
            rack.reload_slot_store();
            // Other slots may list the same folder.
            rack.refresh_all(&engine.borrow());
            rack.refresh_slot(&engine.borrow(), slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_delete_slot_preset(move |slot| {
            let slot = slot as usize;
            let Some((dir, name)) = rack.current_slot_presets.borrow()[slot]
                .as_ref()
                .map(|current| (current.dir.clone(), current.name.clone()))
            else {
                return;
            };
            let error = match slot_presets::delete(&dir) {
                Ok(()) => {
                    for other in 0..SLOT_COUNT {
                        let deleted = rack.current_slot_presets.borrow()[other]
                            .as_ref()
                            .is_some_and(|current| current.dir == dir);
                        if deleted {
                            rack.set_current_slot_preset(other, None);
                        }
                    }
                    None
                }
                Err(err) => {
                    log::error!("failed to delete slot preset '{name}': {err:#}");
                    Some(format!("Deleting slot preset \"{name}\" failed: {err:#}"))
                }
            };
            rack.reload_slot_store();
            rack.refresh_all(&engine.borrow());
            rack.refresh_slot(&engine.borrow(), slot, error);
        });
    }

    {
        let rack = rack.clone();
        logic.on_filter_scenes(move |query| {
            *rack.scene_query.borrow_mut() = query.to_string();
            rack.scenes_dirty.set(true);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_load_scene(move |index| {
            if let Some(stored) = rack.scene_at(index) {
                load_scene(&engine, &config, &rack, &stored);
            }
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_begin_scene_save(move || {
            let current = rack.current_scene_stored();
            *rack.scene_target.borrow_mut() = None;
            rack.scene_status.borrow_mut().clear();
            *rack.scene_target_name.borrow_mut() =
                current.as_ref().map(|stored| stored.scene.name.clone()).unwrap_or_default();
            rack.scenes_dirty.set(true);
            let choices = (0..SLOT_COUNT)
                .map(|slot| {
                    let mode = match &current {
                        Some(stored) => stored.scene.mode(slot),
                        None if engine.borrow().slot(slot).is_some_and(|chain| chain.plugins().next().is_some()) => {
                            scenes::SlotMode::Restore
                        }
                        None => scenes::SlotMode::Leave,
                    };
                    // Saving stores whatever the slot holds now.
                    SceneSlotChoice {
                        mode: mode.index(),
                        can_restore: true,
                    }
                })
                .collect();
            sync_model(&rack.scene_form_slots, choices);
        });
    }

    {
        let rack = rack.clone();
        logic.on_begin_scene_edit(move |index| {
            let Some(stored) = rack.scene_at(index) else {
                return;
            };
            rack.target_scene(&stored);
            let choices = (0..SLOT_COUNT)
                .map(|slot| SceneSlotChoice {
                    mode: stored.scene.mode(slot).index(),
                    can_restore: stored.scene.can_restore(slot),
                })
                .collect();
            sync_model(&rack.scene_form_slots, choices);
        });
    }

    {
        let rack = rack.clone();
        logic.on_begin_scene_delete(move |index| {
            if let Some(stored) = rack.scene_at(index) {
                rack.target_scene(&stored);
            }
        });
    }

    {
        let rack = rack.clone();
        logic.on_set_scene_slot_mode(move |slot, mode| {
            let slot = slot as usize;
            if let Some(mut choice) = rack.scene_form_slots.row_data(slot)
                && (mode != scenes::SlotMode::Restore.index() || choice.can_restore)
            {
                choice.mode = mode;
                rack.scene_form_slots.set_row_data(slot, choice);
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_save_scene(move |name| {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let modes: Vec<scenes::SlotMode> = rack
                .scene_form_slots
                .iter()
                .map(|choice| scenes::SlotMode::from_index(choice.mode))
                .collect();
            // Saving under an existing scene's name overwrites it.
            let folder = slot_presets::file_name(name);
            let previous = rack
                .scene_store
                .borrow()
                .iter()
                .find(|stored| stored.dir.file_name().is_some_and(|other| other.to_string_lossy() == folder))
                .cloned();
            let result = save_scene(&engine, &config, &rack, name, &modes, previous.as_ref());
            rack.after_scene_change("Saving", name, result);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_overwrite_scene(move |index| {
            let Some(stored) = rack.scene_at(index) else {
                return;
            };
            let modes: Vec<scenes::SlotMode> = (0..SLOT_COUNT).map(|slot| stored.scene.mode(slot)).collect();
            let name = stored.scene.name.clone();
            let result = save_scene(&engine, &config, &rack, &name, &modes, Some(&stored));
            rack.after_scene_change("Saving", &name, result);
        });
    }

    {
        let rack = rack.clone();
        logic.on_edit_scene(move |name| {
            let name = name.trim();
            let target = rack.scene_target.borrow().clone();
            let Some(stored) = target.and_then(|dir| rack.stored_scene(&dir)) else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let modes: Vec<scenes::SlotMode> = rack
                .scene_form_slots
                .iter()
                .map(|choice| scenes::SlotMode::from_index(choice.mode))
                .collect();
            let result = scenes::update(&stored, name, &modes).map(|updated| {
                let mut current = rack.current_scene.borrow_mut();
                if let Some(current) = current.as_mut().filter(|current| current.dir == stored.dir) {
                    *current = CurrentScene::of(&updated, current.changed);
                }
            });
            rack.after_scene_change("Changing", &stored.scene.name, result);
        });
    }

    {
        let rack = rack.clone();
        logic.on_delete_scene(move || {
            let target = rack.scene_target.borrow().clone();
            let Some(stored) = target.and_then(|dir| rack.stored_scene(&dir)) else {
                return;
            };
            let result = scenes::delete(&stored.dir).map(|()| {
                let mut current = rack.current_scene.borrow_mut();
                if current.as_ref().is_some_and(|current| current.dir == stored.dir) {
                    *current = None;
                }
            });
            rack.after_scene_change("Deleting", &stored.scene.name, result);
        });
    }

    {
        let rack = rack.clone();
        logic.on_duplicate_scene(move |index| {
            if let Some(stored) = rack.scene_at(index) {
                let result = scenes::duplicate(&stored).map(drop);
                rack.after_scene_change("Duplicating", &stored.scene.name, result);
            }
        });
    }

    {
        let rack = rack.clone();
        logic.on_move_scene(move |from, to| {
            let (Some(moved), Some(target)) = (rack.scene_at(from), rack.scene_at(to)) else {
                return;
            };
            let Some(position) = rack.scene_store.borrow().iter().position(|stored| stored.dir == target.dir) else {
                return;
            };
            let result = scenes::move_to(&moved.dir, position);
            rack.after_scene_change("Moving", &moved.scene.name, result);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_clear_slot(move |slot| {
            let slot = slot as usize;
            // Keep what's there, so Undo can bring it back.
            let name = engine.borrow().slot(slot).and_then(|chain| {
                let instrument = chain.instrument.as_ref().map(|meta| meta.name.clone());
                instrument.or_else(|| chain.plugins().next().map(|meta| meta.name.clone()))
            });
            let closed = name.and_then(|name| {
                let dir = slot_view::snapshot(&mut engine.borrow_mut(), slot, |key| rack.plugin_preset_state(key));
                match dir {
                    Ok(dir) => Some(slot_view::Closed {
                        name,
                        dir,
                        routing: config.borrow().slots[slot].routing(),
                        slot_preset: rack.current_slot_presets.borrow()[slot]
                            .as_ref()
                            .map(|current| (current.dir.clone(), current.name.clone(), current.changed)),
                    }),
                    Err(err) => {
                        log::error!("keeping slot {} for undo failed: {err:#}", slot + 1);
                        None
                    }
                }
            });
            clear_slot(&engine, &config, &rack, slot);
            rack.mark_slot_changed(slot);
            rack.closed.borrow_mut()[slot] = closed;
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_undo_clear(move |slot| {
            let slot = slot as usize;
            let Some(closed) = rack.closed.borrow_mut()[slot].take() else {
                return;
            };
            let errors = match slot_presets::read_dir(&closed.dir) {
                Ok(stored) => load_slot_preset(&engine, &config, &rack, slot, &stored),
                Err(err) => vec![format!("The cleared slot couldn't be read back: {err:#}")],
            };
            {
                let mut config = config.borrow_mut();
                config.slots[slot].set_routing(closed.routing);
                let slot_config = &config.slots[slot];
                let mut engine = engine.borrow_mut();
                engine.set_slot_filter(slot, slot_config.filter());
                engine.set_slot_gain(slot, slot_config.gain());
                rack.set_routing(slot, slot_config, config.midi_inputs.len());
                log_on_err(config::save(&config));
            }
            rack.set_current_slot_preset(
                slot,
                closed.slot_preset.map(|(dir, name, changed)| CurrentSlotPreset {
                    dir,
                    name,
                    changed: changed || !errors.is_empty(),
                }),
            );
            rack.mark_scene_changed(slot);
            rack.refresh_slot(&engine.borrow(), slot, (!errors.is_empty()).then(|| errors.join("\n")));
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_dismiss_closed(move |slot| {
            let slot = slot as usize;
            rack.closed.borrow_mut()[slot] = None;
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let config = config.clone();
        let config_dirty = config_dirty.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_slot_transpose(move |slot, semitones| {
            let slot = slot as usize;
            let mut config = config.borrow_mut();
            let slot_config = &mut config.slots[slot];
            let transpose = semitones.clamp(-config::MAX_TRANSPOSE, config::MAX_TRANSPOSE) as i8;
            if slot_config.transpose == transpose {
                return;
            }
            slot_config.transpose = transpose;
            engine.borrow_mut().set_slot_filter(slot, slot_config.filter());
            rack.mark_scene_changed(slot);
            let input_count = config.midi_inputs.len();
            rack.set_routing(slot, &config.slots[slot], input_count);
            config_dirty.set(true);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_toggle_slot_mute(move |slot| {
            let slot = slot as usize;
            let mut config = config.borrow_mut();
            let slot_config = &mut config.slots[slot];
            slot_config.muted = !slot_config.muted;
            engine.borrow_mut().set_slot_gain(slot, slot_config.gain());
            rack.mark_scene_changed(slot);
            let input_count = config.midi_inputs.len();
            rack.set_routing(slot, &config.slots[slot], input_count);
            log_on_err(config::save(&config));
        });
    }

    {
        let config = config.clone();
        let config_dirty = config_dirty.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_slot_filter(
            move |slot, channel, note_low, note_high, velocity_low, velocity_high| {
                let slot = slot as usize;
                let mut config = config.borrow_mut();
                let slot_config = &mut config.slots[slot];
                let before = slot_config.routing();
                // Channel 0 is omni; note/velocity values come from sliders already
                // bounded to 0..=127.
                slot_config.midi_channel = (1..=16).contains(&channel).then(|| channel as u8 - 1);
                slot_config.note_low = note_low.clamp(0, 127) as u8;
                slot_config.note_high = note_high.clamp(0, 127) as u8;
                slot_config.velocity_low = velocity_low.clamp(0, 127) as u8;
                slot_config.velocity_high = velocity_high.clamp(0, 127) as u8;
                engine.borrow_mut().set_slot_filter(slot, slot_config.filter());
                if slot_config.routing() != before {
                    rack.mark_scene_changed(slot);
                }
                let input_count = config.midi_inputs.len();
                rack.set_routing(slot, &config.slots[slot], input_count);
                config_dirty.set(true);
            },
        );
    }

    {
        let config = config.clone();
        let config_dirty = config_dirty.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_slot_volume(move |slot, volume_db| {
            let slot = slot as usize;
            let mut config = config.borrow_mut();
            let slot_config = &mut config.slots[slot];
            let volume_db = volume_db.clamp(config::MIN_VOLUME_DB, config::MAX_VOLUME_DB);
            if slot_config.volume_db != volume_db {
                rack.mark_scene_changed(slot);
            }
            slot_config.volume_db = volume_db;
            engine.borrow_mut().set_slot_gain(slot, slot_config.gain());
            let input_count = config.midi_inputs.len();
            rack.set_routing(slot, &config.slots[slot], input_count);
            config_dirty.set(true);
        });
    }

    let poll_timer = slint::Timer::default();
    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let app_weak = app.as_weak();
        poll_timer.start(slint::TimerMode::Repeated, UI_POLL_INTERVAL, move || {
            rack.poll_uis(&engine.borrow());
            if rack.scenes_dirty.replace(false)
                && let Some(app) = app_weak.upgrade()
            {
                rack.refresh_scenes(&app);
            }
            // CLAP plugins' main-thread work, and their GUIs' edits.
            let mut edits = Vec::new();
            let reread = engine.borrow_mut().idle_plugins(|key, param_id, value| {
                edits.push(GuiEdit::Param { key, param_id, value });
            });
            for key in reread {
                rack.refresh_param_rows(&engine.borrow(), key);
            }
            edits.extend(edit_rx.try_iter());
            for edit in edits {
                match edit {
                    GuiEdit::Param { key, param_id, value } => {
                        // Plugin GUIs also echo values back (e.g. every value when
                        // opening); only an actual change counts as an edit.
                        if engine.borrow().param_value(key, param_id) == Some(value) {
                            continue;
                        }
                        engine.borrow_mut().send_param_change(key, param_id, value);
                        rack.show_param_value(&engine.borrow(), key, param_id, value);
                        rack.mark_changed(&engine.borrow(), key);
                    }
                    // Settings that only travel as atoms (setBfree's) aren't
                    // recorded, so they don't mark presets changed.
                    GuiEdit::Atom { key, port, atom } => engine.borrow_mut().send_ui_atom(key, port, &atom),
                }
            }
            // What plugins wrote to their atom outputs goes to their open GUIs
            // (setBfree's state and lit keys); for others it's dropped.
            {
                let open_uis = rack.open_uis.borrow();
                engine.borrow_mut().drain_ui_events(|key, port, event| {
                    if let Some(OpenEditor::Lv2(handle)) = open_uis.get(&key) {
                        handle.notify(PortMessage::Atom { port, atom: event.to_atom() });
                    }
                });
            }
            engine.borrow_mut().collect_retired();
            if config_dirty.replace(false) {
                log_on_err(config::save(&config.borrow()));
            }
        });
    }

    let midi_port_timer = slint::Timer::default();
    {
        let config = config.clone();
        let engine = engine.clone();
        midi_port_timer.start(slint::TimerMode::Repeated, MIDI_PORT_POLL_INTERVAL, move || {
            let ports = engine.borrow_mut().sync_midi_inputs();
            midi_inputs.set_ports(&ports, &config.borrow());
        });
    }

    // Saves the session after a change, once every slot has been quiet for
    // `AUTOSAVE_QUIET` (saving can briefly silence a plugin).
    let autosave_timer = slint::Timer::default();
    {
        let engine = engine.clone();
        let rack = rack.clone();
        let started = started.clone();
        let mut last_sound = Instant::now();
        autosave_timer.start(slint::TimerMode::Repeated, AUTOSAVE_POLL_INTERVAL, move || {
            let loud = (0..SLOT_COUNT).any(|slot| engine.borrow_mut().take_slot_peak(slot) > SILENCE);
            if loud {
                last_sound = Instant::now();
            }
            let Some(since) = rack.session_changed_since.get().filter(|_| started.get()) else {
                return;
            };
            if since.elapsed() >= AUTOSAVE_DELAY && last_sound.elapsed() >= AUTOSAVE_QUIET {
                save_session(&engine, &rack);
            }
        });
    }

    // Closing the main window quits, even with parameter windows still open.
    app.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    app.run()?;
    drop(autosave_timer);
    if let Some(path) = mock_data::requested_path() {
        write_mock_data(&app, &engine.borrow(), &rack, &path);
    }
    // Always on a normal exit (some plugin settings change without the host
    // seeing it), unless startup didn't finish, or the last session wasn't
    // restored and nothing has happened since: either would overwrite it with
    // an empty rack.
    if started.get() && (restored.get() || rack.session_changed_since.get().is_some()) {
        save_session(&engine, &rack);
    }
    Ok(())
}

/// Recalls slot preset `stored` into `slot`: makes the chain match (keeping
/// plugins that are already in place, so switching between presets of the same
/// instrument doesn't reload it), then restores each plugin's settings. Returns
/// what went wrong (e.g. a plugin that's no longer installed); the rest is
/// recalled anyway.
fn load_slot_preset(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    slot: usize,
    stored: &slot_presets::Stored,
) -> Vec<String> {
    let preset = &stored.preset;
    let mut errors = Vec::new();

    let current_instrument = engine
        .borrow()
        .slot(slot)
        .and_then(|chain| chain.instrument.as_ref().map(|meta| (meta.key, meta.id.clone())));
    let wanted = preset.instrument.as_ref();
    if current_instrument.as_ref().map(|(_, id)| id.as_str()) != wanted.map(|plugin| plugin.id.as_str()) {
        if let Some((key, _)) = current_instrument {
            rack.forget(key);
        }
        match wanted {
            Some(plugin) => {
                let result = with_running_engine(engine, &config.borrow(), |engine| {
                    engine.load_instrument(slot, &plugin.id)
                });
                if let Err(err) = result {
                    log::error!("slot preset '{}': loading {} failed: {err:#}", preset.name, plugin.name);
                    errors.push(format!("{} couldn't be loaded: {err}", plugin.name));
                }
            }
            None => engine.borrow_mut().unload_instrument(slot),
        }
    }

    for section in [Section::Midi, Section::Audio] {
        let wanted = preset.effects(section);
        let current: Vec<(PluginKey, String)> = engine
            .borrow()
            .slot(slot)
            .map(|chain| chain.effects(section).iter().map(|meta| (meta.key, meta.id.clone())).collect())
            .unwrap_or_default();
        if current.iter().map(|(_, id)| id).eq(wanted.iter().map(|plugin| &plugin.id)) {
            continue;
        }
        for (key, _) in &current {
            rack.forget(*key);
            engine.borrow_mut().remove_effect(slot, section, 0);
        }
        for plugin in wanted {
            let result = with_running_engine(engine, &config.borrow(), |engine| {
                engine.add_effect(slot, section, &plugin.id)
            });
            if let Err(err) = result {
                log::error!("slot preset '{}': loading {} failed: {err:#}", preset.name, plugin.name);
                errors.push(format!("{} couldn't be loaded: {err}", plugin.name));
            }
        }
    }

    // Pair the chain's plugins with the preset's in order, skipping any the
    // preset has that failed to load.
    let mut pairs: Vec<(PluginKey, &slot_presets::PluginState)> = Vec::new();
    if let Some(chain) = engine.borrow().slot(slot) {
        if let (Some(meta), Some(plugin)) = (&chain.instrument, &preset.instrument)
            && meta.id == plugin.id
        {
            pairs.push((meta.key, plugin));
        }
        for section in [Section::Midi, Section::Audio] {
            let mut loaded = chain.effects(section).iter().peekable();
            for plugin in preset.effects(section) {
                if let Some(meta) = loaded.next_if(|meta| meta.id == plugin.id) {
                    pairs.push((meta.key, plugin));
                }
            }
        }
    }
    for (key, plugin) in pairs {
        let result = engine.borrow_mut().load_state_file(key, &stored.dir.join(&plugin.state));
        match result {
            Ok(()) => {
                let current = plugin.preset.as_ref().map(PresetInfo::from);
                rack.set_current_preset_changed(key, current, plugin.preset_changed);
                rack.refresh_param_rows(&engine.borrow(), key);
            }
            Err(err) => {
                log::error!("slot preset '{}': restoring {} failed: {err:#}", preset.name, plugin.name);
                errors.push(format!("{}'s settings couldn't be restored: {err}", plugin.name));
            }
        }
    }

    rack.session_changed();
    rack.set_current_slot_preset(
        slot,
        Some(CurrentSlotPreset {
            dir: stored.dir.clone(),
            name: preset.name.clone(),
            changed: !errors.is_empty(),
        }),
    );
    errors
}

/// Saves the slots as scene `name`, with `modes` per slot, and makes it the
/// current scene. `previous` is the scene being overwritten, if any.
fn save_scene(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    name: &str,
    modes: &[scenes::SlotMode],
    previous: Option<&scenes::Stored>,
) -> anyhow::Result<()> {
    let slots: Vec<scenes::SlotToSave> = modes
        .iter()
        .enumerate()
        .map(|(slot, &mode)| scenes::SlotToSave {
            slot,
            mode,
            routing: config.borrow().slots[slot].routing(),
            slot_preset: rack.current_slot_presets.borrow()[slot].as_ref().map(|current| scenes::SlotPresetRef {
                dir: current.dir.clone(),
                name: current.name.clone(),
                changed: current.changed,
            }),
        })
        .collect();
    let stored = scenes::save(&mut engine.borrow_mut(), name, &slots, previous, |key| {
        rack.plugin_preset_state(key)
    })?;
    *rack.current_scene.borrow_mut() = Some(CurrentScene::of(&stored, false));
    Ok(())
}

/// Recalls scene `stored`: each slot it restores gets the scene's chain (as
/// [`load_slot_preset`] does, so plugins already in place are kept), its
/// routing and volume; each slot it clears is emptied; the others are left
/// alone. What goes wrong is shown in the slot's status line; the rest is
/// recalled anyway.
fn load_scene(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    stored: &scenes::Stored,
) {
    let mut failed = false;
    for slot in 0..SLOT_COUNT {
        match stored.scene.mode(slot) {
            scenes::SlotMode::Leave => continue,
            scenes::SlotMode::Clear => {
                clear_slot(engine, config, rack, slot);
                rack.refresh_slot(&engine.borrow(), slot, None);
                continue;
            }
            scenes::SlotMode::Restore => {}
        }
        let Some((saved, chain)) = stored.saved(slot) else {
            continue;
        };
        let errors = match chain {
            Ok(chain) => load_slot_preset(engine, config, rack, slot, &chain),
            Err(err) => {
                log::error!("scene '{}': reading slot {}: {err:#}", stored.scene.name, slot + 1);
                vec![format!("Scene \"{}\" couldn't be read: {err:#}", stored.scene.name)]
            }
        };
        failed |= !errors.is_empty();
        // The library slot preset it showed, if that still exists.
        let slot_preset = saved
            .slot_preset
            .as_ref()
            .filter(|preset| preset.dir.is_dir())
            .map(|preset| CurrentSlotPreset {
                dir: preset.dir.clone(),
                name: preset.name.clone(),
                changed: preset.changed || !errors.is_empty(),
            });
        rack.set_current_slot_preset(slot, slot_preset);

        let mut config = config.borrow_mut();
        config.slots[slot].set_routing(saved.routing);
        let slot_config = &config.slots[slot];
        let mut engine = engine.borrow_mut();
        engine.set_slot_filter(slot, slot_config.filter());
        engine.set_slot_gain(slot, slot_config.gain());
        rack.set_routing(slot, slot_config, config.midi_inputs.len());
        rack.refresh_slot(&engine, slot, (!errors.is_empty()).then(|| errors.join("\n")));
    }
    log_on_err(config::save(&config.borrow()));
    *rack.current_scene.borrow_mut() = Some(CurrentScene::of(stored, failed));
    *rack.scene_status.borrow_mut() = String::new();
    rack.scenes_dirty.set(true);
    rack.session_changed();
}

/// Writes the window's state as the preview's mock data (see `mock_data`), with
/// an open parameter window's plugin, else the first loaded plugin, for
/// `ParamsWindow`.
fn write_mock_data(app: &AppWindow, engine: &Engine, rack: &Rack, path: &std::path::Path) {
    let params_window = match rack.param_windows.borrow().values().next() {
        Some(open) => (open.window.get_plugin_name(), ModelRc::from(open.rows.clone())),
        None => {
            let meta = (0..SLOT_COUNT).find_map(|slot| engine.slot(slot)?.plugins().next());
            let name = meta.map(|meta| meta.name.as_str()).unwrap_or_default().into();
            let rows = meta.map(|meta| param_rows(engine, meta.key)).unwrap_or_default();
            (name, ModelRc::new(VecModel::from(rows)))
        }
    };
    let text = mock_data::generate(app, params_window);
    match std::fs::write(path, text) {
        Ok(()) => log::info!("wrote the preview's mock data to {}", path.display()),
        Err(err) => log::error!("writing {} failed: {err}", path.display()),
    }
}

/// Saves the session: every slot, the current scene, and which plugin windows
/// are open. Lending LV2 plugins with state silences them meanwhile, so it's
/// done on exit and, after a change, once the rack has been quiet for a while.
/// Returns whether it was saved.
fn save_session(engine: &RefCell<Engine>, rack: &Rack) -> bool {
    rack.session_changed_since.set(None);
    let slots = (0..SLOT_COUNT)
        .map(|slot| {
            let engine = engine.borrow();
            let chain = engine.slot(slot);
            let positions = |keys: Vec<PluginKey>| -> Vec<session::Position> {
                keys.into_iter()
                    .filter_map(|key| chain?.position_of(key).map(session::Position::from))
                    .collect()
            };
            session::SlotToSave {
                slot_preset: rack.current_slot_presets.borrow()[slot].as_ref().map(|current| {
                    scenes::SlotPresetRef {
                        dir: current.dir.clone(),
                        name: current.name.clone(),
                        changed: current.changed,
                    }
                }),
                open_guis: positions(rack.open_uis.borrow().keys().copied().collect()),
                open_params: positions(rack.param_windows.borrow().keys().copied().collect()),
            }
        })
        .collect();
    let current_scene = rack.current_scene.borrow().as_ref().map(|current| session::CurrentSceneRef {
        dir: current.dir.clone(),
        changed: current.changed,
    });
    let result = session::save(&mut engine.borrow_mut(), slots, current_scene, |key| {
        rack.plugin_preset_state(key)
    });
    if let Err(err) = &result {
        log::error!("saving the session failed: {err:#}");
    }
    result.is_ok()
}

/// Restores the session saved last time, if any: each slot's chain and
/// settings, its slot preset, the current scene, and the plugin windows that
/// were open. Skipped if the last attempt never finished (a plugin crashed the
/// app during it). Returns whether it was restored (or there was nothing to
/// restore).
fn restore_session(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    logic: &AppLogic<'_>,
) -> bool {
    let Some((saved, chains)) = session::load() else {
        return true;
    };
    if !session::begin_restore() {
        log::error!("not restoring the last session: the app quit while restoring it last time");
        rack.refresh_slot(
            &engine.borrow(),
            0,
            Some("The last session wasn't restored: the app quit while restoring it last time.".into()),
        );
        session::end_restore();
        return false;
    }
    let mut windows = Vec::new();
    for (slot, chain) in chains {
        if slot >= SLOT_COUNT {
            continue;
        }
        let errors = match chain {
            Ok(chain) => load_slot_preset(engine, config, rack, slot, &chain),
            Err(err) => vec![format!("The last session's slot {} couldn't be read: {err:#}", slot + 1)],
        };
        let saved_slot = saved.slots.iter().find(|saved_slot| saved_slot.slot == slot);
        let slot_preset = saved_slot
            .and_then(|saved_slot| saved_slot.slot_preset.as_ref())
            .filter(|preset| preset.dir.is_dir())
            .map(|preset| CurrentSlotPreset {
                dir: preset.dir.clone(),
                name: preset.name.clone(),
                changed: preset.changed || !errors.is_empty(),
            });
        rack.set_current_slot_preset(slot, slot_preset);
        if let Some(saved_slot) = saved_slot {
            windows.push((slot, saved_slot.open_guis.clone(), saved_slot.open_params.clone()));
        }
        rack.refresh_slot(&engine.borrow(), slot, (!errors.is_empty()).then(|| errors.join("\n")));
    }
    if let Some(scene) = saved.current_scene
        && let Some(stored) = rack.stored_scene(&scene.dir)
    {
        *rack.current_scene.borrow_mut() = Some(CurrentScene::of(&stored, scene.changed));
        rack.scenes_dirty.set(true);
    }
    session::end_restore();

    // Reopen plugin windows once everything is loaded.
    for (slot, guis, params) in windows {
        let key_at = |position: session::Position| {
            engine.borrow().slot(slot).and_then(|chain| chain.at(position.into())).map(|meta| meta.key as i32)
        };
        for key in guis.into_iter().filter_map(key_at) {
            logic.invoke_open_plugin_gui(key);
        }
        for key in params.into_iter().filter_map(key_at) {
            logic.invoke_toggle_params(key);
        }
    }
    // Restoring isn't a change.
    rack.session_changed_since.set(None);
    true
}

/// Removes every plugin from `slot` and resets its MIDI routing and volume to
/// the defaults; it no longer shows a slot preset.
fn clear_slot(engine: &RefCell<Engine>, config: &RefCell<config::AppConfig>, rack: &Rack, slot: usize) {
    let keys: Vec<PluginKey> = engine
        .borrow()
        .slot(slot)
        .map(|chain| chain.plugins().map(|meta| meta.key).collect())
        .unwrap_or_default();
    for key in keys {
        rack.forget(key);
    }
    {
        let mut engine = engine.borrow_mut();
        engine.unload_instrument(slot);
        for section in [Section::Midi, Section::Audio] {
            let count = engine.slot(slot).map_or(0, |chain| chain.effects(section).len());
            for _ in 0..count {
                engine.remove_effect(slot, section, 0);
            }
        }
    }
    {
        let mut config = config.borrow_mut();
        config.slots[slot].set_routing(config::SlotConfig::default().routing());
        let slot_config = &config.slots[slot];
        let mut engine = engine.borrow_mut();
        engine.set_slot_filter(slot, slot_config.filter());
        engine.set_slot_gain(slot, slot_config.gain());
        rack.set_routing(slot, slot_config, config.midi_inputs.len());
    }
    rack.session_changed();
    rack.set_current_slot_preset(slot, None);
}

/// Runs `action` on the engine, first starting the audio stream on the
/// configured device if this is the first plugin loaded.
fn with_running_engine<T>(
    engine: &RefCell<Engine>,
    config: &config::AppConfig,
    action: impl FnOnce(&mut Engine) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut engine = engine.borrow_mut();
    if !engine.is_running() {
        engine.start(config.audio_device.as_deref())?;
    }
    action(&mut engine)
}

/// Applies a changed audio device selection to an already running engine. The
/// running plugins are moved to the new device as they are
/// (`Engine::switch_output`) when it can run at their sample rate. Otherwise
/// every plugin has to be reloaded (with new keys): open plugin GUIs, holding
/// the old instances' handles, are closed first, and everything is carried
/// across as a session, saved before and restored after. A no-op if nothing
/// has started the engine yet — the selection is picked up by the first load
/// instead.
fn restart_engine(engine: &RefCell<Engine>, config: &RefCell<config::AppConfig>, rack: &Rack, logic: &AppLogic<'_>) {
    if !engine.borrow().is_running() {
        return;
    }
    let audio_device = config.borrow().audio_device.clone();
    let switched = engine.borrow_mut().switch_output(audio_device.as_deref());
    let saved = match switched {
        Ok(true) => return,
        // Nothing changed yet: save the plugins' settings to restore after.
        Ok(false) => save_session(engine, rack),
        // The plugins are out of reach; the last saved session is the best
        // there is, if it's up to date.
        Err(err) => {
            log::error!("moving the plugins to the new output failed, reloading them: {err:#}");
            rack.session_changed_since.get().is_none()
        }
    };
    rack.forget_all();
    let result = engine.borrow_mut().start(audio_device.as_deref());
    rack.refresh_all(&engine.borrow());
    match result {
        Ok(()) if saved => {
            restore_session(engine, config, rack, logic);
        }
        Ok(()) => {}
        Err(err) => {
            log::error!("failed to restart audio with the new device selection: {err:#}");
            for slot in 0..SLOT_COUNT {
                rack.refresh_slot(&engine.borrow(), slot, Some(format!("restart failed: {err}")));
            }
        }
    }
}

/// Index of `value` in `items`, or `-1` (ComboBox's "nothing selected") if absent.
fn index_of(items: &[String], value: Option<&str>) -> i32 {
    value
        .and_then(|value| items.iter().position(|item| item == value))
        .map_or(-1, |index| index as i32)
}

/// Each MIDI input's devices, as the engine takes them.
fn midi_input_devices(config: &config::AppConfig) -> Vec<Vec<String>> {
    config.midi_inputs.iter().map(|input| input.devices.clone()).collect()
}

/// Applies changed MIDI inputs (or their devices) everywhere: the engine's
/// connections, each slot's filter and input buttons, Settings, the config file.
fn apply_midi_inputs(
    engine: &mut Engine,
    config: &config::AppConfig,
    rack: &Rack,
    midi_inputs: &midi_inputs::MidiInputsUi,
) {
    let ports = engine.set_midi_inputs(midi_input_devices(config));
    for (slot, slot_config) in config.slots.iter().enumerate() {
        engine.set_slot_filter(slot, slot_config.filter());
    }
    rack.set_all_routing(config);
    midi_inputs.refresh(config);
    midi_inputs.set_ports(&ports, config);
    log_on_err(config::save(config));
}

fn string_model(items: Vec<String>) -> ModelRc<slint::SharedString> {
    let items: Vec<slint::SharedString> = items.into_iter().map(Into::into).collect();
    ModelRc::new(VecModel::from(items))
}

fn refresh_plugin_folders(app: &AppWindow, config: &config::AppConfig) {
    let folders: Vec<String> = config
        .extra_plugin_dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect();
    app.global::<AppState>().set_extra_plugin_folders(string_model(folders));
}

/// The plugin scan in progress, if any, and the timer that runs it a slice at a
/// time and installs it once done (set up in `main`).
#[derive(Default)]
struct Scanner {
    scan: RefCell<Option<PluginScan>>,
    timer: slint::Timer,
}

impl Scanner {
    /// Starts a scan of the default plugin folders plus `extra_dirs`, unless one
    /// is running. The plugins already loaded keep playing until it's installed.
    fn start(&self, app: &AppWindow, extra_dirs: &[std::path::PathBuf]) {
        if self.scan.borrow().is_some() {
            return;
        }
        *self.scan.borrow_mut() = Some(PluginScan::start(extra_dirs));
        app.global::<AppState>().set_scanning(true);
        self.timer.restart();
    }
}

/// Hands the plugin picker and the Settings tab's list the plugins found by the
/// last scan.
fn refresh_plugin_list(app: &AppWindow, engine: &Engine, picker: &Picker) {
    picker.set_plugins(engine.list_plugins());
    let names = picker
        .plugins
        .borrow()
        .iter()
        .map(|plugin| format!("{} — {} ({})", plugin.name, plugin.kind.label(), plugin.format.label()))
        .collect();
    app.global::<AppState>().set_discovered_plugins(string_model(names));
}

fn log_on_err(result: anyhow::Result<()>) {
    if let Err(err) = result {
        log::error!("failed to save config: {err:#}");
    }
}
