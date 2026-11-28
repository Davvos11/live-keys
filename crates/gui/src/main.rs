slint::include_modules!();

mod config;
mod inline_displays;
mod learn;
mod mappings;
mod metronome;
mod midi_inputs;
mod mock_data;
mod profiles;
mod scenes;
mod session;
mod slot_presets;
mod slot_view;
mod surface_editor;
mod surfaces;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use live_keys_builtin::{DownloadProgress, MissingSamples};
use std::sync::Arc;
use live_keys_engine::{
    Engine, LoadedPluginMeta, MAX_EFFECTS, MAX_MIDI_EFFECTS, ParamInfo, ParamKind, PluginEditor,
    MASTER_SLOT, PluginInfo, PluginKey, PluginKind, PluginScan, PresetInfo, SLOT_COUNT, Section, SlotChain,
    MAX_MIDI_INPUTS, default_plugin_search_paths, list_logical_output_devices, list_midi_ports,
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

/// A built-in instrument's samples, offered or being downloaded, and where to
/// load the instrument once they're in.
struct SampleDownload {
    slot: usize,
    plugin_id: String,
    samples: MissingSamples,
    progress: Arc<DownloadProgress>,
    worker: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

/// Offers the download (`AppState.download-state` 1).
fn offer_download(app: &AppWindow, samples: &MissingSamples) {
    let state = app.global::<AppState>();
    state.set_download_title(format!("Download the {}'s samples?", samples.instrument).into());
    state.set_download_text(
        format!(
            "The {} plays the {} samples: {}. They're downloaded once, about {} MB ({} MB on disk), into {}.",
            samples.instrument,
            samples.name,
            samples.credit,
            samples.download_mb,
            samples.installed_mb,
            samples.dir.display()
        )
        .into(),
    );
    state.set_download_progress(0.0);
    state.set_download_status("".into());
    state.set_download_state(1);
}

/// Follows a running download: its progress, and once it's done, loading the
/// instrument (or showing why it failed).
fn poll_download(app: &AppWindow, download: &RefCell<Option<SampleDownload>>) {
    let state = app.global::<AppState>();
    let finished = {
        let current = download.borrow();
        let Some(current) = current.as_ref() else { return };
        let Some(worker) = &current.worker else { return };
        let files = current.progress.files.load(std::sync::atomic::Ordering::Relaxed);
        let megabytes = current.progress.bytes.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000;
        let total = current.samples.files.max(1);
        state.set_download_progress(files as f32 / total as f32);
        state.set_download_status(
            if current.progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "Cancelling…".to_string()
            } else {
                format!("{files} of {total} files, {megabytes} MB")
            }
            .into(),
        );
        worker.is_finished()
    };
    if !finished {
        return;
    }
    let mut current = download.borrow_mut();
    let Some(done) = current.as_mut() else { return };
    let result = match done.worker.take().map(std::thread::JoinHandle::join) {
        Some(Ok(result)) => result,
        _ => Err(anyhow::anyhow!("the download stopped unexpectedly")),
    };
    let cancelled = done.progress.cancel.load(std::sync::atomic::Ordering::Relaxed);
    match result {
        Ok(()) => {
            let done = current.take().expect("checked above");
            drop(current);
            state.set_download_state(0);
            log::info!("installed {} into {}", done.samples.name, done.samples.dir.display());
            // Loading it now finds its samples.
            app.global::<AppLogic>().invoke_pick_plugin(done.slot as i32, 0, done.plugin_id.into());
        }
        Err(_) if cancelled => {
            *current = None;
            state.set_download_state(0);
        }
        Err(err) => {
            log::error!("downloading {}: {err:#}", done.samples.name);
            state.set_download_title(format!("Couldn't download the {}'s samples", done.samples.instrument).into());
            state.set_download_text(format!("{err:#}").into());
            state.set_download_state(3);
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
        // Our own instruments' UI is the slot's panel and the parameter window.
        PluginEditor::Builtin => false,
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

/// The scene last recalled or saved.
struct CurrentScene {
    dir: std::path::PathBuf,
    name: String,
    /// The slots it restores or clears.
    slots: Vec<usize>,
    changed: bool,
}

impl CurrentScene {
    /// Scene `scene`, as recalled from `recalled` (it, or its draft).
    fn of(scene: &scenes::Stored, recalled: &scenes::Stored, changed: bool) -> Self {
        Self {
            dir: scene.dir.clone(),
            name: scene.scene.name.clone(),
            slots: recalled.scene.affected_slots(),
            changed,
        }
    }
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

/// Which slot `key` is loaded in ([`MASTER_SLOT`] for the master chain), if any.
fn slot_of(engine: &Engine, key: PluginKey) -> Option<usize> {
    (0..=MASTER_SLOT).find(|&slot| {
        engine
            .slot(slot)
            .is_some_and(|chain| chain.plugins().any(|meta| meta.key == key))
    })
}

/// The panel a slot shows for its instrument.
struct SlotPanel {
    key: PluginKey,
    plugin_id: String,
    panel: profiles::Panel,
}

/// An effect's square's sliders: its panel and the model its chain row
/// shows (kept and refilled in place, so a slider being dragged survives).
struct EffectPanel {
    plugin_id: String,
    panel: profiles::Panel,
    controls: Rc<VecModel<EffectControl>>,
}

/// What "More parameters…" picks a parameter for.
#[derive(Clone, Copy)]
enum SearchFor {
    /// Slot `slot`'s instrument's panel cell `cell`.
    Cell { slot: usize, cell: usize },
    /// An effect's square's sliders.
    Effect { key: PluginKey },
}

/// "More parameters…" of a cell's or an effect square's context menu: a
/// search over every parameter of the plugin, to show one.
struct ParamSearch {
    target: SearchFor,
    plugin_id: String,
    /// Every parameter's name and control id, in the plugin's order.
    params: Vec<(String, String)>,
    /// Indices into `params` matching the search, as listed.
    shown: Vec<usize>,
}

impl ParamSearch {
    /// Lists the parameters whose name contains every word of `query`.
    fn filter(&mut self, query: &str, rows: &VecModel<slint::SharedString>) {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.shown = (0..self.params.len())
            .filter(|&index| {
                let name = self.params[index].0.to_lowercase();
                words.iter().all(|word| name.contains(word.as_str()))
            })
            .collect();
        rows.set_vec(self.shown.iter().map(|&index| self.params[index].0.as_str().into()).collect::<Vec<_>>());
    }
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
    /// Shipped and user panel profiles, and the user's row choices.
    profiles: RefCell<profiles::Profiles>,
    /// Per slot: its instrument's panel, built when the instrument is loaded and
    /// after its row changes.
    panels: RefCell<Vec<Option<SlotPanel>>>,
    /// Per slot, like the effect models: the panel's row, drawbars and the pool
    /// as the cells' menu lists it.
    cells: Vec<Rc<VecModel<PanelCell>>>,
    drawbars: Vec<Rc<VecModel<DrawbarCell>>>,
    pool_rows: Vec<Rc<VecModel<PanelPoolRow>>>,
    /// Effects' squares' sliders, by key, built when a row first shows.
    effect_panels: RefCell<HashMap<PluginKey, EffectPanel>>,
    /// Learn mode's flags for them (`EffectControl.learn`), as last shown.
    effect_learn: RefCell<HashMap<PluginKey, Vec<i32>>>,
    /// The pool of the effect whose square's menu is open
    /// (`AppState.effect-control-pool`).
    effect_pool_rows: Rc<VecModel<PanelPoolRow>>,
    /// "More parameters…", while its popup is open; its rows back
    /// `AppState.param-search-rows`.
    param_search: RefCell<Option<ParamSearch>>,
    param_search_rows: Rc<VecModel<slint::SharedString>>,
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
    /// Every scene on disk.
    scene_store: RefCell<Vec<scenes::Stored>>,
    scene_query: RefCell<String>,
    /// Backs `AppState.scene-rows`: the scenes matching `scene_query`.
    scene_rows: Rc<VecModel<SceneRow>>,
    current_scene: RefCell<Option<CurrentScene>>,
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
    /// The master chain's effects (`AppState.master-effects`).
    master_effects: Rc<VecModel<EffectRow>>,
    /// The main window, for the master chain's status line.
    app: std::cell::OnceCell<slint::Weak<AppWindow>>,
    /// Effects' inline displays (their FX squares' pictures).
    inline: RefCell<inline_displays::InlineDisplays>,
    /// MIDI learn's mappings and learn mode.
    mappings: mappings::MappingsUi,
    /// The learn flags last shown, to change only what differs.
    learn_flags: RefCell<mappings::LearnFlags>,
}

impl Rack {
    fn new(config: &config::AppConfig) -> Self {
        fn models<T: 'static>() -> Vec<Rc<VecModel<T>>> {
            (0..SLOT_COUNT).map(|_| Rc::new(VecModel::default())).collect()
        }
        let (midi_effects, audio_effects) = (models(), models());
        let (cells, drawbars, pool_rows) = (models(), models(), models());
        let slots = (0..SLOT_COUNT)
            .map(|slot| SlotState {
                cells: ModelRc::from(cells[slot].clone()),
                drawbars: ModelRc::from(drawbars[slot].clone()),
                pool: ModelRc::from(pool_rows[slot].clone()),
                midi_effects: ModelRc::from(midi_effects[slot].clone()),
                audio_effects: ModelRc::from(audio_effects[slot].clone()),
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
            profiles: RefCell::new(profiles::Profiles::load()),
            panels: RefCell::new((0..SLOT_COUNT).map(|_| None).collect()),
            cells,
            drawbars,
            pool_rows,
            effect_panels: RefCell::default(),
            effect_learn: RefCell::default(),
            effect_pool_rows: Rc::new(VecModel::default()),
            param_search: RefCell::default(),
            param_search_rows: Rc::new(VecModel::default()),
            closed: RefCell::new((0..SLOT_COUNT).map(|_| None).collect()),
            open_uis: RefCell::new(HashMap::new()),
            param_windows: RefCell::new(HashMap::new()),
            current_presets: RefCell::new(HashMap::new()),
            browser: RefCell::new(None),
            preset_rows: Rc::new(VecModel::default()),
            scene_store: RefCell::new(scenes::list()),
            scene_query: RefCell::default(),
            scene_rows: Rc::new(VecModel::default()),
            current_scene: RefCell::new(None),
            scene_status: RefCell::default(),
            scene_target: RefCell::default(),
            scene_target_name: RefCell::default(),
            session_changed_since: Cell::new(None),
            scenes_dirty: Cell::new(true),
            master_effects: Rc::new(VecModel::default()),
            app: std::cell::OnceCell::new(),
            inline: RefCell::new(inline_displays::InlineDisplays::new()),
            mappings: mappings::MappingsUi::new(config),
            learn_flags: RefCell::default(),
        }
    }

    /// Shows the master chain's effects, and `error` (if any) as its status line.
    fn refresh_master(&self, engine: &Engine, error: Option<String>) {
        let rows = engine
            .slot(MASTER_SLOT)
            .map(|chain| chain.effects(Section::Audio).iter().map(|meta| self.effect_row(engine, meta)).collect())
            .unwrap_or_default();
        sync_model(&self.master_effects, rows);
        if let Some(app) = self.app.get().and_then(slint::Weak::upgrade) {
            app.global::<AppState>().set_master_status(error.unwrap_or_default().into());
        }
    }

    /// Recomputes `slot`'s chain and its plugins' GUI and parameter window state
    /// from the engine. `error`, if given, is shown as the slot's status line.
    /// The master chain ([`MASTER_SLOT`]) only has its effects to show.
    fn refresh_slot(&self, engine: &Engine, slot: usize, error: Option<String>) {
        // The chain may have changed: mappings resolve against it.
        self.mappings.setup_dirty.set(true);
        self.mappings.view_dirty.set(true);
        if slot == MASTER_SLOT {
            return self.refresh_master(engine, error);
        }
        let empty = SlotChain::default();
        let chain = engine.slot(slot).unwrap_or(&empty);

        let effect_rows = |section: Section| -> Vec<EffectRow> {
            chain.effects(section).iter().map(|meta| self.effect_row(engine, meta)).collect()
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
        self.refresh_panel(engine, slot, false);
        // Anything loaded into a cleared slot replaces what Undo would bring back.
        if chain.plugins().next().is_some() {
            self.closed.borrow_mut()[slot] = None;
        }
        row.closed_name = self.closed.borrow()[slot].as_ref().map(|c| c.name.as_str()).unwrap_or_default().into();

        self.slots.set_row_data(slot, row);
    }

    /// Notes that the session needs saving.
    fn session_changed(&self) {
        if self.session_changed_since.get().is_none() {
            self.session_changed_since.set(Some(Instant::now()));
        }
    }

    /// Notes that `slot`'s chain changed, so the current scene shows as
    /// changed if it includes the slot.
    fn mark_slot_changed(&self, slot: usize) {
        self.session_changed();
        if slot < SLOT_COUNT {
            // (The master chain is only in the session.)
            self.mark_scene_changed(slot);
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
                let stored = &store[index];
                let current = current.as_ref().filter(|c| c.dir == stored.dir);
                SceneRow {
                    name: stored.scene.name.as_str().into(),
                    current: current.is_some(),
                    // The current scene's changes are the slots' own; another's
                    // are its draft.
                    changed: current.map_or_else(|| scenes::has_draft(&stored.dir), |c| c.changed),
                }
            })
            .collect();
        sync_model(&self.scene_rows, rows);
        state.set_scene_count(store.len() as i32);
        state.set_current_scene(current.as_ref().map(|c| c.name.as_str()).unwrap_or_default().into());
        state.set_scene_status(self.scene_status.borrow().as_str().into());
        state.set_scene_target_name(self.scene_target_name.borrow().as_str().into());
    }

    /// The scene in row `index` of the list, or the current scene for -1.
    /// The scene `step` scenes on from the current one, in the list's order
    /// (all scenes, whatever the search shows), wrapping; with none current,
    /// the first (or the last, going back).
    fn scene_stepped(&self, step: i32) -> Option<scenes::Stored> {
        let store = self.scene_store.borrow();
        if store.is_empty() {
            return None;
        }
        let count = store.len() as i32;
        let current = self.current_scene.borrow().as_ref().and_then(|current| store.iter().position(|stored| stored.dir == current.dir));
        let index = match current {
            Some(index) => (index as i32 + step).rem_euclid(count),
            None if step < 0 => count - 1,
            None => 0,
        };
        store.get(index as usize).cloned()
    }

    /// Every scene's name, in the list's order.
    pub fn scene_names(&self) -> Vec<String> {
        self.scene_store.borrow().iter().map(|stored| stored.scene.name.clone()).collect()
    }

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

    fn is_current_scene(&self, dir: &std::path::Path) -> bool {
        self.current_scene.borrow().as_ref().is_some_and(|current| current.dir == dir)
    }

    /// The current scene as stored.
    fn current_scene_stored(&self) -> Option<scenes::Stored> {
        let dir = self.current_scene.borrow().as_ref()?.dir.clone();
        self.stored_scene(&dir)
    }

    /// What saving `key` in a slot preset needs besides its state: its current
    /// plugin preset and whether it changed since, and its scene mappings.
    fn plugin_extras(&self, key: PluginKey) -> slot_presets::PluginExtras {
        let (preset, preset_changed) = match self.current_presets.borrow().get(&key) {
            Some(current) => (Some(current.preset.clone()), current.changed),
            None => (None, false),
        };
        let mappings = self.mappings.scene.borrow().get(&key).cloned().unwrap_or_default();
        let values = self.mappings.saved_values(key);
        slot_presets::PluginExtras { preset, preset_changed, mappings, values }
    }

    /// The chain row of a loaded effect: [`Rack::plugin_row`] with its sliders.
    fn effect_row(&self, engine: &Engine, meta: &LoadedPluginMeta) -> EffectRow {
        EffectRow { controls: self.effect_controls(engine, meta, false), ..self.plugin_row(meta) }
    }

    /// Effect `meta`'s sliders at current values, building its panel first
    /// if there's none yet (or `rebuild`: its row changed).
    fn effect_controls(&self, engine: &Engine, meta: &LoadedPluginMeta, rebuild: bool) -> ModelRc<EffectControl> {
        let mut panels = self.effect_panels.borrow_mut();
        let entry = panels.entry(meta.key).or_insert_with(|| EffectPanel {
            plugin_id: meta.id.clone(),
            panel: self.profiles.borrow().effect_panel(&meta.id, &meta.params),
            controls: Rc::new(VecModel::default()),
        });
        if rebuild {
            entry.panel = self.profiles.borrow().effect_panel(&meta.id, &meta.params);
            // Learn targets go by the sliders.
            self.mappings.setup_dirty.set(true);
            self.mappings.view_dirty.set(true);
        }
        let mut controls = slot_view::effect_controls(engine, meta.key, &entry.panel);
        self.mappings.decorate_effect(engine, meta.key, &entry.panel, &mut controls);
        if let Some(learn) = self.effect_learn.borrow().get(&meta.key) {
            for (control, &learn) in controls.iter_mut().zip(learn) {
                control.learn = learn;
            }
        }
        sync_model(&entry.controls, controls);
        ModelRc::from(entry.controls.clone())
    }

    /// Shows effect `key`'s sliders at current values, if it has a square.
    fn refresh_effect_controls(&self, engine: &Engine, key: PluginKey) {
        if !self.effect_panels.borrow().contains_key(&key) {
            return;
        }
        if let Some(meta) = engine.plugin(key) {
            self.effect_controls(engine, meta, false);
        }
    }

    /// Rebuilds the sliders of every effect `plugin_id` after its row changed
    /// (rows are per plugin).
    fn rebuild_effect_panels(&self, engine: &Engine, plugin_id: &str) {
        let keys: Vec<PluginKey> = self
            .effect_panels
            .borrow()
            .iter()
            .filter(|(_, panel)| panel.plugin_id == plugin_id)
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            if let Some(meta) = engine.plugin(key) {
                self.effect_controls(engine, meta, true);
            }
        }
    }

    /// Effect `key`'s slider `index`'s control.
    fn effect_control(&self, key: PluginKey, index: usize) -> Option<profiles::Control> {
        let panels = self.effect_panels.borrow();
        let panel = &panels.get(&key)?.panel;
        panel.pool.get(*panel.row.get(index)?).cloned()
    }

    /// Shows learn mode's flags on the effects' sliders, where they changed.
    fn show_effect_learn(&self, engine: &Engine, flags: &HashMap<PluginKey, Vec<i32>>) {
        let changed: Vec<PluginKey> = {
            let shown = self.effect_learn.borrow();
            let keys = flags.keys().chain(shown.keys());
            keys.filter(|key| flags.get(key) != shown.get(key)).copied().collect()
        };
        *self.effect_learn.borrow_mut() = flags.clone();
        for key in changed {
            self.refresh_effect_controls(engine, key);
        }
    }

    /// The chain row of a loaded plugin (an effect's sliders left out: see
    /// [`Rack::effect_row`]).
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
            display: self.inline.borrow().display(meta.key),
            preset_name: current.map(|c| c.preset.name.as_str()).unwrap_or_default().into(),
            preset_user: current.is_some_and(|c| c.preset.user),
            preset_changed: current.is_some_and(|c| c.changed),
            controls: ModelRc::default(),
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
        self.mark_slot_changed(slot);
        if newly_changed {
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
        self.refresh_panel_of(engine, key);
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
        for slot in 0..=MASTER_SLOT {
            self.refresh_slot(engine, slot, None);
        }
    }

    /// Shows a param value that was just sent to the engine in the plugin's
    /// parameter window, if open.
    fn show_param_value(&self, engine: &Engine, key: PluginKey, param_id: u32, value: f32) {
        self.refresh_panel_of(engine, key);
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

    /// Shows `slot`'s instrument panel at current values, building it first if
    /// the instrument changed (or `rebuild`: its row did).
    fn refresh_panel(&self, engine: &Engine, slot: usize, rebuild: bool) {
        let instrument = engine.slot(slot).and_then(|chain| chain.instrument.as_ref());
        let mut panels = self.panels.borrow_mut();
        let current = &mut panels[slot];
        match instrument {
            None => *current = None,
            Some(meta) if rebuild || current.as_ref().is_none_or(|panel| panel.key != meta.key) => {
                // Panel positions and learn targets go by the panel.
                self.mappings.setup_dirty.set(true);
                self.mappings.view_dirty.set(true);
                *current = Some(SlotPanel {
                    key: meta.key,
                    plugin_id: meta.id.clone(),
                    panel: self.profiles.borrow().panel(&meta.id, &meta.params),
                });
            }
            Some(_) => {}
        }
        let (cells, drawbars, pool) = match current {
            Some(SlotPanel { key, panel, .. }) => {
                let mut cells = slot_view::panel_cells(engine, *key, panel);
                self.mappings.decorate(engine, *key, panel, &mut cells);
                (cells, slot_view::panel_drawbars(engine, *key, panel), slot_view::panel_pool(panel))
            }
            None => Default::default(),
        };
        slot_view::sync_cells(&self.cells[slot], cells);
        sync_model(&self.drawbars[slot], drawbars);
        sync_model(&self.pool_rows[slot], pool);
    }

    /// Shows the values of `key`'s panel, if it's an instrument (after a
    /// parameter or setBfree value changed).
    fn refresh_panel_of(&self, engine: &Engine, key: PluginKey) {
        self.refresh_effect_controls(engine, key);
        let slot = self.panels.borrow().iter().position(|panel| panel.as_ref().is_some_and(|panel| panel.key == key));
        if let Some(slot) = slot {
            self.refresh_panel(engine, slot, false);
        }
    }

    /// Rebuilds the panel of every slot with this instrument, after its row
    /// changed (rows are per plugin).
    fn rebuild_panels(&self, engine: &Engine, plugin_id: &str) {
        for slot in 0..SLOT_COUNT {
            let shows = self.panels.borrow()[slot].as_ref().is_some_and(|panel| panel.plugin_id == plugin_id);
            if shows {
                self.refresh_panel(engine, slot, true);
            }
        }
    }

    /// The instrument and control behind `slot`'s row cell `cell`, or its
    /// drawbar `cell` if `drawbar`.
    fn panel_control(&self, slot: usize, cell: usize, drawbar: bool) -> Option<(PluginKey, profiles::Control)> {
        let panels = self.panels.borrow();
        let SlotPanel { key, panel, .. } = panels.get(slot)?.as_ref()?;
        let control = if drawbar {
            panel.drawbars.get(cell)?
        } else {
            panel.pool.get(*panel.row.get(cell)?)?
        };
        Some((*key, control.clone()))
    }

    /// Sets a panel control to `value` (a host-side edit), and shows it: in the
    /// panel, an open parameter window and an open LV2 GUI.
    fn write_control(&self, engine: &RefCell<Engine>, key: PluginKey, control: &profiles::Control, value: profiles::CellValue) {
        if control.read(&engine.borrow(), key) == value {
            return;
        }
        if let profiles::CellValue::Position(position) = value {
            self.mappings.edited(&engine.borrow(), key, control, position);
        }
        let for_gui = control.write(&mut engine.borrow_mut(), key, value);
        let engine = engine.borrow();
        match control.target {
            profiles::Target::Param(param_id) => {
                let value = engine.param_value(key, param_id).unwrap_or_default();
                self.show_param_value(&engine, key, param_id, value);
                self.notify_ui(key, param_id, value);
            }
            profiles::Target::Setbfree(_) => {
                // setBfree doesn't report what the host sets, so its open GUI
                // gets the change as if the plugin had.
                if let Some(OpenEditor::Lv2(handle)) = self.open_uis.borrow().get(&key) {
                    for event in for_gui {
                        handle.notify(PortMessage::Atom { port: event.port, atom: event.to_atom() });
                    }
                }
                self.refresh_panel_of(&engine, key);
            }
        }
        self.mark_changed(&engine, key);
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
        self.inline.borrow_mut().forget(key);
        self.effect_panels.borrow_mut().remove(&key);
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
        self.inline.borrow_mut().clear();
        self.effect_panels.borrow_mut().clear();
        // The plugins come back at their defaults, if at all.
        for slot in 0..SLOT_COUNT {
            self.mark_slot_changed(slot);
        }
    }

    /// Renders the inline displays that asked to be redrawn, of effects whose
    /// squares show (see `InlineDisplays`), into their chain rows.
    fn render_inline_displays(&self, engine: &Engine, scale: f32) {
        let now = Instant::now();
        let mut inline = self.inline.borrow_mut();
        let sections = (0..SLOT_COUNT)
            .flat_map(|slot| [(slot, false, &self.audio_effects[slot]), (slot, true, &self.midi_effects[slot])])
            .chain([(MASTER_SLOT, false, &self.master_effects)]);
        for (slot, midi, model) in sections {
            if !inline.visible(slot, midi) {
                continue;
            }
            for index in 0..model.row_count() {
                let Some(mut row) = model.row_data(index) else { continue };
                let key = row.key as PluginKey;
                // Only when it gets a picture or loses it: the picture itself
                // is in a model of its own.
                if inline.update(engine, key, scale, now) {
                    row.display = inline.display(key);
                    model.set_row_data(index, row);
                }
            }
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
        engine.set_slot_filter(slot, slot_config.filter(config.borrow().master_transpose));
        set_slot_mix(&mut engine, slot, slot_config);
    }
    {
        let config = config.borrow();
        let mut engine = engine.borrow_mut();
        set_master_mix(&mut engine, &config);
        engine.set_metronome(config.metronome.settings(false, config.master_muted));
    }
    // Whether the metronome is playing; it always starts stopped.
    let metronome_playing = Rc::new(Cell::new(false));
    let tap_tempo = Rc::new(RefCell::new(metronome::TapTempo::default()));
    let midi_inputs = Rc::new(midi_inputs::MidiInputsUi::new());
    {
        let ports = engine.borrow_mut().set_midi_inputs(midi_input_devices(&config.borrow()));
        midi_inputs.set_ports(&ports, &config.borrow());
    }
    let surfaces = Rc::new(surfaces::SurfacesUi::new(live_keys_surfaces::Library::load()));
    {
        let ports = list_midi_ports().unwrap_or_default();
        let config = config.borrow();
        surfaces.set_ports(&ports, &config);
        surfaces.refresh(&config);
        sync_surfaces(&mut engine.borrow_mut(), &config, &surfaces, None);
    }
    // Param changes from a plugin's own open GUI arrive as `(key, param_id, value)`
    // — on its hosting thread for LV2, on this thread for VST3; they're forwarded
    // here and applied to the engine from this (the control) thread by the poll
    // timer below, since only one thread may drive the audio engine's command
    // queue.
    let (edit_tx, edit_rx) = mpsc::channel::<GuiEdit>();
    let rack = Rc::new(Rack::new(&config.borrow()));
    let picker = Rc::new(Picker::new());

    let app = AppWindow::new()?;
    let _ = rack.app.set(app.as_weak());
    let settings = SettingsWindow::new()?;
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
    state.set_surfaces(ModelRc::from(surfaces.views.clone()));
    state.set_surface_devices(ModelRc::from(surfaces.devices.clone()));
    state.set_surface_summary(surfaces.summary(&config.borrow()).into());
    state.set_surfaces_collapsed(config.borrow().surfaces.collapsed);
    state.set_scene_rows(ModelRc::from(rack.scene_rows.clone()));
    state.set_learn_rows(ModelRc::from(rack.mappings.rows.clone()));
    state.set_max_midi_inputs(MAX_MIDI_INPUTS as i32);

    settings.set_default_plugin_folders(string_model(default_plugin_search_paths()));
    state.set_master_effects(ModelRc::from(rack.master_effects.clone()));
    state.set_master_slot(MASTER_SLOT as i32);
    state.set_selected_slot(config.borrow().selected_slot.min(SLOT_COUNT - 1) as i32);
    {
        let config = config.borrow();
        state.set_master_volume_db(config.master_volume_db);
        state.set_master_muted(config.master_muted);
        state.set_master_transpose(config.master_transpose.into());
        state.set_metronome_signatures(string_model(metronome::signature_labels()));
        state.set_metronome_subdivisions(string_model(metronome::subdivision_labels()));
        show_metronome(&app, &config.metronome, false);
        state.set_min_bpm(live_keys_engine::MIN_BPM as i32);
        state.set_max_bpm(live_keys_engine::MAX_BPM as i32);
    }
    state.set_slots(ModelRc::from(rack.slots.clone()));
    state.set_max_midi_effects(MAX_MIDI_EFFECTS as i32);
    state.set_max_effects(MAX_EFFECTS as i32);
    state.set_picker_rows(ModelRc::from(picker.rows.clone()));
    state.set_preset_rows(ModelRc::from(rack.preset_rows.clone()));
    state.set_param_search_rows(ModelRc::from(rack.param_search_rows.clone()));
    state.set_effect_control_pool(ModelRc::from(rack.effect_pool_rows.clone()));
    state.set_max_effect_controls(profiles::MAX_EFFECT_CONTROLS as i32);
    state.set_inline_displays(rack.inline.borrow().model());
    refresh_plugin_folders(&settings, &config.borrow());
    refresh_plugin_list(&settings, &engine.borrow(), &picker);
    rack.refresh_all(&engine.borrow());
    rack.set_all_routing(&config.borrow());
    state.set_min_volume_db(config::MIN_VOLUME_DB);
    state.set_max_volume_db(config::MAX_VOLUME_DB);
    state.set_max_transpose(config::MAX_TRANSPOSE);

    let logic = app.global::<AppLogic>();
    let learn = Rc::new(learn::Learn {
        engine: engine.clone(),
        config: config.clone(),
        config_dirty: config_dirty.clone(),
        rack: rack.clone(),
        surfaces: surfaces.clone(),
        metronome_playing: metronome_playing.clone(),
        app: app.as_weak(),
        editor: RefCell::default(),
        edit_banks: Rc::new(VecModel::default()),
        edit_controls: Rc::new(VecModel::default()),
        action_menu: Cell::default(),
        actions: RefCell::default(),
        params: RefCell::default(),
        param_rows: Rc::new(VecModel::default()),
    });
    learn.connect(&logic);
    learn.connect_editor(&logic);

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

    // The sidebar: master output, metronome, MIDI panic.
    {
        let config = config.clone();
        let config_dirty = config_dirty.clone();
        let engine = engine.clone();
        let app_weak = app.as_weak();
        logic.on_set_master_volume(move |volume_db| {
            let mut config = config.borrow_mut();
            config.master_volume_db = volume_db.clamp(config::MIN_VOLUME_DB, config::MAX_VOLUME_DB);
            set_master_mix(&mut engine.borrow_mut(), &config);
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_master_volume_db(config.master_volume_db);
            }
            // Fires on every step of a drag; the poll timer saves.
            config_dirty.set(true);
        });
    }
    {
        let config = config.clone();
        let engine = engine.clone();
        let app_weak = app.as_weak();
        logic.on_set_master_transpose(move |semitones| {
            let mut config = config.borrow_mut();
            let transpose = semitones.clamp(-config::MAX_TRANSPOSE, config::MAX_TRANSPOSE) as i8;
            if config.master_transpose == transpose {
                return;
            }
            config.master_transpose = transpose;
            let mut engine = engine.borrow_mut();
            for (slot, slot_config) in config.slots.iter().enumerate() {
                engine.set_slot_filter(slot, slot_config.filter(transpose));
            }
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_master_transpose(transpose.into());
            }
            log_on_err(config::save(&config));
        });
    }
    {
        let config = config.clone();
        let engine = engine.clone();
        let app_weak = app.as_weak();
        let playing = metronome_playing.clone();
        logic.on_toggle_master_mute(move || {
            let mut config = config.borrow_mut();
            config.master_muted = !config.master_muted;
            let mut engine = engine.borrow_mut();
            set_master_mix(&mut engine, &config);
            // The click follows the mute too.
            engine.set_metronome(config.metronome.settings(playing.get(), config.master_muted));
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_master_muted(config.master_muted);
            }
            log_on_err(config::save(&config));
        });
    }
    // Every metronome change: the engine gets it, the sidebar shows it, the
    // config keeps it (`save`: now, else by the poll timer).
    let apply_metronome = {
        let config = config.clone();
        let config_dirty = config_dirty.clone();
        let engine = engine.clone();
        let app_weak = app.as_weak();
        let playing = metronome_playing.clone();
        Rc::new(move |change: &dyn Fn(&mut metronome::MetronomeConfig), save: bool| {
            let mut config = config.borrow_mut();
            change(&mut config.metronome);
            engine.borrow_mut().set_metronome(config.metronome.settings(playing.get(), config.master_muted));
            if let Some(app) = app_weak.upgrade() {
                show_metronome(&app, &config.metronome, playing.get());
            }
            if save {
                log_on_err(config::save(&config));
            } else {
                config_dirty.set(true);
            }
        })
    };
    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let playing = metronome_playing.clone();
        let apply = apply_metronome.clone();
        logic.on_toggle_metronome(move || {
            // The click needs the output stream, which the first plugin starts.
            if !playing.get()
                && let Err(err) = with_running_engine(&engine, &config.borrow(), |_| Ok(()))
            {
                log::error!("failed to start the metronome: {err:#}");
                rack.refresh_master(&engine.borrow(), Some(format!("The metronome couldn't start: {err}")));
                return;
            }
            playing.set(!playing.get());
            apply(&|_| {}, false);
        });
    }
    {
        let apply = apply_metronome.clone();
        logic.on_set_metronome_bpm(move |bpm| {
            let bpm = (bpm as f32).clamp(live_keys_engine::MIN_BPM, live_keys_engine::MAX_BPM) as u16;
            apply(&|metronome| metronome.bpm = bpm, false);
        });
    }
    {
        let apply = apply_metronome.clone();
        logic.on_tap_tempo(move || {
            if let Some(bpm) = tap_tempo.borrow_mut().tap(Instant::now()) {
                apply(&|metronome| metronome.bpm = bpm, false);
            }
        });
    }
    {
        let apply = apply_metronome.clone();
        logic.on_set_metronome_volume(move |volume| {
            apply(&|metronome| metronome.volume = volume.clamp(0.0, 1.0), false);
        });
    }
    {
        let apply = apply_metronome.clone();
        logic.on_set_metronome_signature(move |index| {
            if (0..metronome::SIGNATURES.len() as i32).contains(&index) {
                apply(&|metronome| metronome.signature = index as usize, true);
            }
        });
    }
    {
        let apply = apply_metronome.clone();
        logic.on_set_metronome_subdivision(move |index| {
            if (0..live_keys_engine::Subdivision::ALL.len() as i32).contains(&index) {
                apply(&|metronome| metronome.subdivision = index as usize, true);
            }
        });
    }
    {
        let engine = engine.clone();
        logic.on_midi_panic(move || engine.borrow_mut().panic());
    }

    {
        let config = config.clone();
        let rack = rack.clone();
        let app_weak = app.as_weak();
        logic.on_select_slot(move |slot| {
            let slot = (slot.max(0) as usize).min(SLOT_COUNT - 1);
            let mut config = config.borrow_mut();
            if config.selected_slot != slot {
                config.selected_slot = slot;
                log_on_err(config::save(&config));
                // "The selected slot" targets resolve elsewhere now.
                rack.mappings.setup_dirty.set(true);
                rack.mappings.view_dirty.set(true);
            }
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_selected_slot(slot as i32);
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_step_scene(move |step| {
            if let Some(stored) = rack.scene_stepped(step) {
                recall_scene(&engine, &config, &rack, &stored);
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_recall_scene_number(move |number| {
            let stored = (number >= 1).then(|| rack.scene_store.borrow().get(number as usize - 1).cloned()).flatten();
            if let Some(stored) = stored {
                recall_scene(&engine, &config, &rack, &stored);
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_recall_scene(move |name| {
            let stored = rack.scene_store.borrow().iter().find(|stored| stored.scene.name == name.as_str()).cloned();
            if let Some(stored) = stored {
                recall_scene(&engine, &config, &rack, &stored);
            }
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_step_instrument_preset(move |slot, step| {
            step_instrument_preset(&engine, &rack, slot.max(0) as usize, step);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let surfaces = surfaces.clone();
        let app_weak = app.as_weak();
        logic.on_toggle_surface_device(move |device| {
            let mut config = config.borrow_mut();
            let mut devices = surfaces.shown_devices(&config);
            match devices.iter().position(|name| *name == device.as_str()) {
                Some(index) => {
                    devices.remove(index);
                }
                None => devices.push(device.to_string()),
            }
            config.surfaces.devices = Some(devices);
            surfaces.refresh(&config);
            sync_surfaces(&mut engine.borrow_mut(), &config, &surfaces, app_weak.upgrade().as_ref());
            log_on_err(config::save(&config));
        });
    }

    {
        let config = config.clone();
        let app_weak = app.as_weak();
        logic.on_set_surfaces_collapsed(move |collapsed| {
            let mut config = config.borrow_mut();
            config.surfaces.collapsed = collapsed;
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_surfaces_collapsed(collapsed);
            }
            log_on_err(config::save(&config));
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
            let master_transpose = config.master_transpose;
            let Some(slot_config) = config.slots.get_mut(slot as usize) else {
                return;
            };
            if !(0..input_count as i32).contains(&input) {
                return;
            }
            slot_config.midi_inputs ^= 1 << input;
            rack.mark_scene_changed(slot as usize);
            engine.borrow_mut().set_slot_filter(slot as usize, slot_config.filter(master_transpose));
            rack.set_routing(slot as usize, slot_config, input_count);
            log_on_err(config::save(&config));
        });
    }

    {
        let settings_weak = settings.as_weak();
        let config = config.clone();
        settings.on_add_plugin_folder(move || {
            let Some(settings) = settings_weak.upgrade() else {
                return;
            };
            let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                return;
            };
            config.borrow_mut().extra_plugin_dirs.push(folder);
            log_on_err(config::save(&config.borrow()));
            refresh_plugin_folders(&settings, &config.borrow());
        });
    }

    {
        let settings_weak = settings.as_weak();
        let config = config.clone();
        settings.on_remove_plugin_folder(move |folder| {
            let Some(settings) = settings_weak.upgrade() else {
                return;
            };
            config
                .borrow_mut()
                .extra_plugin_dirs
                .retain(|dir| dir.display().to_string() != folder.as_str());
            log_on_err(config::save(&config.borrow()));
            refresh_plugin_folders(&settings, &config.borrow());
        });
    }

    {
        let settings_weak = settings.as_weak();
        logic.on_open_settings(move || {
            if let Some(settings) = settings_weak.upgrade() {
                log_on_err(settings.show().map_err(Into::into));
            }
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
        let settings_weak = settings.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let picker = picker.clone();
        let restored = restored.clone();
        let started = started.clone();
        scanner.timer.start(slint::TimerMode::Repeated, SCAN_TICK, move || {
            let (Some(scanner), Some(app), Some(settings)) =
                (scanner_weak.upgrade(), app_weak.upgrade(), settings_weak.upgrade())
            else {
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
            refresh_plugin_list(&settings, &engine.borrow(), &picker);
            // The picker may be open, waiting for the list.
            picker.refilter(&config.borrow().pinned_plugins);
            rack.refresh_all(&engine.borrow());
            set_scanning(&app, &settings, false);
            if !started.get() {
                restored.set(restore_session(&engine, &config, &rack, &app.global::<AppLogic>()));
                started.set(true);
                app.global::<AppState>().set_starting(false);
            }
        });
    }
    scanner.start(&app, &settings, &config.borrow().extra_plugin_dirs);
    {
        let app_weak = app.as_weak();
        let settings_weak = settings.as_weak();
        let config = config.clone();
        let scanner = scanner.clone();
        settings.on_rescan_plugins(move || {
            if let (Some(app), Some(settings)) = (app_weak.upgrade(), settings_weak.upgrade()) {
                scanner.start(&app, &settings, &config.borrow().extra_plugin_dirs);
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

    let download: Rc<RefCell<Option<SampleDownload>>> = Rc::new(RefCell::new(None));
    {
        let download = download.clone();
        let app_weak = app.as_weak();
        logic.on_start_download(move || {
            let mut current = download.borrow_mut();
            let Some(current) = current.as_mut() else { return };
            if current.worker.is_some() {
                return;
            }
            let progress = Arc::new(DownloadProgress::default());
            current.progress = progress.clone();
            let samples = current.samples.clone();
            let title = format!("Downloading the {}'s samples…", samples.instrument);
            current.worker = Some(
                std::thread::Builder::new()
                    .name("sample-download".into())
                    .spawn(move || samples.install(&progress))
                    .expect("failed to spawn the download thread"),
            );
            if let Some(app) = app_weak.upgrade() {
                let state = app.global::<AppState>();
                state.set_download_title(title.into());
                state.set_download_progress(0.0);
                state.set_download_status("Starting…".into());
                state.set_download_state(2);
            }
        });
    }
    {
        let download = download.clone();
        let app_weak = app.as_weak();
        logic.on_cancel_download(move || {
            let mut current = download.borrow_mut();
            match current.as_ref() {
                // The poll timer closes the dialog once it has stopped.
                Some(running) if running.worker.is_some() => {
                    running.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {
                    *current = None;
                    if let Some(app) = app_weak.upgrade() {
                        app.global::<AppState>().set_download_state(0);
                    }
                }
            }
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let download = download.clone();
        let app_weak = app.as_weak();
        logic.on_pick_plugin(move |slot, category, plugin_id| {
            let slot = slot as usize;
            let kind = kind_from(category);
            // A built-in instrument without its samples: offer them first; the
            // instrument loads once they're in (`poll_download`).
            if kind == PluginKind::Instrument
                && let Some(samples) = live_keys_builtin::missing_samples(&plugin_id)
            {
                if download.borrow().as_ref().is_some_and(|d| d.worker.is_some()) {
                    return;
                }
                if let Some(app) = app_weak.upgrade() {
                    offer_download(&app, &samples);
                }
                *download.borrow_mut() = Some(SampleDownload {
                    slot,
                    plugin_id: plugin_id.to_string(),
                    samples,
                    progress: Arc::default(),
                    worker: None,
                });
                return;
            }
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
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_effect(move |key| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let logic = app.global::<AppLogic>();
            let supported = engine.borrow().plugin(key as PluginKey).is_some_and(gui_supported);
            if supported {
                logic.invoke_open_plugin_gui(key);
            }
            // Opening the parameter window clears the GUI's error, if any.
            if !rack.open_uis.borrow().contains_key(&(key as PluginKey)) {
                logic.invoke_toggle_params(key);
            }
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
                    // And its control outputs (meters); changes follow from the poll timer.
                    for (port, value) in engine.control_outputs(key) {
                        handle.notify(PortMessage::Control { port, value });
                    }
                    OpenEditor::Lv2(handle)
                }),
                PluginEditor::Vst3(controller) => {
                    Vst3Editor::open(controller, &meta.name, Box::new(forward_edit))
                        .map(OpenEditor::Vst3)
                }
                // Its edits come through `Engine::idle_plugins`.
                PluginEditor::Clap(controller) => ClapEditor::open(controller, &meta.name).map(OpenEditor::Clap),
                PluginEditor::Builtin => Err(anyhow::anyhow!("it has no GUI of its own")),
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

    // The instrument panel: values of its cells and drawbars, and which
    // controls the row shows.
    {
        let engine = engine.clone();
        let rack = rack.clone();
        let edit = move |slot: i32, cell: i32, drawbar: bool, value: profiles::CellValue| {
            if let Some((key, control)) = rack.panel_control(slot as usize, cell as usize, drawbar) {
                rack.write_control(&engine, key, &control, value);
            }
        };
        let edit = Rc::new(edit);
        let e = edit.clone();
        logic.on_set_cell_position(move |slot, cell, position| {
            e(slot, cell, false, profiles::CellValue::Position(position));
        });
        let e = edit.clone();
        logic.on_set_cell_on(move |slot, cell, on| e(slot, cell, false, profiles::CellValue::On(on)));
        let e = edit.clone();
        logic.on_set_cell_index(move |slot, cell, index| {
            e(slot, cell, false, profiles::CellValue::Index(index.max(0) as usize));
        });
        logic.on_set_drawbar(move |slot, drawbar, level| {
            edit(slot, drawbar, true, profiles::CellValue::Index(level.clamp(0, 8) as usize));
        });
    }

    // Shows a control in a row cell (from its menu, or "More parameters…"),
    // keeping the choice for this instrument; errors go to the slot's status.
    let set_panel_cell = {
        let engine = engine.clone();
        let rack = rack.clone();
        Rc::new(move |slot: usize, cell: usize, control_id: &str, toggle: bool| {
            let engine = engine.borrow();
            let Some(meta) = engine.slot(slot).and_then(|chain| chain.instrument.as_ref()) else {
                return;
            };
            // From a cell's menu, a shown control is taken off; anything else
            // is added (instead of that cell's control when the panel is full).
            let shown = rack.panels.borrow()[slot].as_ref().is_some_and(|panel| {
                let panel = &panel.panel;
                panel.row.iter().any(|&index| panel.pool[index].id == control_id)
            });
            let mut profiles = rack.profiles.borrow_mut();
            let result = if shown && toggle {
                profiles.remove_control(profiles::PanelKind::Instrument, &meta.id, &meta.params, control_id)
            } else {
                profiles.add_control(profiles::PanelKind::Instrument, &meta.id, &meta.params, control_id, cell)
            };
            drop(profiles);
            match result {
                Ok(()) => rack.rebuild_panels(&engine, &meta.id),
                Err(err) => {
                    log::error!("failed to change the panel of {}: {err:#}", meta.name);
                    rack.refresh_slot(&engine, slot, Some(format!("failed to change the panel: {err}")));
                }
            }
        })
    };
    // Shows a parameter as one of an effect's square's sliders (from its
    // menu, or "More parameters…"), kept for this plugin; a shown one is taken
    // off when `toggle`. Full: instead of the last.
    let set_effect_control = {
        let engine = engine.clone();
        let rack = rack.clone();
        Rc::new(move |key: PluginKey, control_id: &str, toggle: bool| {
            let engine = engine.borrow();
            let Some(meta) = engine.plugin(key) else { return };
            let shown = rack.effect_panels.borrow().get(&key).is_some_and(|panel| {
                let panel = &panel.panel;
                panel.row.iter().any(|&index| panel.pool[index].id == control_id)
            });
            let kind = profiles::PanelKind::Effect;
            let mut profiles = rack.profiles.borrow_mut();
            let result = if shown && toggle {
                profiles.remove_control(kind, &meta.id, &meta.params, control_id)
            } else {
                profiles.add_control(kind, &meta.id, &meta.params, control_id, profiles::MAX_EFFECT_CONTROLS - 1)
            };
            drop(profiles);
            match result {
                Ok(()) => rack.rebuild_effect_panels(&engine, &meta.id),
                Err(err) => log::error!("failed to change the sliders of {}: {err:#}", meta.name),
            }
            // The menu, if open, shows the change.
            if let Some(panel) = rack.effect_panels.borrow().get(&key) {
                rack.effect_pool_rows.set_vec(slot_view::panel_pool(&panel.panel));
            }
        })
    };
    {
        let rack = rack.clone();
        let set_panel_cell = set_panel_cell.clone();
        logic.on_set_panel_cell(move |slot, cell, pool_index| {
            let control_id = rack.panels.borrow()[slot as usize]
                .as_ref()
                .and_then(|panel| panel.panel.pool.get(pool_index as usize))
                .map(|control| control.id.clone());
            if let Some(control_id) = control_id {
                set_panel_cell(slot as usize, cell as usize, &control_id, true);
            }
        });
    }
    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_reset_panel_row(move |slot| {
            let slot = slot as usize;
            let engine = engine.borrow();
            let Some(meta) = engine.slot(slot).and_then(|chain| chain.instrument.as_ref()) else {
                return;
            };
            let result = rack.profiles.borrow_mut().reset_row(&meta.id);
            match result {
                Ok(()) => rack.rebuild_panels(&engine, &meta.id),
                Err(err) => {
                    log::error!("failed to reset the panel of {}: {err:#}", meta.name);
                    rack.refresh_slot(&engine, slot, Some(format!("failed to reset the panel: {err}")));
                }
            }
        });
    }
    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_param_search(move |slot, cell| {
            let engine = engine.borrow();
            let Some(meta) = engine.slot(slot as usize).and_then(|chain| chain.instrument.as_ref()) else {
                return;
            };
            let mut search = ParamSearch {
                target: SearchFor::Cell { slot: slot as usize, cell: cell as usize },
                plugin_id: meta.id.clone(),
                params: meta
                    .params
                    .iter()
                    .map(|param| (param.name.clone(), profiles::param_control(param).id))
                    .collect(),
                shown: Vec::new(),
            };
            search.filter("", &rack.param_search_rows);
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_param_search_count(search.params.len() as i32);
            }
            *rack.param_search.borrow_mut() = Some(search);
        });
    }
    {
        let rack = rack.clone();
        logic.on_filter_param_search(move |query| {
            if let Some(search) = rack.param_search.borrow_mut().as_mut() {
                search.filter(&query, &rack.param_search_rows);
            }
        });
    }
    {
        let engine = engine.clone();
        let rack = rack.clone();
        let set_effect_control = set_effect_control.clone();
        logic.on_pick_param_search(move |index| {
            let Some(search) = rack.param_search.borrow_mut().take() else {
                return;
            };
            let Some(control_id) = search.shown.get(index as usize).map(|&shown| &search.params[shown].1) else {
                return;
            };
            match search.target {
                SearchFor::Cell { slot, cell } => {
                    // The instrument may have changed meanwhile.
                    let same = engine
                        .borrow()
                        .slot(slot)
                        .and_then(|chain| chain.instrument.as_ref())
                        .is_some_and(|meta| meta.id == search.plugin_id);
                    if same {
                        set_panel_cell(slot, cell, control_id, false);
                    }
                }
                SearchFor::Effect { key } => set_effect_control(key, control_id, false),
            }
        });
    }
    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_effect_param_search(move |key| {
            let engine = engine.borrow();
            let Some(meta) = engine.plugin(key as PluginKey) else { return };
            let mut search = ParamSearch {
                target: SearchFor::Effect { key: meta.key },
                plugin_id: meta.id.clone(),
                params: meta
                    .params
                    .iter()
                    .map(|param| (param.name.clone(), profiles::param_control(param).id))
                    .collect(),
                shown: Vec::new(),
            };
            search.filter("", &rack.param_search_rows);
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppState>().set_param_search_count(search.params.len() as i32);
            }
            *rack.param_search.borrow_mut() = Some(search);
        });
    }
    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_effect_control(move |key, index, position| {
            let key = key as PluginKey;
            if let Some(control) = rack.effect_control(key, index.max(0) as usize) {
                rack.write_control(&engine, key, &control, control.cell_at(position));
            }
        });
    }
    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_effect_controls(move |key| {
            let engine = engine.borrow();
            let Some(meta) = engine.plugin(key as PluginKey) else { return };
            // Built if its square hasn't shown yet.
            rack.effect_controls(&engine, meta, false);
            let pool = rack
                .effect_panels
                .borrow()
                .get(&meta.key)
                .map(|panel| slot_view::panel_pool(&panel.panel))
                .unwrap_or_default();
            rack.effect_pool_rows.set_vec(pool);
        });
    }
    {
        let rack = rack.clone();
        let set_effect_control = set_effect_control.clone();
        logic.on_toggle_effect_control(move |key, pool| {
            let key = key as PluginKey;
            let control_id = rack
                .effect_panels
                .borrow()
                .get(&key)
                .and_then(|panel| panel.panel.pool.get(pool.max(0) as usize))
                .map(|control| control.id.clone());
            if let Some(control_id) = control_id {
                set_effect_control(key, &control_id, true);
            }
        });
    }
    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_reset_effect_controls(move |key| {
            let engine = engine.borrow();
            let Some(meta) = engine.plugin(key as PluginKey) else { return };
            let result = rack.profiles.borrow_mut().reset_row(&meta.id);
            match result {
                Ok(()) => rack.rebuild_effect_panels(&engine, &meta.id),
                Err(err) => log::error!("failed to reset the sliders of {}: {err:#}", meta.name),
            }
            if let Some(panel) = rack.effect_panels.borrow().get(&meta.key) {
                rack.effect_pool_rows.set_vec(slot_view::panel_pool(&panel.panel));
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
                recall_scene(&engine, &config, &rack, &stored);
            }
        });
    }

    {
        let rack = rack.clone();
        logic.on_begin_scene_save(move || {
            let current = rack.current_scene_stored();
            *rack.scene_target.borrow_mut() = None;
            rack.scene_status.borrow_mut().clear();
            *rack.scene_target_name.borrow_mut() =
                current.as_ref().map(|stored| stored.scene.name.clone()).unwrap_or_default();
            rack.scenes_dirty.set(true);
        });
    }

    {
        let rack = rack.clone();
        logic.on_begin_scene_edit(move |index| {
            if let Some(stored) = rack.scene_at(index) {
                rack.target_scene(&stored);
            }
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
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_save_scene(move |name| {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            // Saving under an existing scene's name overwrites it.
            let result = save_scene(&engine, &config, &rack, name);
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
            let name = stored.scene.name.clone();
            let result = save_scene(&engine, &config, &rack, &name);
            rack.after_scene_change("Saving", &name, result);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_save_scene_changes(move |index| {
            let Some(stored) = rack.scene_at(index) else {
                return;
            };
            let name = stored.scene.name.clone();
            let result = if rack.is_current_scene(&stored.dir) {
                save_scene(&engine, &config, &rack, &name)
            } else {
                scenes::apply_draft(&stored).map(drop)
            };
            rack.after_scene_change("Saving", &name, result);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_discard_scene_changes(move |index| {
            let Some(stored) = rack.scene_at(index) else {
                return;
            };
            let result = scenes::discard_draft(&stored.dir);
            if result.is_ok() && rack.is_current_scene(&stored.dir) {
                load_scene(&engine, &config, &rack, &stored, &stored, false);
            }
            rack.after_scene_change("Discarding the changes to", &stored.scene.name, result);
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
            let result = scenes::rename(&stored, name).map(|renamed| {
                let mut current = rack.current_scene.borrow_mut();
                if let Some(current) = current.as_mut().filter(|current| current.dir == stored.dir) {
                    current.dir = renamed.dir;
                    current.name = renamed.scene.name;
                }
            });
            rack.after_scene_change("Renaming", &stored.scene.name, result);
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
                let dir = slot_view::snapshot(&mut engine.borrow_mut(), slot, |key| rack.plugin_extras(key));
                match dir {
                    Ok(dir) => Some(slot_view::Closed {
                        name,
                        dir,
                        routing: config.borrow().slots[slot].routing(),
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
                engine.set_slot_filter(slot, slot_config.filter(config.master_transpose));
                set_slot_mix(&mut engine, slot, slot_config);
                rack.set_routing(slot, slot_config, config.midi_inputs.len());
                log_on_err(config::save(&config));
            }
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
            let master_transpose = config.master_transpose;
            let slot_config = &mut config.slots[slot];
            let transpose = semitones.clamp(-config::MAX_TRANSPOSE, config::MAX_TRANSPOSE) as i8;
            if slot_config.transpose == transpose {
                return;
            }
            slot_config.transpose = transpose;
            engine.borrow_mut().set_slot_filter(slot, slot_config.filter(master_transpose));
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
            set_slot_mix(&mut engine.borrow_mut(), slot, slot_config);
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
                let master_transpose = config.master_transpose;
                let slot_config = &mut config.slots[slot];
                let before = slot_config.routing();
                // Channel 0 is omni; note/velocity values come from sliders already
                // bounded to 0..=127.
                slot_config.midi_channel = (1..=16).contains(&channel).then(|| channel as u8 - 1);
                slot_config.note_low = note_low.clamp(0, 127) as u8;
                slot_config.note_high = note_high.clamp(0, 127) as u8;
                slot_config.velocity_low = velocity_low.clamp(0, 127) as u8;
                slot_config.velocity_high = velocity_high.clamp(0, 127) as u8;
                engine.borrow_mut().set_slot_filter(slot, slot_config.filter(master_transpose));
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
        let rack = rack.clone();
        logic.on_set_slot_tab(move |slot, tab| {
            rack.inline.borrow_mut().set_tab(slot as usize, tab);
        });
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
            set_slot_mix(&mut engine.borrow_mut(), slot, slot_config);
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
        let surfaces = surfaces.clone();
        let learn = learn.clone();
        poll_timer.start(slint::TimerMode::Repeated, UI_POLL_INTERVAL, move || {
            learn.poll();
            let activity = surfaces.tick();
            if let Some(app) = app_weak.upgrade() {
                let state = app.global::<AppState>();
                if state.get_surface_activity() != activity {
                    state.set_surface_activity(activity);
                }
            }
            rack.poll_uis(&engine.borrow());
            if let Some(app) = app_weak.upgrade() {
                rack.render_inline_displays(&engine.borrow(), app.window().scale_factor());
                let beat = engine.borrow().metronome_beat().map_or(-1, |beat| beat as i32);
                let state = app.global::<AppState>();
                if state.get_metronome_beat() != beat {
                    state.set_metronome_beat(beat);
                }
            }
            if rack.scenes_dirty.replace(false)
                && let Some(app) = app_weak.upgrade()
            {
                rack.refresh_scenes(&app);
            }
            if let Some(app) = app_weak.upgrade() {
                poll_download(&app, &download);
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
                // Their control outputs (meters…) too: LV2 GUIs learn them only from us.
                engine.borrow_mut().drain_control_outputs(|key, port, value| {
                    if let Some(OpenEditor::Lv2(handle)) = open_uis.get(&key) {
                        handle.notify(PortMessage::Control { port, value });
                    }
                });
            }
            // setBfree's reports (its own GUI, a program change, a state load)
            // move its panel's drawbars and switches.
            let changed = engine.borrow_mut().take_setbfree_changes();
            for key in changed {
                rack.refresh_panel_of(&engine.borrow(), key);
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
        let surfaces = surfaces.clone();
        let app_weak = app.as_weak();
        midi_port_timer.start(slint::TimerMode::Repeated, MIDI_PORT_POLL_INTERVAL, move || {
            let ports = engine.borrow_mut().sync_midi_inputs();
            let config = config.borrow();
            midi_inputs.set_ports(&ports, &config);
            surfaces.set_ports(&ports, &config);
            sync_surfaces(&mut engine.borrow_mut(), &config, &surfaces, app_weak.upgrade().as_ref());
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
        write_mock_data(&app, &settings, &engine.borrow(), &rack, &path);
    }
    // Always on a normal exit (some plugin settings change without the host
    // seeing it), unless startup didn't finish, or the last session wasn't
    // restored and nothing has happened since: either would overwrite it with
    // an empty rack.
    if started.get() && (restored.get() || rack.session_changed_since.get().is_some()) {
        save_session(&engine, &rack);
    }
    // Keyboards in DAW mode go back to their own.
    engine.borrow().close_surfaces();
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
        if result.is_ok() {
            restore_values(&mut engine.borrow_mut(), key, &plugin.values);
        }
        // The preset's mappings replace the plugin's, loaded or not.
        rack.mappings.set_scene(key, plugin.mappings.clone());
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
    errors
}

/// Puts back the bases of `key`'s modulated targets that its slot preset kept
/// (its state file has them as the modulation left them).
fn restore_values(engine: &mut Engine, key: PluginKey, values: &[mappings::SavedValue]) {
    for saved in values {
        match &saved.target {
            mappings::PluginTarget::Param { symbol } => {
                let id = engine
                    .plugin(key)
                    .and_then(|meta| meta.params.iter().find(|param| param.symbol == *symbol))
                    .map(|param| param.id);
                if let Some(id) = id {
                    engine.set_param(key, id, saved.value);
                }
            }
            mappings::PluginTarget::Setbfree { function } => {
                engine.restore_setbfree(key, function, saved.value.round().clamp(0.0, 127.0) as u8);
            }
        }
    }
}

/// What saving the slots as a scene stores: each slot with an instrument is
/// restored, each without is cleared.
fn slots_to_save(engine: &Engine, config: &config::AppConfig) -> Vec<scenes::SlotToSave> {
    (0..SLOT_COUNT)
        .map(|slot| scenes::SlotToSave {
            slot,
            mode: if engine.slot(slot).is_some_and(|chain| chain.instrument.is_some()) {
                scenes::SlotMode::Restore
            } else {
                scenes::SlotMode::Clear
            },
            routing: config.slots[slot].routing(),
        })
        .collect()
}

/// Saves the slots as scene `name` (replacing one of that name, and dropping
/// its draft), and makes it the current scene.
fn save_scene(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    name: &str,
) -> anyhow::Result<()> {
    let slots = slots_to_save(&engine.borrow(), &config.borrow());
    let stored = scenes::save(&mut engine.borrow_mut(), name, &slots, |key| rack.plugin_extras(key))?;
    *rack.current_scene.borrow_mut() = Some(CurrentScene::of(&stored, &stored, false));
    Ok(())
}

/// Recalls scene `stored` as the user does (a click, a mapped button): the
/// current scene's unsaved changes are kept as its draft first (if that
/// fails, nothing is recalled), and a scene with a draft is recalled as its
/// draft, still changed. The current scene, changed, is left as it is.
fn recall_scene(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    stored: &scenes::Stored,
) {
    let changed_current = rack.current_scene.borrow().as_ref().filter(|current| current.changed).map(|current| current.dir.clone());
    if let Some(dir) = changed_current {
        if dir == stored.dir {
            return;
        }
        if let Some(current) = rack.stored_scene(&dir) {
            let slots = slots_to_save(&engine.borrow(), &config.borrow());
            let kept = scenes::save_draft(&mut engine.borrow_mut(), &current, &slots, |key| rack.plugin_extras(key));
            if let Err(err) = kept {
                log::error!("keeping the changes to scene '{}' failed: {err:#}", current.scene.name);
                *rack.scene_status.borrow_mut() =
                    format!("Keeping the changes to \"{}\" failed, so it's still current: {err:#}", current.scene.name);
                rack.scenes_dirty.set(true);
                return;
            }
        }
    }
    match scenes::draft(stored) {
        Some(Ok(draft)) => load_scene(engine, config, rack, &draft, stored, true),
        Some(Err(err)) => {
            log::error!("scene '{}': reading its changes failed: {err:#}", stored.scene.name);
            load_scene(engine, config, rack, stored, stored, false);
        }
        None => load_scene(engine, config, rack, stored, stored, false),
    }
}

/// Recalls `stored` (scene `scene`, or its draft) and makes `scene` current
/// (`changed` for a draft): each slot it restores gets the saved chain (as
/// [`load_slot_preset`] does, so plugins already in place are kept), its
/// routing and volume; each slot it clears is emptied; the others are left
/// alone. What goes wrong is shown in the slot's status line; the rest is
/// recalled anyway.
fn load_scene(
    engine: &RefCell<Engine>,
    config: &RefCell<config::AppConfig>,
    rack: &Rack,
    stored: &scenes::Stored,
    scene: &scenes::Stored,
    changed: bool,
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

        let mut config = config.borrow_mut();
        config.slots[slot].set_routing(saved.routing);
        let slot_config = &config.slots[slot];
        let mut engine = engine.borrow_mut();
        engine.set_slot_filter(slot, slot_config.filter(config.master_transpose));
        set_slot_mix(&mut engine, slot, slot_config);
        rack.set_routing(slot, slot_config, config.midi_inputs.len());
        rack.refresh_slot(&engine, slot, (!errors.is_empty()).then(|| errors.join("\n")));
    }
    log_on_err(config::save(&config.borrow()));
    *rack.current_scene.borrow_mut() = Some(CurrentScene::of(scene, stored, changed || failed));
    *rack.scene_status.borrow_mut() = String::new();
    rack.scenes_dirty.set(true);
    rack.session_changed();
}

/// Writes the window's state as the preview's mock data (see `mock_data`), with
/// an open parameter window's plugin, else the first loaded plugin, for
/// `ParamsWindow`.
fn write_mock_data(app: &AppWindow, settings: &SettingsWindow, engine: &Engine, rack: &Rack, path: &std::path::Path) {
    let params_window = match rack.param_windows.borrow().values().next() {
        Some(open) => (open.window.get_plugin_name(), ModelRc::from(open.rows.clone())),
        None => {
            let meta = (0..SLOT_COUNT).find_map(|slot| engine.slot(slot)?.plugins().next());
            let name = meta.map(|meta| meta.name.as_str()).unwrap_or_default().into();
            let rows = meta.map(|meta| param_rows(engine, meta.key)).unwrap_or_default();
            (name, ModelRc::new(VecModel::from(rows)))
        }
    };
    let text = mock_data::generate(app, settings, params_window);
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
    let slots = (0..=MASTER_SLOT)
        .map(|slot| {
            let engine = engine.borrow();
            let chain = engine.slot(slot);
            let positions = |keys: Vec<PluginKey>| -> Vec<session::Position> {
                keys.into_iter()
                    .filter_map(|key| chain?.position_of(key).map(session::Position::from))
                    .collect()
            };
            session::SlotToSave {
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
        rack.plugin_extras(key)
    });
    if let Err(err) = &result {
        log::error!("saving the session failed: {err:#}");
    }
    result.is_ok()
}

/// Restores the session saved last time, if any: each slot's chain and
/// settings, the current scene, and the plugin windows that
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
        if slot > MASTER_SLOT {
            continue;
        }
        let errors = match chain {
            Ok(chain) => load_slot_preset(engine, config, rack, slot, &chain),
            Err(err) if slot == MASTER_SLOT => vec![format!("The last session's master effects couldn't be read: {err:#}")],
            Err(err) => vec![format!("The last session's slot {} couldn't be read: {err:#}", slot + 1)],
        };
        let saved_slot = saved.slots.iter().find(|saved_slot| saved_slot.slot == slot);
        if let Some(saved_slot) = saved_slot {
            windows.push((slot, saved_slot.open_guis.clone(), saved_slot.open_params.clone()));
        }
        rack.refresh_slot(&engine.borrow(), slot, (!errors.is_empty()).then(|| errors.join("\n")));
    }
    if let Some(scene) = saved.current_scene
        && let Some(stored) = rack.stored_scene(&scene.dir)
    {
        *rack.current_scene.borrow_mut() = Some(CurrentScene::of(&stored, &stored, scene.changed));
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
/// the defaults.
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
        engine.set_slot_filter(slot, slot_config.filter(config.master_transpose));
        set_slot_mix(&mut engine, slot, slot_config);
        rack.set_routing(slot, slot_config, config.midi_inputs.len());
    }
    rack.session_changed();
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

/// Hands the engine the devices the control surfaces panel shows, if that
/// changed, and shows them in the panel's summary.
/// Loads the preset `step` presets on (wrapping) from the current one of
/// `slot`'s instrument, in the order the browser lists them (favourites
/// aside); with none current, the first (or last, going back).
fn step_instrument_preset(engine: &RefCell<Engine>, rack: &Rack, slot: usize, step: i32) {
    let Some(key) = engine.borrow().slot(slot).and_then(|chain| chain.instrument.as_ref().map(|meta| meta.key)) else {
        return;
    };
    let presets = engine.borrow().presets(key);
    if presets.is_empty() {
        return;
    }
    let count = presets.len() as i32;
    let current = rack
        .current_presets
        .borrow()
        .get(&key)
        .and_then(|current| presets.iter().position(|preset| preset.id == current.preset.id));
    let next = match current {
        Some(index) => (index as i32 + step).rem_euclid(count),
        None if step < 0 => count - 1,
        None => 0,
    } as usize;
    let preset = presets[next].clone();
    let result = engine.borrow_mut().load_preset(key, &preset.id);
    let engine = engine.borrow();
    let error = match result {
        Ok(()) => {
            rack.set_current_preset(key, Some(preset));
            rack.refresh_param_rows(&engine, key);
            rack.mark_slot_changed(slot);
            None
        }
        Err(err) => {
            log::error!("failed to load preset '{}': {err:#}", preset.name);
            Some(format!("Loading \"{}\" failed: {err}", preset.name))
        }
    };
    rack.refresh_slot(&engine, slot, error);
}

fn sync_surfaces(engine: &mut Engine, config: &config::AppConfig, surfaces: &surfaces::SurfacesUi, app: Option<&AppWindow>) {
    if let Some(devices) = surfaces.take_devices_to_apply(config) {
        let ports = engine.set_surface_devices(devices);
        surfaces.set_ports(&ports, config);
    }
    if let Some(app) = app {
        app.global::<AppState>().set_surface_summary(surfaces.summary(config).into());
    }
}

/// Gives the engine `slot`'s volume and mute.
fn set_slot_mix(engine: &mut Engine, slot: usize, slot_config: &config::SlotConfig) {
    engine.set_slot_gain(slot, slot_config.gain());
    engine.set_slot_muted(slot, slot_config.muted);
}

/// Gives the engine the master volume and mute.
fn set_master_mix(engine: &mut Engine, config: &config::AppConfig) {
    engine.set_master_gain(config.master_gain());
    engine.set_master_muted(config.master_muted);
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
        engine.set_slot_filter(slot, slot_config.filter(config.master_transpose));
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

fn refresh_plugin_folders(settings: &SettingsWindow, config: &config::AppConfig) {
    let folders: Vec<String> = config
        .extra_plugin_dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect();
    settings.set_extra_plugin_folders(string_model(folders));
}

/// Shows whether a plugin scan is running, in the plugin picker and Settings.
fn set_scanning(app: &AppWindow, settings: &SettingsWindow, scanning: bool) {
    app.global::<AppState>().set_scanning(scanning);
    settings.set_scanning(scanning);
}

/// Shows the metronome's settings in the sidebar.
fn show_metronome(app: &AppWindow, config: &metronome::MetronomeConfig, playing: bool) {
    let state = app.global::<AppState>();
    state.set_metronome_playing(playing);
    state.set_metronome_bpm(config.bpm.into());
    state.set_metronome_volume(config.volume);
    state.set_metronome_signature(config.signature as i32);
    state.set_metronome_beats(config.beats_per_bar().into());
    state.set_metronome_subdivision(config.subdivision as i32);
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
    fn start(&self, app: &AppWindow, settings: &SettingsWindow, extra_dirs: &[std::path::PathBuf]) {
        if self.scan.borrow().is_some() {
            return;
        }
        *self.scan.borrow_mut() = Some(PluginScan::start(extra_dirs));
        set_scanning(app, settings, true);
        self.timer.restart();
    }
}

/// Hands the plugin picker and the Settings window's list the plugins found by
/// the last scan.
fn refresh_plugin_list(settings: &SettingsWindow, engine: &Engine, picker: &Picker) {
    picker.set_plugins(engine.list_plugins());
    let names = picker
        .plugins
        .borrow()
        .iter()
        .map(|plugin| format!("{} — {} ({})", plugin.name, plugin.kind.label(), plugin.format.label()))
        .collect();
    settings.set_discovered_plugins(string_model(names));
}

fn log_on_err(result: anyhow::Result<()>) {
    if let Err(err) = result {
        log::error!("failed to save config: {err:#}");
    }
}
