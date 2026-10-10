slint::include_modules!();

mod config;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use live_keys_engine::{
    Engine, LoadedPluginMeta, MAX_EFFECTS, MAX_MIDI_EFFECTS, ParamInfo, ParamKind, PluginEditor,
    PluginInfo, PluginKey, PluginKind, SLOT_COUNT, Section, SlotChain, default_plugin_search_paths,
    list_logical_output_devices, list_midi_ports,
};
use live_keys_lv2_ui::{LaunchUiRequest, PluginUiHandle, UiKind, best_supported_ui, launch_ui};
use live_keys_vst3_ui::Vst3Editor;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

const MAX_BLOCK_SIZE: usize = 8192;
/// How often the plugin-GUI-closed check and param-change forwarding run, and VST3
/// editors get pumped (see `Vst3Editor::idle`).
const UI_POLL_INTERVAL: Duration = Duration::from_millis(16);
/// Sentinel entry always shown first in the audio device list, mapping to
/// `AppConfig.audio_device: None` — lets the OS pick the output device (e.g.
/// PipeWire's current default sink) instead of a specific one pinned by name.
/// Without this there's no way back to "no preference" once a specific device
/// has been selected once.
const SYSTEM_DEFAULT_LABEL: &str = "System Default";

/// A plugin's open GUI. LV2 GUIs run on a thread of their own; VST3 editors live
/// on this (the main) thread and need pumping.
enum OpenEditor {
    Lv2(PluginUiHandle),
    Vst3(Vst3Editor),
}

impl OpenEditor {
    fn is_closed(&self) -> bool {
        match self {
            OpenEditor::Lv2(handle) => handle.is_closed(),
            OpenEditor::Vst3(editor) => editor.is_closed(),
        }
    }

    fn close(&mut self) {
        match self {
            OpenEditor::Lv2(handle) => handle.request_close(),
            OpenEditor::Vst3(editor) => editor.close(),
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
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let plugins = self.plugins.borrow();
        let matches = plugins.iter().filter(|plugin| {
            let text = format!("{} {}", plugin.name, plugin.format.label()).to_lowercase();
            plugin.kind == kind && words.iter().all(|word| text.contains(word.as_str()))
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

/// A plugin's open parameter window, and the model its list shows.
struct ParamWindow {
    window: ParamsWindow,
    rows: Rc<VecModel<ParamRow>>,
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
    open_uis: RefCell<HashMap<PluginKey, OpenEditor>>,
    param_windows: RefCell<HashMap<PluginKey, ParamWindow>>,
    /// Last value sent per (plugin, parameter), so a newly opened parameter window
    /// shows where parameters actually are rather than their defaults.
    param_values: RefCell<HashMap<(PluginKey, u32), f32>>,
}

impl Rack {
    fn new() -> Self {
        fn models<T: 'static>() -> Vec<Rc<VecModel<T>>> {
            (0..SLOT_COUNT).map(|_| Rc::new(VecModel::default())).collect()
        }
        let (midi_effects, audio_effects) = (models(), models());
        let slots = (0..SLOT_COUNT)
            .map(|slot| SlotState {
                midi_effects: ModelRc::from(midi_effects[slot].clone()),
                audio_effects: ModelRc::from(audio_effects[slot].clone()),
                instrument_key: -1,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Self {
            slots: Rc::new(VecModel::from(slots)),
            midi_effects,
            audio_effects,
            open_uis: RefCell::new(HashMap::new()),
            param_windows: RefCell::new(HashMap::new()),
            param_values: RefCell::new(HashMap::new()),
        }
    }

    /// Recomputes `slot`'s chain and its plugins' GUI and parameter window state
    /// from the engine. `error`, if given, is shown as the slot's status line.
    fn refresh_slot(&self, engine: &Engine, slot: usize, error: Option<String>) {
        let empty = SlotChain::default();
        let chain = engine.slot(slot).unwrap_or(&empty);

        let open_uis = self.open_uis.borrow();
        let param_windows = self.param_windows.borrow();
        let effect_rows = |section: Section| -> Vec<EffectRow> {
            chain
                .effects(section)
                .iter()
                .map(|meta| EffectRow {
                    key: meta.key as i32,
                    name: meta.name.clone().into(),
                    gui_supported: gui_supported(meta),
                    gui_open: open_uis.contains_key(&meta.key),
                    params_open: param_windows.contains_key(&meta.key),
                })
                .collect()
        };
        self.midi_effects[slot].set_vec(effect_rows(Section::Midi));
        self.audio_effects[slot].set_vec(effect_rows(Section::Audio));

        let instrument = chain.instrument.as_ref();
        let mut row = self.slots.row_data(slot).unwrap();
        row.plugin_name = instrument.map(|meta| meta.name.as_str()).unwrap_or_default().into();
        row.instrument_key = instrument.map_or(-1, |meta| meta.key as i32);
        row.gui_supported = instrument.is_some_and(gui_supported);
        row.gui_open = instrument.is_some_and(|meta| open_uis.contains_key(&meta.key));
        row.params_open = instrument.is_some_and(|meta| param_windows.contains_key(&meta.key));
        row.status = error.unwrap_or_default().into();
        drop((open_uis, param_windows));
        self.slots.set_row_data(slot, row);
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
        let values = self.param_values.borrow();
        let rows: Vec<ParamRow> = meta
            .params
            .iter()
            .map(|param| {
                let value = values.get(&(key, param.id)).copied().unwrap_or(param.default_value);
                param_row(engine, key, param, value)
            })
            .collect();
        drop(values);
        let rows = Rc::new(VecModel::from(rows));

        let window = ParamsWindow::new()?;
        window.set_plugin_name(meta.name.as_str().into());
        window.set_params(ModelRc::from(rows.clone()));
        let app_weak = app.as_weak();
        window.on_set_param(move |param_id, value| {
            if let Some(app) = app_weak.upgrade() {
                app.global::<AppLogic>().invoke_set_param(key as i32, param_id, value);
            }
        });
        // Closing it by its title bar only hides it; `poll_uis` notices and
        // drops it.
        window.show()?;
        self.param_windows.borrow_mut().insert(key, ParamWindow { window, rows });
        Ok(())
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
    fn set_routing(&self, slot: usize, config: &config::SlotConfig) {
        let mut row = self.slots.row_data(slot).unwrap();
        row.midi_channel = config.midi_channel.map_or(0, |channel| i32::from(channel) + 1);
        row.note_low = config.note_low.into();
        row.note_high = config.note_high.into();
        row.velocity_low = config.velocity_low.into();
        row.velocity_high = config.velocity_high.into();
        row.volume_db = config.volume_db;
        self.slots.set_row_data(slot, row);
    }

    fn refresh_all(&self, engine: &Engine) {
        for slot in 0..SLOT_COUNT {
            self.refresh_slot(engine, slot, None);
        }
    }

    /// The value last sent for `key`'s parameter `param_id`, if any.
    fn param_value(&self, key: PluginKey, param_id: u32) -> Option<f32> {
        self.param_values.borrow().get(&(key, param_id)).copied()
    }

    /// Records a param value that was just sent to the engine, so the plugin's
    /// parameter window reflects it (now if open, or when it next opens).
    fn set_param_value(&self, engine: &Engine, key: PluginKey, param_id: u32, value: f32) {
        self.param_values.borrow_mut().insert((key, param_id), value);
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
    /// cached param values.
    fn forget(&self, key: PluginKey) {
        self.close_ui(key);
        self.close_params(key);
        self.param_values.borrow_mut().retain(|&(cached, _), _| cached != key);
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
        self.param_values.borrow_mut().clear();
    }

    /// Pumps open VST3 editors, and forgets any GUI or parameter window that has
    /// closed, refreshing its slot.
    fn poll_uis(&self, engine: &Engine) {
        let mut closed = Vec::new();
        for (&key, editor) in self.open_uis.borrow_mut().iter_mut() {
            if let OpenEditor::Vst3(editor) = editor {
                editor.idle();
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

/// Mirrors `slot`'s chain into the config, so the last setup is on record.
fn record_chain(config: &RefCell<config::AppConfig>, engine: &Engine, slot: usize) {
    let mut config = config.borrow_mut();
    if let Some(chain) = engine.slot(slot) {
        let slot_config = &mut config.slots[slot];
        slot_config.plugin_id = chain.instrument.as_ref().map(|meta| meta.id.clone());
        let ids = |section| chain.effects(section).iter().map(|meta| meta.id.clone()).collect();
        slot_config.midi_effect_ids = ids(Section::Midi);
        slot_config.effect_ids = ids(Section::Audio);
    }
    log_on_err(config::save(&config));
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut config = config::load();
    config.slots.resize_with(SLOT_COUNT, Default::default);
    let config = Rc::new(RefCell::new(config));
    // Set by edits that can fire continuously (a dragged fader), and saved by the
    // poll timer instead of writing the file on every step.
    let config_dirty = Rc::new(Cell::new(false));
    let engine = Rc::new(RefCell::new(Engine::new(MAX_BLOCK_SIZE)));
    for (slot, slot_config) in config.borrow().slots.iter().enumerate() {
        let mut engine = engine.borrow_mut();
        engine.set_slot_filter(slot, slot_config.filter());
        engine.set_slot_gain(slot, slot_config.gain());
    }
    {
        let extra_dirs = config.borrow().extra_plugin_dirs.clone();
        if !extra_dirs.is_empty() {
            engine.borrow_mut().rescan_plugins(&extra_dirs);
        }
    }
    // Param changes from a plugin's own open GUI arrive as `(key, param_id, value)`
    // — on its hosting thread for LV2, on this thread for VST3; they're forwarded
    // here and applied to the engine from this (the control) thread by the poll
    // timer below, since only one thread may drive the audio engine's command
    // queue.
    let (param_tx, param_rx) = mpsc::channel::<(PluginKey, u32, f32)>();
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

    let midi_ports = list_midi_ports().unwrap_or_default();
    state.set_selected_midi_index(index_of(&midi_ports, config.borrow().midi_port.as_deref()));
    state.set_midi_ports(string_model(midi_ports));

    state.set_default_plugin_folders(string_model(default_plugin_search_paths()));
    state.set_slots(ModelRc::from(rack.slots.clone()));
    state.set_max_midi_effects(MAX_MIDI_EFFECTS as i32);
    state.set_max_effects(MAX_EFFECTS as i32);
    state.set_picker_rows(ModelRc::from(picker.rows.clone()));
    refresh_plugin_folders(&app, &config.borrow());
    refresh_plugin_list(&app, &engine.borrow(), &picker);
    rack.refresh_all(&engine.borrow());
    for (slot, slot_config) in config.borrow().slots.iter().enumerate() {
        rack.set_routing(slot, slot_config);
    }
    state.set_min_volume_db(config::MIN_VOLUME_DB);
    state.set_max_volume_db(config::MAX_VOLUME_DB);

    let logic = app.global::<AppLogic>();

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_select_audio_device(move |value| {
            config.borrow_mut().audio_device = if value == SYSTEM_DEFAULT_LABEL {
                None
            } else {
                Some(value.to_string())
            };
            log_on_err(config::save(&config.borrow()));
            restart_engine(&engine, &config.borrow(), &rack);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_select_midi_port(move |value| {
            config.borrow_mut().midi_port = Some(value.to_string());
            log_on_err(config::save(&config.borrow()));
            restart_engine(&engine, &config.borrow(), &rack);
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

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        let picker = picker.clone();
        logic.on_rescan_plugins(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // Rescanning unloads every plugin, invalidating any open plugin GUI's
            // instance handle.
            rack.forget_all();
            engine
                .borrow_mut()
                .rescan_plugins(&config.borrow().extra_plugin_dirs);
            refresh_plugin_list(&app, &engine.borrow(), &picker);
            rack.refresh_all(&engine.borrow());
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
            let pinned = &mut config.pinned_plugins;
            match pinned.iter().position(|id| id == plugin_id.as_str()) {
                Some(index) => {
                    pinned.remove(index);
                }
                None => pinned.push(plugin_id.to_string()),
            }
            picker.refilter(pinned);
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
                    record_chain(&config, &engine.borrow(), slot);
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
        let config = config.clone();
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
            record_chain(&config, &engine.borrow(), slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let config = config.clone();
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
            record_chain(&config, &engine.borrow(), slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_move_effect(move |slot, section, from, to| {
            let slot = slot as usize;
            engine
                .borrow_mut()
                .move_effect(slot, section_from(section), from as usize, to as usize);
            record_chain(&config, &engine.borrow(), slot);
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
            let param_tx = param_tx.clone();
            // Fails only once the poll timer's receiver is gone, i.e. the app is
            // already shutting down — nothing useful to do about it, and this can
            // fire rapidly (e.g. a dragged knob), so it isn't logged.
            let forward_edit = move |param_id: u32, value: f32| {
                let _ = param_tx.send((key, param_id, value));
            };
            let opened = match &meta.editor {
                PluginEditor::Lv2 {
                    plugin,
                    features,
                    raw_handle,
                } => launch_ui(LaunchUiRequest {
                    plugin,
                    features: features.clone(),
                    instance_handle: *raw_handle,
                    port_write: Box::new(forward_edit),
                })
                .map(OpenEditor::Lv2),
                PluginEditor::Vst3(controller) => {
                    Vst3Editor::open(controller, &meta.name, Box::new(forward_edit))
                        .map(OpenEditor::Vst3)
                }
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
            if rack.param_value(key, param_id) == Some(value) {
                return;
            }
            engine.borrow_mut().set_param(key, param_id, value);
            rack.set_param_value(&engine.borrow(), key, param_id, value);
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
                // Channel 0 is omni; note/velocity values come from sliders already
                // bounded to 0..=127.
                slot_config.midi_channel = (1..=16).contains(&channel).then(|| channel as u8 - 1);
                slot_config.note_low = note_low.clamp(0, 127) as u8;
                slot_config.note_high = note_high.clamp(0, 127) as u8;
                slot_config.velocity_low = velocity_low.clamp(0, 127) as u8;
                slot_config.velocity_high = velocity_high.clamp(0, 127) as u8;
                engine.borrow_mut().set_slot_filter(slot, slot_config.filter());
                rack.set_routing(slot, slot_config);
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
            slot_config.volume_db = volume_db.clamp(config::MIN_VOLUME_DB, config::MAX_VOLUME_DB);
            engine.borrow_mut().set_slot_gain(slot, slot_config.gain());
            rack.set_routing(slot, slot_config);
            config_dirty.set(true);
        });
    }

    let poll_timer = slint::Timer::default();
    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        poll_timer.start(slint::TimerMode::Repeated, UI_POLL_INTERVAL, move || {
            rack.poll_uis(&engine.borrow());
            while let Ok((key, param_id, value)) = param_rx.try_recv() {
                engine.borrow_mut().send_param_change(key, param_id, value);
                rack.set_param_value(&engine.borrow(), key, param_id, value);
            }
            engine.borrow_mut().collect_retired();
            if config_dirty.replace(false) {
                log_on_err(config::save(&config.borrow()));
            }
        });
    }

    app.run()?;
    Ok(())
}

/// Runs `action` on the engine, first starting the audio/MIDI stream on the
/// configured devices if this is the first plugin loaded.
fn with_running_engine<T>(
    engine: &RefCell<Engine>,
    config: &config::AppConfig,
    action: impl FnOnce(&mut Engine) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut engine = engine.borrow_mut();
    if !engine.is_running() {
        engine.start(config.audio_device.as_deref(), config.midi_port.as_deref())?;
    }
    action(&mut engine)
}

/// Applies a changed audio/MIDI device selection to an already running engine.
/// Restarting re-instantiates every loaded plugin (with new keys), so open plugin
/// GUIs (holding the old instances' handles) are closed first. A no-op if nothing
/// has started the engine yet — the selection is picked up by the first load
/// instead.
fn restart_engine(engine: &RefCell<Engine>, config: &config::AppConfig, rack: &Rack) {
    if !engine.borrow().is_running() {
        return;
    }
    rack.forget_all();
    let result = engine
        .borrow_mut()
        .start(config.audio_device.as_deref(), config.midi_port.as_deref());
    rack.refresh_all(&engine.borrow());
    if let Err(err) = result {
        log::error!("failed to restart audio with the new device selection: {err:#}");
        for slot in 0..SLOT_COUNT {
            rack.refresh_slot(&engine.borrow(), slot, Some(format!("restart failed: {err}")));
        }
    }
}

/// Index of `value` in `items`, or `-1` (ComboBox's "nothing selected") if absent.
fn index_of(items: &[String], value: Option<&str>) -> i32 {
    value
        .and_then(|value| items.iter().position(|item| item == value))
        .map_or(-1, |index| index as i32)
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
