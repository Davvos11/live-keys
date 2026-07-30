slint::include_modules!();

mod config;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use live_keys_engine::{
    Engine, default_plugin_search_paths, list_logical_output_devices, list_midi_ports,
};
use live_keys_lv2_ui::{LaunchUiRequest, PluginUiHandle, UiKind, best_supported_ui, launch_ui};
use slint::{ComponentHandle, ModelRc, VecModel};

const MAX_BLOCK_SIZE: usize = 8192;
/// How often the plugin-GUI-closed check and param-change forwarding run.
const UI_POLL_INTERVAL: Duration = Duration::from_millis(16);

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = Rc::new(RefCell::new(config::load()));
    let engine = Rc::new(RefCell::new(Engine::new(MAX_BLOCK_SIZE)));
    {
        let extra_dirs = config.borrow().extra_plugin_dirs.clone();
        if !extra_dirs.is_empty() {
            engine.borrow_mut().rescan_plugins(&extra_dirs);
        }
    }
    // Param changes from a plugin's own open GUI arrive on its hosting thread;
    // they're forwarded here and applied to the engine from this (the control)
    // thread by the poll timer below, since only one thread may drive the audio
    // engine's param queue.
    let (param_tx, param_rx) = mpsc::channel::<(u32, f32)>();
    let open_plugin_ui: Rc<RefCell<Option<PluginUiHandle>>> = Rc::new(RefCell::new(None));

    let app = AppWindow::new()?;
    let state = app.global::<AppState>();

    let audio_devices = list_logical_output_devices().unwrap_or_default();
    state.set_selected_audio_index(index_of(&audio_devices, config.borrow().audio_device.as_deref()));
    state.set_audio_devices(string_model(audio_devices));

    let midi_ports = list_midi_ports().unwrap_or_default();
    state.set_selected_midi_index(index_of(&midi_ports, config.borrow().midi_port.as_deref()));
    state.set_midi_ports(string_model(midi_ports));

    state.set_default_plugin_folders(string_model(default_plugin_search_paths()));
    refresh_plugin_folders(&app, &config.borrow());
    refresh_plugin_list(&app, &engine.borrow());
    refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);

    let logic = app.global::<AppLogic>();

    {
        let config = config.clone();
        logic.on_select_audio_device(move |value| {
            config.borrow_mut().audio_device = Some(value.to_string());
            log_on_err(config::save(&config.borrow()));
        });
    }

    {
        let config = config.clone();
        logic.on_select_midi_port(move |value| {
            config.borrow_mut().midi_port = Some(value.to_string());
            log_on_err(config::save(&config.borrow()));
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
        let open_plugin_ui = open_plugin_ui.clone();
        logic.on_rescan_plugins(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            engine
                .borrow_mut()
                .rescan_plugins(&config.borrow().extra_plugin_dirs);
            refresh_plugin_list(&app, &engine.borrow());
            refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);
        });
    }

    {
        let app_weak = app.as_weak();
        let config = config.clone();
        let engine = engine.clone();
        let open_plugin_ui = open_plugin_ui.clone();
        logic.on_load_plugin(move |uri| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let audio_filter = config.borrow().audio_device.clone();
            let midi_filter = config.borrow().midi_port.clone();
            // Bound to a local first: `match engine.borrow_mut()....{ }` would keep
            // that mutable borrow alive for the whole match body (the scrutinee's
            // temporary lifetime extends to the end of the match), causing the
            // `engine.borrow()` calls below to panic.
            let result =
                engine
                    .borrow_mut()
                    .load_and_start(&uri, audio_filter.as_deref(), midi_filter.as_deref());
            match result {
                Ok(()) => {
                    config.borrow_mut().last_plugin_uri = Some(uri.to_string());
                    log_on_err(config::save(&config.borrow()));
                    app.global::<AppState>().set_loaded_plugin_name(
                        engine.borrow().loaded_plugin_name().unwrap_or_default().into(),
                    );
                    refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);
                    refresh_control_ports(&app, &engine.borrow());
                }
                Err(err) => {
                    log::error!("failed to load plugin '{uri}': {err:#}");
                    app.global::<AppState>().set_loaded_plugin_name("".into());
                    app.global::<AppState>().set_plugin_gui_supported(false);
                    app.global::<AppState>()
                        .set_plugin_gui_status(format!("load failed: {err}").into());
                    app.global::<AppState>().set_control_ports(ModelRc::new(VecModel::from(Vec::new())));
                }
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let open_plugin_ui = open_plugin_ui.clone();
        logic.on_open_plugin_gui(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            if open_plugin_ui.borrow().is_some() {
                return;
            }
            let engine_ref = engine.borrow();
            let Some(meta) = engine_ref.loaded_plugin_meta() else {
                return;
            };
            let param_tx = param_tx.clone();
            let request = LaunchUiRequest {
                plugin: &meta.plugin,
                features: meta.features.clone(),
                instance_handle: meta.raw_handle,
                port_write: Box::new(move |port_index, value| {
                    // Fails only once the poll timer's receiver is gone, i.e. the
                    // app is already shutting down — nothing useful to do about it,
                    // and this can fire rapidly (e.g. a dragged knob), so it isn't
                    // logged.
                    let _ = param_tx.send((port_index, value));
                }),
            };
            match launch_ui(request) {
                Ok(handle) => {
                    *open_plugin_ui.borrow_mut() = Some(handle);
                    drop(engine_ref);
                    refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);
                }
                Err(err) => {
                    log::error!("failed to open plugin GUI: {err:#}");
                    app.global::<AppState>()
                        .set_plugin_gui_status(format!("failed to open: {err}").into());
                }
            }
        });
    }

    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let open_plugin_ui = open_plugin_ui.clone();
        logic.on_close_plugin_gui(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // Best-effort signal in case the hosting thread is still responsive;
            // either way, our own bookkeeping is cleared unconditionally below so
            // the user can try a fresh session even if the previous one's plugin
            // code has wedged and will never confirm closing itself (observed with
            // Helm's own UI). The old thread, if genuinely stuck, is abandoned —
            // it leaks until the app exits, which beats leaving the button
            // permanently disabled.
            if let Some(mut handle) = open_plugin_ui.borrow_mut().take() {
                handle.request_close();
            }
            refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);
        });
    }

    {
        let engine = engine.clone();
        logic.on_set_control_param(move |port_index, value| {
            engine.borrow_mut().send_param_change(port_index as u32, value);
        });
    }

    let poll_timer = slint::Timer::default();
    {
        let app_weak = app.as_weak();
        let engine = engine.clone();
        let open_plugin_ui = open_plugin_ui.clone();
        poll_timer.start(slint::TimerMode::Repeated, UI_POLL_INTERVAL, move || {
            while let Ok((port_index, value)) = param_rx.try_recv() {
                engine.borrow_mut().send_param_change(port_index, value);
            }
            let mut open_ui = open_plugin_ui.borrow_mut();
            if open_ui.as_ref().is_some_and(PluginUiHandle::is_closed) {
                *open_ui = None;
                drop(open_ui);
                if let Some(app) = app_weak.upgrade() {
                    refresh_plugin_gui_state(&app, &engine.borrow(), &open_plugin_ui);
                }
            }
        });
    }

    app.run()?;
    Ok(())
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
    let rows: Vec<PluginRow> = engine
        .list_plugins()
        .into_iter()
        .filter(|plugin| plugin.is_instrument)
        .map(|plugin| PluginRow {
            uri: plugin.uri.into(),
            name: plugin.name.into(),
        })
        .collect();
    app.global::<AppState>().set_plugins(ModelRc::new(VecModel::from(rows)));
}

fn refresh_control_ports(app: &AppWindow, engine: &Engine) {
    let rows: Vec<ControlPortRow> = engine
        .control_ports()
        .iter()
        .map(|port| ControlPortRow {
            port_index: port.port_index as i32,
            name: port.name.clone().into(),
            min_value: port.min_value,
            max_value: port.max_value,
            default_value: port.default_value,
        })
        .collect();
    app.global::<AppState>().set_control_ports(ModelRc::new(VecModel::from(rows)));
}

/// Recomputes whether "Open Plugin GUI" should be enabled (and why not if it
/// isn't), and whether a plugin GUI session is currently considered open.
fn refresh_plugin_gui_state(
    app: &AppWindow,
    engine: &Engine,
    open_plugin_ui: &RefCell<Option<PluginUiHandle>>,
) {
    let state = app.global::<AppState>();
    let (supported, status) = match engine.loaded_plugin_meta() {
        None => (false, "no plugin loaded"),
        Some(meta) => match best_supported_ui(&meta.plugin) {
            Some((_, UiKind::ExternalUi | UiKind::X11Ui)) => (true, ""),
            None => (false, "this plugin has no GUI"),
        },
    };
    state.set_plugin_gui_supported(supported);
    state.set_plugin_gui_status(status.into());
    state.set_plugin_gui_open(open_plugin_ui.borrow().is_some());
}

fn log_on_err(result: anyhow::Result<()>) {
    if let Err(err) = result {
        log::error!("failed to save config: {err:#}");
    }
}
