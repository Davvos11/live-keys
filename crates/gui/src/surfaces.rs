//! The control surfaces panel: which MIDI devices it shows, and their controls'
//! last values, from the events the mapping thread hands over (see
//! `Engine::take_mapper_reports`); and for MIDI learn, which control is
//! selected, which drive something, and which have their target elsewhere.
//! See `docs/design/midi-learn.md`.
//!
//! A device is a MIDI port (by `midi_device_name`), or several ports that one
//! surface spans, together (`Library::device_of`): the engine is handed the
//! ports, everything else here goes by device.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use live_keys_engine::{SurfaceEvent, SurfaceSetup, midi_device_name};
use live_keys_surfaces::{ControlKind, Library, Message, Options, Reader, Reading, Surface};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::config::AppConfig;
use crate::{MidiDeviceRow, SurfaceControlView, SurfaceView, sync_model};

/// How long a fader or knob shows as moving after its last message, and the
/// panel as active.
const MOVING: Duration = Duration::from_millis(250);

/// A MIDI port present, and the device it belongs to.
#[derive(Debug, Clone, PartialEq)]
struct Port {
    /// Its `midi_device_name`.
    name: String,
    device: String,
    number: u8,
}

/// One device shown.
struct Shown {
    device: String,
    /// Its surface, as the library has it, and as it's read (the user's
    /// options applied).
    surface: Option<(usize, Arc<Surface>)>,
    reader: Reader,
    controls: Rc<VecModel<SurfaceControlView>>,
    /// Per control, when a fader or knob last moved, while it shows as moving.
    moved: Vec<Option<Instant>>,
}

pub struct SurfacesUi {
    library: RefCell<Library>,
    /// `AppState.surfaces`.
    pub views: Rc<VecModel<SurfaceView>>,
    /// `AppState.surface-devices`.
    pub devices: Rc<VecModel<MidiDeviceRow>>,
    shown: RefCell<Vec<Shown>>,
    /// The MIDI ports present at the last [`SurfacesUi::set_ports`].
    ports: RefCell<Vec<Port>>,
    /// The ports last handed to the engine.
    applied: RefCell<Option<Vec<String>>>,
    last_event: Cell<Option<Instant>>,
    /// The shown devices changed since [`SurfacesUi::take_changed`].
    changed: Cell<bool>,
}

impl SurfacesUi {
    pub fn new(library: Library) -> Self {
        Self {
            library: RefCell::new(library),
            views: Rc::new(VecModel::default()),
            devices: Rc::new(VecModel::default()),
            shown: RefCell::default(),
            ports: RefCell::default(),
            applied: RefCell::default(),
            last_event: Cell::new(None),
            changed: Cell::new(true),
        }
    }

    /// Whether the shown devices (or how they're read) changed since the last
    /// call (MIDI learn's setup covers them).
    pub fn take_changed(&self) -> bool {
        self.changed.replace(false)
    }

    /// The devices present, in port order.
    fn present(&self) -> Vec<String> {
        let mut present: Vec<String> = Vec::new();
        for port in self.ports.borrow().iter() {
            if !present.contains(&port.device) {
                present.push(port.device.clone());
            }
        }
        present
    }

    /// The devices shown: the user's choice, or, until they choose, every
    /// device plugged in that has a known surface.
    pub fn shown_devices(&self, config: &AppConfig) -> Vec<String> {
        match &config.surfaces.devices {
            Some(devices) => devices.clone(),
            None => self
                .present()
                .into_iter()
                .filter(|device| self.library.borrow().for_device(device).is_some())
                .collect(),
        }
    }

    /// Records which MIDI ports are present (as the engine lists them);
    /// refreshes if that changed.
    pub fn set_ports(&self, ports: &[String], config: &AppConfig) {
        let mut present: Vec<Port> = Vec::new();
        for port in ports {
            let name = midi_device_name(port);
            if !present.iter().any(|known| known.name == name) {
                let (device, number) = self.library.borrow().device_of(name);
                present.push(Port { name: name.to_string(), device, number });
            }
        }
        if *self.ports.borrow() != present {
            *self.ports.borrow_mut() = present;
            self.refresh(config);
        }
    }

    /// The ports the engine should watch, if that changed since the last call:
    /// those of the shown devices (a device not present by its own name, so
    /// it's connected when it appears).
    pub fn take_devices_to_apply(&self, config: &AppConfig) -> Option<Vec<String>> {
        let ports = self.ports.borrow();
        let mut names: Vec<String> = Vec::new();
        for device in self.shown_devices(config) {
            let of_device: Vec<String> =
                ports.iter().filter(|port| port.device == device).map(|port| port.name.clone()).collect();
            if of_device.is_empty() {
                names.push(device);
            } else {
                names.extend(of_device);
            }
        }
        let mut applied = self.applied.borrow_mut();
        (applied.as_ref() != Some(&names)).then(|| {
            *applied = Some(names.clone());
            names
        })
    }

    /// The shown devices, for the button that opens the device list.
    pub fn summary(&self, config: &AppConfig) -> String {
        let present = self.present();
        self.shown_devices(config)
            .iter()
            .map(|device| {
                let library = self.library.borrow();
                let name = library.for_device(device).map_or(device.as_str(), |surface| surface.name.as_str());
                if present.contains(device) { name.to_string() } else { format!("{name} (not connected)") }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Shows `config`'s devices against the ports last present. Devices that
    /// stay shown keep their controls' values.
    pub fn refresh(&self, config: &AppConfig) {
        let shown_devices = self.shown_devices(config);
        let present = self.present();
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

        self.changed.set(true);
        {
            let mut shown = self.shown.borrow_mut();
            let mut old: Vec<Shown> = shown.drain(..).collect();
            for device in &shown_devices {
                let options = self.options(device, config);
                let previous = old.iter().position(|item| &item.device == device).map(|index| old.remove(index));
                let item = match previous {
                    Some(item) if item.surface.as_ref().is_none_or(|(_, surface)| surface.options == options) => item,
                    // Read differently now (another variant): drawn anew, on the same page.
                    Some(item) => {
                        let mut new = self.show(device, options);
                        new.reader.set_page(item.reader.page());
                        show_page(&new);
                        new
                    }
                    None => self.show(device, options),
                };
                shown.push(item);
            }
        }
        self.refresh_views();
    }

    fn refresh_views(&self) {
        let present = self.present();
        let shown = self.shown.borrow();
        let views: Vec<SurfaceView> = shown
            .iter()
            .map(|item| {
                let surface = item.surface.as_ref().map(|(_, surface)| surface);
                let page = item.reader.page();
                let variant = surface
                    .zip(page)
                    .and_then(|(surface, page)| {
                        let page = &surface.pages[page];
                        let chosen = surface.options.variants.get(&page.id).copied().unwrap_or(0);
                        page.variants.get(chosen)
                    })
                    .map_or(SharedString::default(), |name| name.into());
                SurfaceView {
                    name: surface.map_or(item.device.as_str(), |surface| surface.name.as_str()).into(),
                    device: item.device.as_str().into(),
                    known: surface.is_some(),
                    connected: present.contains(&item.device),
                    width: surface.map_or(0.0, |surface| surface.width as f32),
                    height: surface.map_or(0.0, |surface| surface.height as f32),
                    controls: ModelRc::from(item.controls.clone()),
                    pages: ModelRc::from(Rc::new(VecModel::from(
                        surface
                            .map(|surface| {
                                surface.pages.iter().map(|page| SharedString::from(page.name.as_str())).collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                    ))),
                    page: page.map_or(-1, |page| page as i32),
                    variant,
                    driver: surface
                        .and_then(|surface| surface.driver.as_deref())
                        .and_then(live_keys_surfaces::driver::switch_label)
                        .unwrap_or_default()
                        .into(),
                    driver_on: surface.is_some_and(|surface| surface.driver.is_some() && !surface.options.one_port),
                }
            })
            .collect();
        sync_model(&self.views, views);
    }

    /// How `device`'s surface is read: the user's variants; one port while
    /// it has a driver that's switched off (or unknown here): the device
    /// spreads its messages over its ports only when its driver asks.
    fn options(&self, device: &str, config: &AppConfig) -> Options {
        let library = self.library.borrow();
        let driver = library.for_device(device).and_then(|surface| surface.driver.as_deref()).is_some_and(|driver| {
            live_keys_surfaces::driver::switch_label(driver).is_none()
                || config.surfaces.without_driver.iter().any(|off| off == device)
        });
        Options {
            variants: config
                .surfaces
                .variants
                .get(device)
                .map(|variants| variants.iter().map(|(page, &variant)| (page.clone(), variant)).collect())
                .unwrap_or_default(),
            one_port: driver,
        }
    }

    /// Replaces the library (a surface was saved or deleted), and shows the
    /// devices again with it.
    pub fn reload(&self, library: Library, config: &AppConfig) {
        *self.library.borrow_mut() = library;
        let ports: Vec<String> = self.ports.borrow().iter().map(|port| port.name.clone()).collect();
        self.ports.borrow_mut().clear();
        self.shown.borrow_mut().clear();
        self.set_ports(&ports, config);
        self.refresh(config);
    }

    /// The surface the library has for `device`, shown or not.
    pub fn library_surface(&self, device: &str) -> Option<Surface> {
        self.library.borrow().for_device(device).cloned()
    }

    fn show(&self, device: &str, options: Options) -> Shown {
        let library = self.library.borrow();
        let surface = library
            .surfaces
            .iter()
            .position(|surface| surface.matches(device))
            .map(|index| (index, Arc::new(library.surfaces[index].with_options(options))));
        let reader = surface.as_ref().map(|(_, surface)| Reader::new(surface)).unwrap_or_default();
        let controls = surface
            .as_ref()
            .map(|(_, surface)| control_views(surface, reader.page(), reader.shift()))
            .unwrap_or_default();
        Shown {
            device: device.to_string(),
            surface,
            reader,
            moved: vec![None; controls.len()],
            controls: Rc::new(VecModel::from(controls)),
        }
    }

    /// The shown devices that have a surface: theirs, as read, and their ports present.
    pub fn shown_surfaces(&self) -> Vec<SurfaceSetup> {
        let ports = self.ports.borrow();
        self.shown
            .borrow()
            .iter()
            .filter_map(|item| {
                let (_, surface) = item.surface.as_ref()?;
                let mut of_device: Vec<(Arc<str>, u8)> = ports
                    .iter()
                    .filter(|port| port.device == item.device)
                    .map(|port| (port.name.as_str().into(), port.number))
                    .collect();
                if of_device.is_empty() {
                    of_device.push((item.device.as_str().into(), 1));
                }
                Some(SurfaceSetup { device: item.device.as_str().into(), surface: surface.clone(), ports: of_device })
            })
            .collect()
    }

    /// The surface of shown device `device`, as read.
    pub fn surface_of(&self, device: &str) -> Option<Arc<Surface>> {
        let shown = self.shown.borrow();
        let item = shown.iter().find(|item| item.device == device)?;
        Some(item.surface.as_ref()?.1.clone())
    }

    /// The device MIDI port `port` belongs to, and its number there.
    fn port(&self, port: &str) -> Option<(String, u8)> {
        let ports = self.ports.borrow();
        let found = ports.iter().find(|known| known.name == port)?;
        Some((found.device.clone(), found.number))
    }

    /// If MIDI port `port` is part of device `device` (shown or not): its
    /// number there.
    pub fn port_of_device(&self, port: &str, device: &str) -> Option<u8> {
        match self.port(port) {
            Some((of, number)) => (of == device).then_some(number),
            None => (port == device).then_some(1),
        }
    }

    /// Shows page `page` of `device`'s surface (the user clicked it).
    pub fn set_page(&self, device: &str, page: usize) {
        let mut shown = self.shown.borrow_mut();
        let Some(item) = shown.iter_mut().find(|item| item.device == device) else { return };
        item.reader.set_page(Some(page));
        show_page(item);
        drop(shown);
        self.refresh_views();
    }

    /// Steps the variant of `device`'s shown page by `step` (wrapping), in
    /// `config`; true if it changed (refresh with the config).
    pub fn step_variant(&self, device: &str, step: i32, config: &mut AppConfig) -> bool {
        let Some(surface) = self.surface_of(device) else { return false };
        let shown = self.shown.borrow();
        let Some(page) = shown.iter().find(|item| item.device == device).and_then(|item| item.reader.page()) else {
            return false;
        };
        let page = &surface.pages[page];
        let count = page.variants.len() as i32;
        if count < 2 {
            return false;
        }
        let variants = config.surfaces.variants.entry(device.to_string()).or_default();
        let now = variants.get(&page.id).copied().unwrap_or(0) as i32;
        let next = (now + step).rem_euclid(count) as usize;
        if next == 0 {
            variants.remove(&page.id);
        } else {
            variants.insert(page.id.clone(), next);
        }
        if variants.is_empty() {
            config.surfaces.variants.remove(device);
        }
        true
    }

    /// Marks MIDI learn's selected control and the controls that drive
    /// something (both as device and control index).
    pub fn set_learn(&self, selected: Option<&(String, usize)>, mapped: &HashSet<(String, usize)>) {
        for item in self.shown.borrow().iter() {
            for control in 0..item.controls.row_count() {
                let Some(mut row) = item.controls.row_data(control) else { continue };
                let key = (item.device.clone(), control);
                let (is_selected, is_mapped) = (selected == Some(&key), mapped.contains(&key));
                if row.selected != is_selected || row.mapped != is_mapped {
                    row.selected = is_selected;
                    row.mapped = is_mapped;
                    item.controls.set_row_data(control, row);
                }
            }
        }
    }

    /// Shows control `control` of `device` with its target at `at` (as a
    /// position of the control) and the hardware as a ghost, or not.
    pub fn set_waiting(&self, device: &str, control: usize, at: Option<f32>) {
        let shown = self.shown.borrow();
        let Some(item) = shown.iter().find(|item| item.device == device) else { return };
        if let Some(mut row) = item.controls.row_data(control) {
            row.waiting = at.is_some();
            row.target = at.unwrap_or_default();
            item.controls.set_row_data(control, row);
        }
    }

    /// Draws control `control` of `device` where it was sent (MIDI feedback):
    /// that's where the hardware is now.
    pub fn show_sent(&self, device: &str, control: usize, shown: live_keys_engine::Shown) {
        let shown_devices = self.shown.borrow();
        let Some(item) = shown_devices.iter().find(|item| item.device == device) else { return };
        let Some(mut row) = item.controls.row_data(control) else { return };
        match shown {
            live_keys_engine::Shown::Position(position) => row.value = position,
            live_keys_engine::Shown::Lit(on) => {
                row.pressed = on;
                row.value = if on { 1.0 } else { 0.0 };
            }
        }
        item.controls.set_row_data(control, row);
    }

    /// Shows no control's target as elsewhere (a new setup starts over).
    pub fn clear_waiting(&self) {
        for item in self.shown.borrow().iter() {
            for control in 0..item.controls.row_count() {
                if let Some(mut row) = item.controls.row_data(control)
                    && row.waiting
                {
                    row.waiting = false;
                    item.controls.set_row_data(control, row);
                }
            }
        }
    }

    /// Moves the controls `events` came from; returns the last one moved or
    /// pressed (device, control), for MIDI learn to select.
    pub fn apply(&self, events: &[SurfaceEvent]) -> Option<(String, usize)> {
        if events.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut moved = None;
        let mut pages_changed = false;
        {
            let mut shown = self.shown.borrow_mut();
            for event in events {
                let Some((device, port)) = self.port(&event.device) else { continue };
                let Some(item) = shown.iter_mut().find(|item| item.device == device) else { continue };
                let Some((_, surface)) = item.surface.clone() else { continue };
                self.last_event.set(Some(now));
                let shift = item.reader.shift();
                let read = item.reader.read(&surface, port, &event.event);
                if read.page_changed || item.reader.shift() != shift {
                    show_page(item);
                    pages_changed |= read.page_changed;
                }
                if let Some((control, reading)) = read.control {
                    if !matches!(reading, Reading::Release) {
                        moved = Some((item.device.clone(), control));
                    }
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
                } else if let Some((pads, value)) = read.pressure {
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
        if pages_changed {
            self.refresh_views();
        }
        moved
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

/// Hides the controls of `item` that aren't on its page (or are shift twins
/// in the wrong state), shows the others, and lights the page's button.
fn show_page(item: &Shown) {
    let Some((_, surface)) = &item.surface else { return };
    for control in 0..item.controls.row_count() {
        let hidden = !surface.visible(control, item.reader.page(), item.reader.shift());
        let active = page_button_lit(surface, control, item.reader.page());
        if let Some(mut row) = item.controls.row_data(control)
            && (row.hidden != hidden || row.active != active)
        {
            row.hidden = hidden;
            row.active = active;
            item.controls.set_row_data(control, row);
        }
    }
}

/// Whether `control` is the button of `page`, the page shown.
fn page_button_lit(surface: &Surface, control: usize, page: Option<usize>) -> bool {
    page.and_then(|page| surface.pages.get(page)).is_some_and(|page| page.button == Some(control))
}

/// `surface`'s controls as the panel draws them, at rest, on page `page`
/// with shift `shift`.
pub fn control_views(surface: &Surface, page: Option<usize>, shift: bool) -> Vec<SurfaceControlView> {
    surface
        .controls
        .iter()
        .enumerate()
        .map(|(index, control)| {
            let (width, height) = (control.width, control.height);
            SurfaceControlView {
                kind: match control.kind {
                    ControlKind::Fader => 0,
                    ControlKind::Knob => 1,
                    ControlKind::Pad => 2,
                    ControlKind::Button => 3,
                },
                label: control.short.as_str().into(),
                x: control.x as f32,
                y: control.y as f32,
                width: width as f32,
                height: height as f32,
                // Pitch bend rests in the middle.
                value: if control.message == Message::PitchBend { 0.5 } else { 0.0 },
                pressed: false,
                moving: false,
                mapped: false,
                selected: false,
                waiting: false,
                target: 0.0,
                hidden: !surface.visible(index, page, shift),
                active: page_button_lit(surface, index, page),
            }
        })
        .collect()
}
