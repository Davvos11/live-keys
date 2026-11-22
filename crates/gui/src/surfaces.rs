//! The control surfaces panel: which MIDI devices it shows, and their controls'
//! last values, from the events the engine hands over (see
//! `Engine::take_surface_events`). See `docs/design/midi-learn.md`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use live_keys_engine::{SurfaceEvent, midi_device_name};
use live_keys_surfaces::{ControlKind, Library, Message, Reading};
use slint::{Model, ModelRc, VecModel};

use crate::config::AppConfig;
use crate::{MidiDeviceRow, SurfaceControlView, SurfaceView, sync_model};

/// How long a fader or knob shows as moving after its last message, and the
/// panel as active.
const MOVING: Duration = Duration::from_millis(250);

/// One device shown.
struct Shown {
    device: String,
    /// Its surface, as an index into the library.
    surface: Option<usize>,
    controls: Rc<VecModel<SurfaceControlView>>,
    /// Per control, when a fader or knob last moved, while it shows as moving.
    moved: Vec<Option<Instant>>,
}

pub struct SurfacesUi {
    library: Library,
    /// `AppState.surfaces`.
    pub views: Rc<VecModel<SurfaceView>>,
    /// `AppState.surface-devices`.
    pub devices: Rc<VecModel<MidiDeviceRow>>,
    shown: RefCell<Vec<Shown>>,
    /// Device names of the MIDI ports present at the last [`SurfacesUi::set_ports`].
    present: RefCell<Vec<String>>,
    /// The devices last handed to the engine.
    applied: RefCell<Option<Vec<String>>>,
    last_event: Cell<Option<Instant>>,
}

impl SurfacesUi {
    pub fn new(library: Library) -> Self {
        Self {
            library,
            views: Rc::new(VecModel::default()),
            devices: Rc::new(VecModel::default()),
            shown: RefCell::default(),
            present: RefCell::default(),
            applied: RefCell::default(),
            last_event: Cell::new(None),
        }
    }

    /// The devices shown: the user's choice, or, until they choose, every
    /// device plugged in that has a known surface.
    pub fn shown_devices(&self, config: &AppConfig) -> Vec<String> {
        match &config.surfaces.devices {
            Some(devices) => devices.clone(),
            None => self
                .present
                .borrow()
                .iter()
                .filter(|device| self.library.for_device(device).is_some())
                .cloned()
                .collect(),
        }
    }

    /// Records which MIDI ports are present (as the engine lists them);
    /// refreshes if that changed.
    pub fn set_ports(&self, ports: &[String], config: &AppConfig) {
        let mut present: Vec<String> = Vec::new();
        for port in ports {
            let name = midi_device_name(port);
            if !present.iter().any(|known| known == name) {
                present.push(name.to_string());
            }
        }
        if *self.present.borrow() != present {
            *self.present.borrow_mut() = present;
            self.refresh(config);
        }
    }

    /// The devices the engine should watch, if that changed since the last call.
    pub fn take_devices_to_apply(&self, config: &AppConfig) -> Option<Vec<String>> {
        let devices = self.shown_devices(config);
        let mut applied = self.applied.borrow_mut();
        (applied.as_ref() != Some(&devices)).then(|| {
            *applied = Some(devices.clone());
            devices
        })
    }

    /// The shown devices, for the button that opens the device list.
    pub fn summary(&self, config: &AppConfig) -> String {
        let present = self.present.borrow();
        self.shown_devices(config)
            .iter()
            .map(|device| {
                let name = self.library.for_device(device).map_or(device.as_str(), |surface| surface.name.as_str());
                if present.contains(device) { name.to_string() } else { format!("{name} (not connected)") }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Shows `config`'s devices against the ports last present. Devices that
    /// stay shown keep their controls' values.
    pub fn refresh(&self, config: &AppConfig) {
        let shown_devices = self.shown_devices(config);
        let present = self.present.borrow();
        let device_rows: Vec<MidiDeviceRow> = present
            .iter()
            .chain(shown_devices.iter().filter(|device| !present.contains(device)))
            .map(|name| MidiDeviceRow {
                name: name.into(),
                checked: shown_devices.contains(name),
                available: present.contains(name),
            })
            .collect();
        sync_model(&self.devices, device_rows);

        let mut shown = self.shown.borrow_mut();
        let mut old: Vec<Shown> = shown.drain(..).collect();
        for device in &shown_devices {
            let kept = old.iter().position(|item| &item.device == device).map(|index| old.remove(index));
            shown.push(kept.unwrap_or_else(|| self.show(device)));
        }
        let views: Vec<SurfaceView> = shown
            .iter()
            .map(|item| {
                let surface = item.surface.map(|index| &self.library.surfaces[index]);
                SurfaceView {
                    name: surface.map_or(item.device.as_str(), |surface| surface.name.as_str()).into(),
                    device: item.device.as_str().into(),
                    known: surface.is_some(),
                    connected: present.contains(&item.device),
                    width: surface.map_or(0.0, |surface| surface.width as f32),
                    height: surface.map_or(0.0, |surface| surface.height as f32),
                    controls: ModelRc::from(item.controls.clone()),
                }
            })
            .collect();
        sync_model(&self.views, views);
    }

    fn show(&self, device: &str) -> Shown {
        let surface = self.library.surfaces.iter().position(|surface| surface.matches(device));
        let controls: Vec<SurfaceControlView> = surface
            .map(|index| &self.library.surfaces[index])
            .map(|surface| {
                surface
                    .controls
                    .iter()
                    .map(|control| {
                        let (width, height) = control.kind.cell_size();
                        SurfaceControlView {
                            kind: match control.kind {
                                ControlKind::Fader => 0,
                                ControlKind::Knob => 1,
                                ControlKind::Pad => 2,
                                ControlKind::Button => 3,
                            },
                            label: control.label.as_str().into(),
                            x: control.x as f32,
                            y: control.y as f32,
                            width: width as f32,
                            height: height as f32,
                            // Pitch bend rests in the middle.
                            value: if control.message == Message::PitchBend { 0.5 } else { 0.0 },
                            pressed: false,
                            moving: false,
                            mapped: false,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Shown {
            device: device.to_string(),
            surface,
            moved: vec![None; controls.len()],
            controls: Rc::new(VecModel::from(controls)),
        }
    }

    /// Moves the controls `events` came from.
    pub fn apply(&self, events: &[SurfaceEvent]) {
        if events.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut shown = self.shown.borrow_mut();
        for event in events {
            let Some(item) = shown.iter_mut().find(|item| *item.device == *event.device) else { continue };
            let Some(surface) = item.surface.map(|index| &self.library.surfaces[index]) else { continue };
            self.last_event.set(Some(now));
            if let Some((control, reading)) = surface.read(&event.event) {
                let Some(mut row) = item.controls.row_data(control) else { continue };
                match reading {
                    Reading::Value(value) => {
                        row.value = value;
                        if matches!(surface.controls[control].kind, ControlKind::Fader | ControlKind::Knob) {
                            row.moving = true;
                            item.moved[control] = Some(now);
                        }
                    }
                    Reading::Press(velocity) => {
                        row.value = velocity;
                        row.pressed = true;
                    }
                    Reading::Release => row.pressed = false,
                }
                item.controls.set_row_data(control, row);
            } else if let Some((pads, value)) = surface.pad_pressure(&event.event) {
                // Pressure on a channel shared by pads: it's the held ones'.
                for pad in pads {
                    if let Some(mut row) = item.controls.row_data(pad)
                        && row.pressed
                    {
                        row.value = value;
                        item.controls.set_row_data(pad, row);
                    }
                }
            }
        }
    }

    /// Lets faders and knobs that stopped moving stop showing it; returns
    /// whether any shown device sent something a moment ago.
    pub fn tick(&self) -> bool {
        let now = Instant::now();
        for item in self.shown.borrow_mut().iter_mut() {
            for (control, moved) in item.moved.iter_mut().enumerate() {
                if moved.is_some_and(|at| now - at >= MOVING) {
                    *moved = None;
                    if let Some(mut row) = item.controls.row_data(control) {
                        row.moving = false;
                        item.controls.set_row_data(control, row);
                    }
                }
            }
        }
        self.last_event.get().is_some_and(|at| now - at < MOVING)
    }
}
