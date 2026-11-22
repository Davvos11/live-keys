//! MIDI learn's glue between the mapping thread, the mappings
//! ([`mappings::MappingsUi`]) and the window: the reports the poll timer
//! hands over (surface events, targets mappings moved, ghosts), building the
//! setup again when what it covers changes, keeping the mapping thread told
//! of targets moved elsewhere, and learn mode's callbacks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use live_keys_engine::{Engine, MASTER_SLOT, MapperReport, Output, PluginKey, PortEvent, SLOT_COUNT};
use live_keys_lv2_ui::PortMessage;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::config::{self, AppConfig};
use crate::mappings::{self, Concrete, Ctx, LearnFlags, MappingMode};
use crate::surface_editor::{BankMessages, Editor, Listening, MessageType, pressure_index, pressure_of};
use crate::surfaces::{SurfacesUi, control_views};
use crate::{AppLogic, AppState, AppWindow, EditBank, EditBankSettings, OpenEditor, Rack, log_on_err, sync_surfaces};

/// What MIDI learn works with.
pub struct Learn {
    pub engine: Rc<RefCell<Engine>>,
    pub config: Rc<RefCell<AppConfig>>,
    pub config_dirty: Rc<Cell<bool>>,
    pub rack: Rc<Rack>,
    pub surfaces: Rc<SurfacesUi>,
    pub metronome_playing: Rc<Cell<bool>>,
    pub app: slint::Weak<AppWindow>,
    /// The surface being edited, while the editor is open.
    pub editor: RefCell<Option<Editor>>,
    /// The editor's banks and controls, refilled in place: a bank's handle
    /// being dragged mustn't be made anew.
    pub edit_banks: Rc<VecModel<EditBank>>,
    pub edit_controls: Rc<VecModel<crate::SurfaceControlView>>,
}

/// A mapping moved a target: as reported, the latest per target.
struct Applied {
    generation: u64,
    target: usize,
    output: Output,
    position: f32,
    base: f32,
    value: f32,
    for_gui: Vec<PortEvent>,
}

impl Learn {
    /// Runs `work` with what mappings need to know.
    fn with_ctx<T>(&self, work: impl FnOnce(&Ctx) -> T) -> T {
        let engine = self.engine.borrow();
        let config = self.config.borrow();
        let panels = self.rack.panels.borrow();
        let ctx = Ctx {
            engine: &engine,
            config: &config,
            panels: panels.iter().map(|panel| panel.as_ref().map(|panel| (panel.key, &panel.panel))).collect(),
            metronome_playing: self.metronome_playing.get(),
        };
        work(&ctx)
    }

    /// The poll timer's part: what the mapping thread did, then the setup and
    /// the view if they need it.
    pub fn poll(&self) {
        let reports = self.engine.borrow_mut().take_mapper_reports();
        let mut events = Vec::new();
        let mut applied: Vec<Applied> = Vec::new();
        for report in reports {
            match report {
                MapperReport::Event(event) => events.push(event),
                MapperReport::Applied { generation, target, output, position, base, value, for_gui } => {
                    applied.retain(|a| (a.generation, a.target) != (generation, target));
                    applied.push(Applied { generation, target, output, position, base, value, for_gui });
                }
                MapperReport::Feedback { device, control, shown } => self.surfaces.show_sent(&device, control, shown),
                MapperReport::Waiting { generation, device, control, at } => {
                    if generation == self.rack.mappings.generation() {
                        self.surfaces.set_waiting(&device, control, at);
                    }
                }
            }
        }
        // The surface editor learns from its device's events.
        let learned = {
            let mut editor = self.editor.borrow_mut();
            editor.as_mut().is_some_and(|editor| {
                let mut changed = false;
                let device = editor.device.clone();
                for event in &events {
                    if let Some(port) = self.surfaces.port_of_device(&event.device, &device) {
                        changed |= editor.event(port, &event.event);
                    }
                }
                changed
            })
        };
        if learned {
            self.refresh_editor();
        }
        let moved = self.surfaces.apply(&events);
        if self.rack.mappings.learning()
            && let Some((device, control)) = moved
            && let Some(surface) = self.surfaces.surface_of(&device)
        {
            self.rack.mappings.select(&device, control, surface);
        }
        for applied in applied {
            self.apply(applied);
        }
        let surfaces_changed = self.surfaces.take_changed();
        if surfaces_changed {
            // A surface shown for the first time brings its default mappings.
            let mut config = self.config.borrow_mut();
            for shown in self.surfaces.shown_surfaces() {
                if self.rack.mappings.install_defaults(&shown.surface, &mut config) {
                    self.config_dirty.set(true);
                }
            }
        }
        if surfaces_changed {
            self.rack.mappings.setup_dirty.set(true);
            self.rack.mappings.view_dirty.set(true);
        }
        if self.rack.mappings.setup_dirty.get() {
            self.rebuild_setup();
        }
        // A device plugged in again: its controls that show values get them.
        if surfaces_changed {
            self.engine.borrow().resync_feedback();
        }
        self.with_ctx(|ctx| self.rack.mappings.sync_positions(ctx));
        if self.rack.mappings.view_dirty.get() {
            self.refresh_view();
        }
    }

    /// Shows (and for the GUI's own targets, does) what a mapping did.
    fn apply(&self, applied: Applied) {
        let Some(concrete) = self.rack.mappings.concrete(applied.generation, applied.target) else { return };
        // First, so a modulated knob shows its new base.
        self.rack.mappings.reported(applied.generation, applied.target, applied.base);
        let app = self.app.upgrade();
        let logic = app.as_ref().map(|app| app.global::<AppLogic>());
        let position = applied.position;
        match concrete {
            Concrete::Plugin { key, .. } => self.show_plugin_value(key, &applied),
            Concrete::Volume(slot) => {
                // The config (and so scenes) keeps the base; the engine plays
                // the effective volume already.
                let db = mappings::volume_db(applied.base);
                let mut config = self.config.borrow_mut();
                if slot == MASTER_SLOT {
                    config.master_volume_db = db;
                    if let Some(app) = &app {
                        app.global::<AppState>().set_master_volume_db(db);
                    }
                } else if slot < config.slots.len() {
                    if config.slots[slot].volume_db != db {
                        self.rack.mark_scene_changed(slot);
                    }
                    config.slots[slot].volume_db = db;
                    let inputs = config.midi_inputs.len();
                    self.rack.set_routing(slot, &config.slots[slot], inputs);
                }
                self.config_dirty.set(true);
            }
            Concrete::Mute(slot) => {
                let on = position >= 0.5;
                let muted = {
                    let config = self.config.borrow();
                    if slot == MASTER_SLOT { config.master_muted } else { config.slots.get(slot).is_some_and(|s| s.muted) }
                };
                if let Some(logic) = &logic
                    && muted != on
                {
                    if slot == MASTER_SLOT {
                        logic.invoke_toggle_master_mute();
                    } else {
                        logic.invoke_toggle_slot_mute(slot as i32);
                    }
                }
            }
            Concrete::Transpose(slot) => {
                if let Some(logic) = &logic {
                    let semitones = mappings::transpose(position);
                    if slot == MASTER_SLOT {
                        logic.invoke_set_master_transpose(semitones);
                    } else {
                        logic.invoke_set_slot_transpose(slot as i32, semitones);
                    }
                }
            }
            Concrete::MetronomeVolume => {
                if let Some(logic) = &logic {
                    logic.invoke_set_metronome_volume(position);
                }
            }
            Concrete::MetronomePlay => {
                if let Some(logic) = &logic
                    && self.metronome_playing.get() != (position >= 0.5)
                {
                    logic.invoke_toggle_metronome();
                }
            }
            Concrete::Tempo => {
                if let Some(logic) = &logic {
                    logic.invoke_set_metronome_bpm(mappings::tempo(position) as i32);
                }
            }
            // Actions: on the press.
            Concrete::SelectSlot(step) => {
                if let Some(logic) = &logic
                    && position >= 0.5
                {
                    let selected = self.config.borrow().selected_slot as i32;
                    logic.invoke_select_slot((selected + step).rem_euclid(SLOT_COUNT as i32));
                }
            }
            Concrete::InstrumentPreset(step) => {
                if let Some(logic) = &logic
                    && position >= 0.5
                {
                    let selected = self.config.borrow().selected_slot as i32;
                    logic.invoke_step_instrument_preset(selected, step);
                }
            }
        }
        self.with_ctx(|ctx| self.rack.mappings.saw(ctx, applied.target));
    }

    /// A mapping set a plugin's parameter or setBfree function (the engine
    /// has recorded it): its panel, parameter window and open GUI follow.
    fn show_plugin_value(&self, key: PluginKey, applied: &Applied) {
        let engine = self.engine.borrow();
        match &applied.output {
            Output::Param { param, .. } => {
                self.rack.show_param_value(&engine, key, *param, applied.value);
                self.rack.notify_ui(key, *param, applied.value);
            }
            Output::Setbfree { .. } => {
                // setBfree doesn't report what the host sets: its GUI gets
                // the change as if the plugin had.
                if let Some(OpenEditor::Lv2(handle)) = self.rack.open_uis.borrow().get(&key) {
                    for event in &applied.for_gui {
                        handle.notify(PortMessage::Atom { port: event.port, atom: event.to_atom() });
                    }
                }
                self.rack.refresh_panel_of(&engine, key);
            }
            _ => {}
        }
        self.rack.mark_changed(&engine, key);
    }

    /// Hands the engine a new setup of every mapping that applies now.
    fn rebuild_setup(&self) {
        self.rack.mappings.forget_missing(&self.engine.borrow());
        let surfaces = self.surfaces.shown_surfaces();
        let (setup, installed) = self.with_ctx(|ctx| self.rack.mappings.build_setup(ctx, &surfaces));
        if self.rack.mappings.unchanged(&installed) {
            return self.rack.mappings.keep_setup();
        }
        let generation = self.engine.borrow_mut().set_mappings(setup);
        self.rack.mappings.install(installed, generation);
        self.surfaces.clear_waiting();
        // Knobs show their modulation, or no longer.
        for slot in 0..SLOT_COUNT {
            self.rack.refresh_panel(&self.engine.borrow(), slot, false);
        }
        self.rack.mappings.view_dirty.set(true);
    }

    /// Shows learn mode as it is: the pane, the popup, the outlines, and the
    /// panel's selected and mapped controls.
    pub fn refresh_view(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let state = app.global::<AppState>();
        let mappings = &self.rack.mappings;
        let (popup, expanded, flags) = self.with_ctx(|ctx| {
            let (popup, expanded) = mappings.refresh_view(ctx);
            (popup, expanded, mappings.flags(ctx))
        });
        state.set_learning(mappings.learning());
        state.set_learn_control(mappings.control_view());
        state.set_learn_expanded(expanded);
        match popup {
            Some((row, assigned)) => {
                state.set_learn_popup(row);
                state.set_learn_popup_assigned(assigned);
            }
            None => state.set_learn_popup_assigned(false),
        }
        self.show_flags(&state, flags);
        self.surfaces.set_learn(mappings.selected().as_ref(), &mappings.mapped());
    }

    /// Puts the learn outlines' states into the slots' rows and `AppState`,
    /// where they changed.
    fn show_flags(&self, state: &AppState, flags: LearnFlags) {
        let mut shown = self.rack.learn_flags.borrow_mut();
        if *shown == flags {
            return;
        }
        let model = |values: &[i32]| ModelRc::new(VecModel::from(values.to_vec()));
        for slot in 0..SLOT_COUNT {
            let same = shown.cells.get(slot) == flags.cells.get(slot)
                && shown.drawbars.get(slot) == flags.drawbars.get(slot)
                && shown.volume.get(slot) == flags.volume.get(slot)
                && shown.mute.get(slot) == flags.mute.get(slot)
                && shown.transpose.get(slot) == flags.transpose.get(slot);
            if same {
                continue;
            }
            let Some(mut row) = self.rack.slots.row_data(slot) else { continue };
            row.cell_learn = model(&flags.cells[slot]);
            row.drawbar_learn = model(&flags.drawbars[slot]);
            row.volume_learn = flags.volume[slot];
            row.mute_learn = flags.mute[slot];
            row.transpose_learn = flags.transpose[slot];
            self.rack.slots.set_row_data(slot, row);
        }
        state.set_master_volume_learn(flags.volume[MASTER_SLOT]);
        state.set_master_mute_learn(flags.mute[MASTER_SLOT]);
        state.set_master_transpose_learn(flags.transpose[MASTER_SLOT]);
        state.set_metronome_volume_learn(flags.metronome_volume);
        state.set_metronome_play_learn(flags.metronome_play);
        state.set_tempo_learn(flags.tempo);
        *shown = flags;
    }

    /// After a mapping change: the config saved if it changed, the slot's
    /// presets and scene marked changed if its scene mappings did.
    fn changed(&self, (config_changed, slot): (bool, Option<usize>)) {
        if config_changed {
            let mut config = self.config.borrow_mut();
            self.rack.mappings.store(&mut config);
            log_on_err(config::save(&config));
        }
        if let Some(slot) = slot {
            self.rack.mark_slot_changed(slot);
        }
    }

    /// Edits row `row`'s mode settings.
    fn edit(&self, row: i32, change: impl FnOnce(&mut MappingMode, &[f32])) {
        let changed = self.with_ctx(|ctx| self.rack.mappings.edit_mode(ctx, row, change));
        self.changed(changed);
        self.refresh_view();
    }

    /// Hands learn mode's callbacks to the window.
    pub fn connect(self: &Rc<Self>, logic: &AppLogic) {
        let learn = self.clone();
        logic.on_set_learning(move |on| {
            learn.rack.mappings.set_learning(on);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_select(move |device, control| {
            if let Some(surface) = learn.surfaces.surface_of(&device) {
                learn.rack.mappings.select(&device, control as usize, surface);
                learn.refresh_view();
            }
        });
        let learn = self.clone();
        logic.on_surface_page(move |device, page| {
            learn.surfaces.set_page(&device, page.max(0) as usize);
        });
        let learn = self.clone();
        logic.on_surface_variant(move |device, step| {
            let mut config = learn.config.borrow_mut();
            if learn.surfaces.step_variant(&device, step, &mut config) {
                learn.surfaces.refresh(&config);
                learn.config_dirty.set(true);
            }
        });
        let learn = self.clone();
        logic.on_toggle_surface_driver(move |device| {
            let mut config = learn.config.borrow_mut();
            let off = &mut config.surfaces.without_driver;
            match off.iter().position(|known| *known == device.as_str()) {
                Some(index) => drop(off.remove(index)),
                None => off.push(device.to_string()),
            }
            learn.surfaces.refresh(&config);
            learn.config_dirty.set(true);
        });
        let learn = self.clone();
        logic.on_learn_click(move |kind, slot, index| {
            let changed = learn.with_ctx(|ctx| learn.rack.mappings.click(ctx, kind, slot as usize, index as usize));
            learn.changed(changed);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_menu(move |kind, slot, index| {
            // Shown straight after this returns: filled now, not on the next tick.
            learn.with_ctx(|ctx| learn.rack.mappings.open_popup(ctx, kind, slot as usize, index as usize));
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_assign_popup(move || {
            let changed = learn.with_ctx(|ctx| learn.rack.mappings.assign_popup(ctx));
            learn.changed(changed);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_expand(move |row| {
            learn.rack.mappings.expand(row);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_remove(move |row| {
            let changed = learn.with_ctx(|ctx| learn.rack.mappings.remove_row(ctx, row));
            learn.changed(changed);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_remove_all(move || {
            let (config_changed, slots) = learn.with_ctx(|ctx| learn.rack.mappings.remove_all(ctx));
            learn.changed((config_changed, None));
            for slot in slots {
                learn.changed((false, Some(slot)));
            }
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_set_mode(move |row, mode| {
            let changed = learn.with_ctx(|ctx| learn.rack.mappings.set_mode(ctx, row, mode));
            learn.changed(changed);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_set_range(move |row, from, to| {
            learn.edit(row, |mode, _| {
                match mode {
                    MappingMode::Direct { from: f, to: t } | MappingMode::Mirrored { from: f, to: t } => {
                        (*f, *t) = (from.clamp(0.0, 1.0), to.clamp(0.0, 1.0));
                    }
                    MappingMode::Modulate { low, high } => (*low, *high) = (from.clamp(-1.0, 1.0), to.clamp(-1.0, 1.0)),
                    _ => {}
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_set_level(move |row, level, position| {
            learn.edit(row, |mode, _| {
                if let MappingMode::Toggle { levels } = mode
                    && let Some(level) = levels.get_mut(level as usize)
                {
                    *level = position.clamp(0.0, 1.0);
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_add_level(move |row| {
            learn.edit(row, |mode, _| {
                if let MappingMode::Toggle { levels } = mode {
                    let last = levels.last().copied().unwrap_or(0.0);
                    levels.push(if last < 1.0 { 1.0 } else { 0.5 });
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_remove_level(move |row, level| {
            learn.edit(row, |mode, _| {
                if let MappingMode::Toggle { levels } = mode
                    && levels.len() > 2
                    && (level as usize) < levels.len()
                {
                    levels.remove(level as usize);
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_set_value(move |row, position| {
            learn.edit(row, |mode, _| {
                match mode {
                    MappingMode::Momentary { value, .. } => *value = position.clamp(0.0, 1.0),
                    MappingMode::MomentaryModulate { offset, .. } => *offset = position.clamp(-1.0, 1.0),
                    _ => {}
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_set_velocity(move |row, on| {
            learn.edit(row, |mode, _| {
                if let MappingMode::Momentary { velocity, .. } | MappingMode::MomentaryModulate { velocity, .. } = mode {
                    *velocity = on;
                }
            });
        });
        let learn = self.clone();
        logic.on_learn_set_kept(move |row, kept, panel| {
            let changed = learn.with_ctx(|ctx| learn.rack.mappings.set_kept(ctx, row, kept == 1, panel));
            learn.changed(changed);
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_set_takeover(move |takeover| {
            learn.rack.mappings.set_takeover(takeover);
            learn.changed((true, None));
            learn.refresh_view();
        });
        let learn = self.clone();
        logic.on_learn_set_pass_through(move |on| {
            learn.rack.mappings.edit_control(|settings| settings.pass_through = on);
            learn.changed((true, None));
            learn.refresh_view();
        });
    }
}

impl Learn {
    /// Shows the surface editor as it is.
    fn refresh_editor(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let state = app.global::<AppState>();
        let editor = self.editor.borrow();
        let Some(editor) = editor.as_ref() else {
            state.set_editing(false);
            return;
        };
        state.set_editing(true);
        state.set_edit_device(editor.device.as_str().into());
        state.set_edit_name(editor.spec.name.as_str().into());
        state.set_edit_status(editor.status().into());
        state.set_edit_listening(match editor.listening {
            Listening::None => 0,
            Listening::Bank { .. } => 1,
            Listening::Control { .. } => 2,
            Listening::Device => 3,
        });
        state.set_edit_can_delete(self.surfaces.library_surface(&editor.device).is_some_and(|s| s.user && s.id == editor.spec.id));
        if let Ok(surface) = editor.surface() {
            // The control being learned is outlined.
            let learning = match editor.listening {
                Listening::Control { control } => Some(control),
                Listening::Bank { bank, next } => {
                    Some(editor.spec.banks[..bank].iter().map(|bank| bank.controls.len()).sum::<usize>() + next)
                }
                _ => None,
            };
            // Pages are drawn one at a time: the selected bank's, or the first.
            let page = editor
                .selected
                .and_then(|bank| surface.banks.get(bank))
                .and_then(|bank| bank.page)
                .or_else(|| surface.first_page());
            let mut controls = control_views(&surface, page, false);
            if let Some(control) = learning.and_then(|control| controls.get_mut(control)) {
                control.selected = true;
            }
            crate::sync_model(&self.edit_controls, controls);
            state.set_edit_surface(crate::SurfaceView {
                name: surface.name.as_str().into(),
                device: editor.device.as_str().into(),
                known: true,
                connected: true,
                width: surface.width as f32,
                height: surface.height as f32,
                controls: ModelRc::from(self.edit_controls.clone()),
                pages: ModelRc::default(),
                page: page.map_or(-1, |page| page as i32),
                variant: Default::default(),
                driver: Default::default(),
                driver_on: false,
            });
            let banks: Vec<EditBank> = surface
                .banks
                .iter()
                .enumerate()
                .map(|(index, bank)| EditBank {
                    x: bank.x as f32,
                    y: bank.y as f32,
                    width: bank.width as f32,
                    height: bank.height as f32,
                    name: bank.name.as_str().into(),
                    selected: editor.selected == Some(index),
                    hidden: bank.page.is_some() && bank.page != page,
                })
                .collect();
            crate::sync_model(&self.edit_banks, banks);
            state.set_edit_banks(ModelRc::from(self.edit_banks.clone()));
        }
        state.set_edit_selected(editor.selected.map_or(-1, |bank| bank as i32));
        if let Some(bank) = editor.selected.and_then(|bank| editor.spec.banks.get(bank)) {
            let messages = editor.bank_messages(editor.selected.unwrap_or(0));
            state.set_edit_bank(EditBankSettings {
                name: bank.name.as_str().into(),
                kind: bank.kind as i32,
                rows: bank.rows as i32,
                columns: bank.columns as i32,
                numbering: i32::from(bank.numbering == live_keys_surfaces::Numbering::BottomFirst),
                message: messages.map_or(0, |m| MessageType::ALL.iter().position(|k| *k == m.kind).unwrap_or(0) as i32),
                channel: i32::from(bank.channel),
                first: messages.map_or(0, |m| i32::from(m.first)),
                step: messages.map_or(1, |m| i32::from(m.step)),
                feedback: bank.feedback,
                lit: i32::from(bank.lit.unwrap_or(127)),
                pressure: pressure_index(bank.pressure),
            });
        }
    }

    /// Changes the surface being edited, then shows it.
    fn edit_surface(&self, change: impl FnOnce(&mut Editor)) {
        if let Some(editor) = self.editor.borrow_mut().as_mut() {
            change(editor);
        }
        self.refresh_editor();
    }

    /// Closes the editor; `save`: the surface becomes the user's, shown for
    /// its device.
    fn finish_editing(&self, save: bool) {
        let Some(editor) = self.editor.borrow_mut().take() else { return };
        if save {
            if let Err(err) = live_keys_surfaces::Library::save(&editor.spec) {
                // Keep editing: it can't be saved like this.
                if let Some(app) = self.app.upgrade() {
                    app.global::<AppState>().set_edit_status(format!("Can't be saved: {err}").into());
                }
                *self.editor.borrow_mut() = Some(editor);
                return;
            }
            let mut config = self.config.borrow_mut();
            if let Some(devices) = &mut config.surfaces.devices
                && !devices.contains(&editor.device)
            {
                devices.push(editor.device.clone());
                log_on_err(config::save(&config));
            }
        }
        self.reload_surfaces();
        self.refresh_editor();
    }

    /// Reads the surfaces again (one was saved or deleted) and shows them.
    fn reload_surfaces(&self) {
        let config = self.config.borrow();
        self.surfaces.reload(live_keys_surfaces::Library::load(), &config);
        sync_surfaces(&mut self.engine.borrow_mut(), &config, &self.surfaces, self.app.upgrade().as_ref());
    }

    /// Hands the surface editor's callbacks to the window.
    pub fn connect_editor(self: &Rc<Self>, logic: &AppLogic) {
        let learn = self.clone();
        logic.on_edit_surface(move |device, listen| {
            let surface = learn.surfaces.library_surface(&device);
            let mut editor = Editor::new(&device, surface.as_ref());
            if listen {
                editor.listen(Listening::Device);
            }
            *learn.editor.borrow_mut() = Some(editor);
            learn.refresh_editor();
        });
        let learn = self.clone();
        logic.on_edit_finish(move |save| learn.finish_editing(save));
        let learn = self.clone();
        logic.on_edit_add(move |kind, single| {
            let kind = match kind {
                0 => live_keys_surfaces::ControlKind::Fader,
                1 => live_keys_surfaces::ControlKind::Knob,
                2 => live_keys_surfaces::ControlKind::Pad,
                _ => live_keys_surfaces::ControlKind::Button,
            };
            learn.edit_surface(|editor| editor.add_bank(kind, single));
        });
        let learn = self.clone();
        logic.on_edit_select_bank(move |bank| learn.edit_surface(|editor| editor.selected = usize::try_from(bank).ok()));
        let learn = self.clone();
        logic.on_edit_move_bank(move |bank, x, y| {
            learn.edit_surface(|editor| editor.move_bank(bank as usize, x.round() as i32, y.round() as i32));
        });
        let learn = self.clone();
        logic.on_edit_delete_bank(move || {
            learn.edit_surface(|editor| {
                if let Some(bank) = editor.selected {
                    editor.delete_bank(bank);
                }
            });
        });
        let learn = self.clone();
        logic.on_edit_set_name(move |name| learn.edit_surface(|editor| editor.spec.name = name.to_string()));
        let learn = self.clone();
        logic.on_edit_bank_name(move |name| {
            learn.edit_surface(|editor| {
                if let Some(bank) = editor.selected {
                    editor.edit_bank(bank, |spec| spec.name = name.to_string());
                }
            });
        });
        let learn = self.clone();
        logic.on_edit_bank_set(move |field, value| {
            learn.edit_surface(|editor| {
                let Some(bank) = editor.selected else { return };
                let Some(spec) = editor.spec.banks.get(bank) else { return };
                let (rows, columns) = (spec.rows, spec.columns);
                let messages = editor.bank_messages(bank).unwrap_or(BankMessages { kind: MessageType::Cc, first: 0, step: 1 });
                let byte = value.clamp(0, 127) as u8;
                match field.as_str() {
                    "rows" => editor.set_shape(bank, value.max(1) as u32, columns),
                    "columns" => editor.set_shape(bank, rows, value.max(1) as u32),
                    "message" => {
                        let kind = MessageType::ALL.get(value as usize).copied().unwrap_or(MessageType::Cc);
                        editor.set_messages(bank, BankMessages { kind, ..messages });
                    }
                    "first" => editor.set_messages(bank, BankMessages { first: byte, ..messages }),
                    "step" => editor.set_messages(bank, BankMessages { step: byte.max(1), ..messages }),
                    "kind" => editor.edit_bank(bank, |spec| {
                        spec.kind = match value {
                            0 => live_keys_surfaces::ControlKind::Fader,
                            1 => live_keys_surfaces::ControlKind::Knob,
                            2 => live_keys_surfaces::ControlKind::Pad,
                            _ => live_keys_surfaces::ControlKind::Button,
                        };
                    }),
                    "numbering" => editor.edit_bank(bank, |spec| {
                        spec.numbering = if value == 1 {
                            live_keys_surfaces::Numbering::BottomFirst
                        } else {
                            live_keys_surfaces::Numbering::TopFirst
                        };
                    }),
                    "channel" => editor.edit_bank(bank, |spec| {
                        spec.channel = value.clamp(1, 16) as u8;
                        for control in &mut spec.controls {
                            control.channel = None;
                        }
                    }),
                    "feedback" => editor.edit_bank(bank, |spec| spec.feedback = value != 0),
                    "lit" => editor.edit_bank(bank, |spec| spec.lit = Some(byte.max(1))),
                    "pressure" => editor.edit_bank(bank, |spec| spec.pressure = pressure_of(value)),
                    _ => {}
                }
            });
        });
        let learn = self.clone();
        logic.on_edit_learn(move |what| {
            learn.edit_surface(|editor| match what {
                1 => {
                    if let Some(bank) = editor.selected {
                        editor.listen(Listening::Bank { bank, next: 0 });
                    }
                }
                3 => editor.listen(Listening::Device),
                _ => editor.listen(Listening::None),
            });
        });
        let learn = self.clone();
        logic.on_edit_learn_control(move |control| {
            learn.edit_surface(|editor| editor.listen(Listening::Control { control: control as usize }));
        });
        let learn = self.clone();
        logic.on_edit_skip(move || learn.edit_surface(Editor::skip));
        let learn = self.clone();
        logic.on_edit_stop(move || learn.edit_surface(|editor| editor.listen(Listening::None)));
        let learn = self.clone();
        logic.on_edit_delete_surface(move || {
            let Some(id) = learn.editor.borrow().as_ref().map(|editor| editor.spec.id.clone()) else { return };
            if let Err(err) = live_keys_surfaces::Library::delete(&id) {
                log::error!("deleting surface {id} failed: {err}");
            }
            *learn.editor.borrow_mut() = None;
            learn.reload_surfaces();
            learn.refresh_editor();
        });
    }
}
