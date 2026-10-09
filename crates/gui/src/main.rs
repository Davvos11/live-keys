slint::include_modules!();

mod config;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use live_keys_engine::{
    Engine, PluginEditor, SLOT_COUNT, default_plugin_search_paths, list_logical_output_devices,
    list_midi_ports,
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

/// A slot's open plugin GUI. LV2 GUIs run on a thread of their own; VST3 editors
/// live on this (the main) thread and need pumping.
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

/// GUI-side state of the Live tab's slots: the models backing `AppState.slots`,
/// and each slot's open plugin GUI session, if any.
struct Rack {
    slots: Rc<VecModel<SlotState>>,
    /// One per slot, kept for the app's lifetime and refilled on load, so
    /// refreshing a slot's other fields doesn't hand Slint a new model (and
    /// rebuild that slot's sliders).
    ports: Vec<Rc<VecModel<ParamRow>>>,
    open_uis: RefCell<Vec<Option<OpenEditor>>>,
}

impl Rack {
    fn new() -> Self {
        let ports: Vec<_> = (0..SLOT_COUNT).map(|_| Rc::new(VecModel::default())).collect();
        let slots = ports
            .iter()
            .map(|ports| SlotState {
                params: ModelRc::from(ports.clone()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Self {
            slots: Rc::new(VecModel::from(slots)),
            ports,
            open_uis: RefCell::new((0..SLOT_COUNT).map(|_| None).collect()),
        }
    }

    /// Recomputes `slot`'s name and GUI state from the engine. `error`, if given,
    /// is shown in place of the usual status line.
    fn refresh_slot(&self, engine: &Engine, slot: usize, error: Option<String>) {
        let meta = engine.slot(slot);
        let (gui_supported, status) = match meta.map(|meta| &meta.editor) {
            None => (false, ""),
            Some(PluginEditor::Lv2 { plugin, .. }) => match best_supported_ui(plugin) {
                Some((_, UiKind::ExternalUi | UiKind::X11Ui)) => (true, ""),
                None => (false, "this plugin has no GUI"),
            },
            // Whether a VST3 plugin has an editor is only known by trying to
            // create one; a failure shows up as the open error.
            Some(PluginEditor::Vst3(_)) => (true, ""),
        };
        let mut row = self.slots.row_data(slot).unwrap();
        row.plugin_name = meta.map(|meta| meta.name.as_str()).unwrap_or_default().into();
        row.gui_supported = gui_supported;
        row.gui_open = self.open_uis.borrow()[slot].is_some();
        row.status = error.unwrap_or_else(|| status.to_string()).into();
        self.slots.set_row_data(slot, row);
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
            self.reload_ports(engine, slot);
            self.refresh_slot(engine, slot, None);
        }
    }

    /// Refills `slot`'s parameter list from the plugin now loaded there, at
    /// default values.
    fn reload_ports(&self, engine: &Engine, slot: usize) {
        let rows: Vec<ParamRow> = engine
            .slot(slot)
            .map(|meta| meta.params.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|param| ParamRow {
                param_id: param.id as i32,
                name: param.name.clone().into(),
                min_value: param.min_value,
                max_value: param.max_value,
                value: param.default_value,
            })
            .collect();
        self.ports[slot].set_vec(rows);
    }

    /// Records a param value that was just sent to the engine, so the slot's
    /// slider reflects it.
    fn set_param_value(&self, slot: usize, param_id: u32, value: f32) {
        let ports = &self.ports[slot];
        if let Some(row) = ports.iter().position(|port| port.param_id as u32 == param_id) {
            let mut port = ports.row_data(row).unwrap();
            port.value = value;
            ports.set_row_data(row, port);
        }
    }

    /// Closes `slot`'s plugin GUI. For an LV2 GUI this is only a best-effort
    /// signal in case its hosting thread is still responsive; either way, our own
    /// bookkeeping is cleared unconditionally so the user can try a fresh session
    /// even if the previous one's plugin code has wedged and will never confirm
    /// closing itself (observed with Helm's own UI). The old thread, if genuinely
    /// stuck, is abandoned — it leaks until the app exits, which beats leaving the
    /// button permanently disabled. A VST3 editor closes synchronously.
    fn close_ui(&self, slot: usize) {
        let editor = self.open_uis.borrow_mut()[slot].take();
        if let Some(mut editor) = editor {
            editor.close();
        }
    }

    /// Pumps open VST3 editors, and forgets any GUI that has closed, refreshing
    /// its slot.
    fn poll_uis(&self, engine: &Engine) {
        for slot in 0..SLOT_COUNT {
            let closed = {
                let mut open_uis = self.open_uis.borrow_mut();
                if let Some(OpenEditor::Vst3(editor)) = &mut open_uis[slot] {
                    editor.idle();
                }
                open_uis[slot].as_ref().is_some_and(OpenEditor::is_closed)
            };
            if closed {
                self.open_uis.borrow_mut()[slot] = None;
                self.refresh_slot(engine, slot, None);
            }
        }
    }

    fn close_all_uis(&self) {
        for slot in 0..SLOT_COUNT {
            self.close_ui(slot);
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
    // Param changes from a plugin's own open GUI arrive as `(slot, param_id,
    // value)` — on its hosting thread for LV2, on this thread for VST3; they're forwarded here and applied to the engine
    // from this (the control) thread by the poll timer below, since only one thread
    // may drive the audio engine's command queue.
    let (param_tx, param_rx) = mpsc::channel::<(usize, u32, f32)>();
    let rack = Rc::new(Rack::new());

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
    refresh_plugin_folders(&app, &config.borrow());
    refresh_plugin_list(&app, &engine.borrow());
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
        logic.on_rescan_plugins(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // Rescanning unloads every slot, invalidating any open plugin GUI's
            // instance handle.
            rack.close_all_uis();
            engine
                .borrow_mut()
                .rescan_plugins(&config.borrow().extra_plugin_dirs);
            refresh_plugin_list(&app, &engine.borrow());
            rack.refresh_all(&engine.borrow());
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_load_plugin(move |slot, plugin_id| {
            let slot = slot as usize;
            // The slot's current plugin instance is about to be replaced, and an open
            // GUI holds its raw instance handle.
            rack.close_ui(slot);
            let result =
                load_into_slot(&mut engine.borrow_mut(), &config.borrow(), slot, &plugin_id);
            let error = match result {
                Ok(()) => {
                    let mut config = config.borrow_mut();
                    config.slots[slot].plugin_id = Some(plugin_id.to_string());
                    log_on_err(config::save(&config));
                    None
                }
                Err(err) => {
                    log::error!("failed to load plugin '{plugin_id}' into slot {slot}: {err:#}");
                    Some(format!("load failed: {err}"))
                }
            };
            rack.reload_ports(&engine.borrow(), slot);
            rack.refresh_slot(&engine.borrow(), slot, error);
        });
    }

    {
        let config = config.clone();
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_unload_plugin(move |slot| {
            let slot = slot as usize;
            rack.close_ui(slot);
            engine.borrow_mut().unload_slot(slot);
            {
                let mut config = config.borrow_mut();
                config.slots[slot].plugin_id = None;
                log_on_err(config::save(&config));
            }
            rack.reload_ports(&engine.borrow(), slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_open_plugin_gui(move |slot| {
            let slot = slot as usize;
            if rack.open_uis.borrow()[slot].is_some() {
                return;
            }
            let engine = engine.borrow();
            let Some(meta) = engine.slot(slot) else {
                return;
            };
            let param_tx = param_tx.clone();
            // Fails only once the poll timer's receiver is gone, i.e. the app is
            // already shutting down — nothing useful to do about it, and this can
            // fire rapidly (e.g. a dragged knob), so it isn't logged.
            let forward_edit = move |param_id: u32, value: f32| {
                let _ = param_tx.send((slot, param_id, value));
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
                    rack.open_uis.borrow_mut()[slot] = Some(editor);
                    None
                }
                Err(err) => {
                    log::error!("failed to open plugin GUI for slot {slot}: {err:#}");
                    Some(format!("failed to open GUI: {err}"))
                }
            };
            rack.refresh_slot(&engine, slot, error);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_close_plugin_gui(move |slot| {
            let slot = slot as usize;
            rack.close_ui(slot);
            rack.refresh_slot(&engine.borrow(), slot, None);
        });
    }

    {
        let engine = engine.clone();
        let rack = rack.clone();
        logic.on_set_param(move |slot, param_id, value| {
            let (slot, param_id) = (slot as usize, param_id as u32);
            engine.borrow_mut().set_param(slot, param_id, value);
            rack.set_param_value(slot, param_id, value);
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
            while let Ok((slot, param_id, value)) = param_rx.try_recv() {
                engine.borrow_mut().send_param_change(slot, param_id, value);
                rack.set_param_value(slot, param_id, value);
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

/// Loads `uri` into `slot`, first starting the audio/MIDI stream on the configured
/// devices if this is the first plugin loaded.
fn load_into_slot(
    engine: &mut Engine,
    config: &config::AppConfig,
    slot: usize,
    uri: &str,
) -> anyhow::Result<()> {
    if !engine.is_running() {
        engine.start(config.audio_device.as_deref(), config.midi_port.as_deref())?;
    }
    engine.load_into_slot(slot, uri)
}

/// Applies a changed audio/MIDI device selection to an already running engine.
/// Restarting re-instantiates every loaded plugin, so open plugin GUIs (holding the
/// old instances' handles) are closed first. A no-op if nothing has started the
/// engine yet — the selection is picked up by the first load instead.
fn restart_engine(engine: &RefCell<Engine>, config: &config::AppConfig, rack: &Rack) {
    if !engine.borrow().is_running() {
        return;
    }
    rack.close_all_uis();
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

fn refresh_plugin_list(app: &AppWindow, engine: &Engine) {
    let mut plugins: Vec<_> = engine
        .list_plugins()
        .into_iter()
        .filter(|plugin| plugin.is_instrument)
        .collect();
    // By name, so a plugin installed in more than one format lists its formats
    // side by side.
    plugins.sort_by_key(|plugin| plugin.name.to_lowercase());
    let rows: Vec<PluginRow> = plugins
        .into_iter()
        .map(|plugin| PluginRow {
            id: plugin.id.into(),
            name: format!("{} ({})", plugin.name, plugin.format.label()).into(),
        })
        .collect();
    let names = rows.iter().map(|row| row.name.to_string()).collect();
    let state = app.global::<AppState>();
    state.set_plugins(ModelRc::new(VecModel::from(rows)));
    state.set_plugin_names(string_model(names));
}

fn log_on_err(result: anyhow::Result<()>) {
    if let Err(err) = result {
        log::error!("failed to save config: {err:#}");
    }
}
