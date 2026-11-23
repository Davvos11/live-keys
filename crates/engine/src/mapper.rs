//! The mapping thread (MIDI learn): control-surface events come in from the
//! MIDI callbacks, go through the [`Mappings`], and the targets' new values go
//! straight out — plugin parameters and volumes to the audio thread on a queue
//! of their own, setBfree's functions to the plugin's second atom channel —
//! then to the GUI thread as reports, for showing and recording. Neither the GUI
//! thread's pace nor its stalls stand between a hardware move and the sound.
//! Controls that show values (motor faders, LED rings, lit buttons) are sent
//! them from here too, through output connections of the thread's own.
//! See "How it runs" in `docs/design/midi-learn.md`.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use live_keys_core::{ParamChange, Producer};
use live_keys_lv2_host::PortEvent;
use live_keys_surfaces::driver::{self, Driver};
use live_keys_surfaces::{Reader, Shown, Surface};

use crate::midi::midi_device_name;

use crate::engine::PluginKey;
use crate::mapping::{ControlSetup, Mappings, TargetId};
use crate::midi::SurfaceEvent;
use crate::rack::MapperCommand;
use crate::setbfree::SetbfreeSink;

/// How often a glide moves its target on.
const GLIDE_TICK: Duration = Duration::from_millis(2);
/// Inputs (surface events, setups) waiting for the thread; a surface event
/// that doesn't fit is dropped.
const INPUT_QUEUE: usize = 1024;
/// The least time between two values sent to one control: a fader being
/// moved from the screen sends at most this often.
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(10);
/// Reports waiting for the GUI thread, which drains them every ~16 ms.
const REPORT_QUEUE: usize = 8192;

/// The mapping thread's queue to the audio thread while a stream runs.
pub(crate) type SharedMapperTx = Arc<Mutex<Option<Producer<MapperCommand>>>>;

/// Turns a target's position (0..1) into the value its [`Output`] takes.
pub type ToValue = Arc<dyn Fn(f32) -> f32 + Send + Sync>;

/// Where a target's values go.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// A plugin parameter, by its `ParamInfo::id`: straight to the audio thread.
    Param { key: PluginKey, param: u32 },
    /// A setBfree function, value 0..=127: straight to the plugin.
    Setbfree { key: PluginKey, function: Arc<str> },
    /// A slot's volume ([`MASTER_SLOT`](crate::MASTER_SLOT): the master's) as
    /// a linear gain: straight to the audio thread.
    Volume { slot: usize },
    /// Something the GUI thread applies (a mute, a transposition, the
    /// metronome…): only reported.
    Gui,
}

/// One target of a setup.
pub struct TargetSpec {
    pub output: Output,
    pub to_value: ToValue,
    /// Where it is now.
    pub position: f32,
}

/// A shown device with a surface: its name (a port's `midi_device_name`, or
/// the device several ports make, see `Surface::port_of`), its surface, and
/// its ports present (`midi_device_name` and number, 1-based).
#[derive(Clone)]
pub struct SurfaceSetup {
    pub device: Arc<str>,
    pub surface: Arc<Surface>,
    pub ports: Vec<(Arc<str>, u8)>,
}

impl SurfaceSetup {
    /// The port that feedback goes out on (the device's input with that
    /// name), if present.
    pub fn output(&self) -> Option<&Arc<str>> {
        self.ports.iter().find(|(_, port)| *port == self.surface.output_port).map(|(name, _)| name)
    }
}

/// Every mapping, as [`Engine::set_mappings`](crate::Engine::set_mappings)
/// takes them.
pub struct MapperSetup {
    /// The surfaces the controls are on.
    pub surfaces: Vec<SurfaceSetup>,
    pub controls: Vec<ControlSetup>,
    pub targets: Vec<TargetSpec>,
}

/// What the mapping thread did, for the GUI thread.
#[derive(Clone)]
pub enum MapperReport {
    /// A control-surface event, mapped or not (for drawing the panel, and
    /// learning).
    Event(SurfaceEvent),
    /// A mapping moved a target of setup `generation`: to effective position
    /// `position`, which is `value` for its output, with its base (what the
    /// user set, under any modulation) at `base`. `for_gui`: for setBfree,
    /// the messages its open GUI needs to show it (filled by the engine).
    Applied {
        generation: u64,
        target: TargetId,
        output: Output,
        position: f32,
        base: f32,
        value: f32,
        for_gui: Vec<PortEvent>,
    },
    /// A control that shows values was sent `shown` (the panel draws the
    /// hardware there).
    Feedback { device: Arc<str>, control: usize, shown: Shown },
    /// A fader or knob's target is elsewhere than the hardware (`at`, as a
    /// position of the control: draw the hardware as a ghost) or no longer is.
    Waiting { generation: u64, device: Arc<str>, control: usize, at: Option<f32> },
}

/// A target as the thread sends it.
struct Target {
    output: Output,
    setbfree: Option<SetbfreeSink>,
    to_value: ToValue,
    /// The value last sent, so a stepped target isn't sent the same one again.
    last: Option<f32>,
}

/// A setup as the thread takes it: outputs resolved.
pub(crate) struct Setup {
    pub generation: u64,
    pub surfaces: Vec<SurfaceSetup>,
    pub controls: Vec<ControlSetup>,
    /// With the setBfree sink of each `Output::Setbfree` target.
    pub targets: Vec<(TargetSpec, Option<SetbfreeSink>)>,
}

pub(crate) enum MapperInput {
    Event(SurfaceEvent),
    Setup(Box<Setup>),
    /// A target of setup `generation` was moved elsewhere.
    Position { generation: u64, target: TargetId, position: f32 },
    /// Devices may have been (re)connected: open outputs again, set drivers
    /// up again and send every control that shows values its value.
    Resync,
    /// The app is closing: give every device with a driver back, then say so.
    Close(SyncSender<()>),
}

/// A driver running for a shown device.
struct Active {
    driver: Box<dyn Driver>,
    surface: Arc<Surface>,
    output: Arc<str>,
}

/// A control, as (output port, control index).
type ControlKey = (Arc<str>, usize);

/// Sending values back to controls: an output connection per port, and
/// per control when it was last sent one and what's waiting to be.
#[derive(Default)]
struct FeedbackOut {
    /// By port (`midi_device_name`); `None`: it has no output (or it failed), until a resync.
    outputs: HashMap<Arc<str>, Option<midir::MidiOutputConnection>>,
    sent: HashMap<ControlKey, Instant>,
    waiting: HashMap<ControlKey, Vec<Vec<u8>>>,
    /// The messages last sent, so the same aren't sent again (until a resync).
    last: HashMap<ControlKey, Vec<Vec<u8>>>,
}

impl FeedbackOut {
    /// Sends `messages` (a control's value) to `output`'s control `control`,
    /// or keeps them until [`FEEDBACK_INTERVAL`] after the last.
    /// `again`: even if they're what was sent last.
    fn send(&mut self, output: &Arc<str>, control: usize, messages: Vec<Vec<u8>>, again: bool, now: Instant) {
        let key = (output.clone(), control);
        if !again && self.last.get(&key) == Some(&messages) {
            self.waiting.remove(&key);
            return;
        }
        if self.sent.get(&key).is_some_and(|sent| now.duration_since(*sent) < FEEDBACK_INTERVAL) {
            self.waiting.insert(key, messages);
            return;
        }
        self.waiting.remove(&key);
        self.sent.insert(key.clone(), now);
        self.raw(output, &messages);
        self.last.insert(key, messages);
    }

    /// Sends `messages` to `output` now (a driver's).
    fn raw(&mut self, output: &Arc<str>, messages: &[Vec<u8>]) {
        let connection = self.outputs.entry(output.clone()).or_insert_with(|| open_output(output));
        let Some(connection) = connection else { return };
        for message in messages {
            if let Err(err) = connection.send(message) {
                log::warn!("MIDI to {output} failed: {err}");
            }
        }
    }

    /// Sends what waited long enough.
    fn send_due(&mut self, now: Instant) {
        let due: Vec<(ControlKey, Vec<Vec<u8>>)> = self
            .waiting
            .iter()
            .filter(|(key, _)| self.sent.get(*key).is_none_or(|sent| now.duration_since(*sent) >= FEEDBACK_INTERVAL))
            .map(|(key, messages)| (key.clone(), messages.clone()))
            .collect();
        for ((output, control), messages) in due {
            self.send(&output, control, messages, true, now);
        }
    }
}

/// An output connection to the MIDI output port of `device` (by
/// [`midi_device_name`]), if it has one.
fn open_output(device: &str) -> Option<midir::MidiOutputConnection> {
    let output = midir::MidiOutput::new("live-keys").ok()?;
    let port = output
        .ports()
        .into_iter()
        .find(|port| output.port_name(port).is_ok_and(|name| midi_device_name(&name) == device))?;
    match output.connect(&port, "live-keys-feedback") {
        Ok(connection) => {
            log::info!("MIDI feedback output connected: {device}");
            Some(connection)
        }
        Err(err) => {
            log::warn!("connecting MIDI feedback output '{device}' failed: {err}");
            None
        }
    }
}

/// Starts the mapping thread. It runs until every sender of its input queue is
/// gone.
pub(crate) fn spawn(rack: SharedMapperTx) -> (SyncSender<MapperInput>, Receiver<MapperReport>) {
    let (input_tx, input_rx) = mpsc::sync_channel(INPUT_QUEUE);
    let (report_tx, report_rx) = mpsc::sync_channel(REPORT_QUEUE);
    std::thread::Builder::new()
        .name("midi-mapping".into())
        .spawn(move || run(input_rx, rack, report_tx))
        .expect("failed to spawn the MIDI mapping thread");
    (input_tx, report_rx)
}

struct State {
    generation: u64,
    /// Per surface, its device.
    devices: Vec<Arc<str>>,
    /// Per surface, the port its feedback goes out on.
    outputs: Vec<Option<Arc<str>>>,
    /// Port (`midi_device_name`) → surface and port number.
    ports: HashMap<Arc<str>, (usize, u8)>,
    surfaces: Vec<Arc<Surface>>,
    /// By device: the drivers running.
    drivers: HashMap<Arc<str>, Active>,
    feedback: FeedbackOut,
    mappings: Mappings,
    targets: Vec<Target>,
    rack: SharedMapperTx,
    reports: SyncSender<MapperReport>,
}

fn run(inputs: Receiver<MapperInput>, rack: SharedMapperTx, reports: SyncSender<MapperReport>) {
    let mut state = State {
        generation: 0,
        devices: Vec::new(),
        outputs: Vec::new(),
        ports: HashMap::new(),
        surfaces: Vec::new(),
        drivers: HashMap::new(),
        feedback: FeedbackOut::default(),
        mappings: Mappings::new(Vec::new(), Vec::new(), Vec::new()),
        targets: Vec::new(),
        rack,
        reports,
    };
    loop {
        let input = if state.mappings.gliding() || !state.feedback.waiting.is_empty() {
            match inputs.recv_timeout(GLIDE_TICK) {
                Ok(input) => Some(input),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return state.close_drivers(),
            }
        } else {
            match inputs.recv() {
                Ok(input) => Some(input),
                Err(_) => return state.close_drivers(),
            }
        };
        if let Some(input) = input {
            state.handle(input);
            while let Ok(input) = inputs.try_recv() {
                state.handle(input);
            }
        }
        state.mappings.tick(Instant::now());
        state.flush();
        state.feedback.send_due(Instant::now());
    }
}

impl State {
    fn handle(&mut self, input: MapperInput) {
        match input {
            MapperInput::Event(event) => {
                if let Some(&(surface, port)) = self.ports.get(&event.device) {
                    self.mappings.event(surface, port, &event.event, Instant::now());
                }
                // Full: the panel misses a move, nothing else.
                let _ = self.reports.try_send(MapperReport::Event(event));
            }
            MapperInput::Setup(setup) => {
                self.flush();
                let positions = setup.targets.iter().map(|(spec, _)| spec.position).collect();
                // The same device with the same surface keeps its page and shift.
                let key = |device: &Arc<str>, surface: &Surface| format!("{}\n{device}", surface.id);
                let readers: HashMap<String, Reader> = (0..self.surfaces.len())
                    .filter_map(|index| {
                        let reader = self.mappings.reader(index)?.clone();
                        Some((key(&self.devices[index], &self.surfaces[index]), reader))
                    })
                    .collect();
                self.devices = setup.surfaces.iter().map(|surface| surface.device.clone()).collect();
                self.outputs = setup.surfaces.iter().map(|surface| surface.output().cloned()).collect();
                self.ports = setup
                    .surfaces
                    .iter()
                    .enumerate()
                    .flat_map(|(index, surface)| surface.ports.iter().map(move |(name, port)| (name.clone(), (index, *port))))
                    .collect();
                self.surfaces = setup.surfaces.into_iter().map(|surface| surface.surface).collect();
                self.mappings = Mappings::new(self.surfaces.clone(), setup.controls, positions);
                for index in 0..self.surfaces.len() {
                    if let Some(reader) = readers.get(&key(&self.devices[index], &self.surfaces[index])) {
                        self.mappings.set_reader(index, reader.clone());
                    }
                }
                self.sync_drivers(false);
                self.feedback.last.clear();
                self.mappings.resync_feedback();
                self.targets = setup
                    .targets
                    .into_iter()
                    .map(|(spec, setbfree)| Target { output: spec.output, setbfree, to_value: spec.to_value, last: None })
                    .collect();
                self.generation = setup.generation;
            }
            MapperInput::Position { generation, target, position } => {
                if generation == self.generation {
                    // The target has that value now, whatever was sent last.
                    if let Some(target) = self.targets.get_mut(target) {
                        target.last = None;
                    }
                    self.mappings.set_position(target, position);
                }
            }
            MapperInput::Resync => {
                self.feedback.outputs.clear();
                self.feedback.last.clear();
                self.sync_drivers(true);
                self.mappings.resync_feedback();
            }
            MapperInput::Close(done) => {
                self.close_drivers();
                let _ = done.send(());
            }
        }
    }

    /// Starts the drivers of shown devices that need one (and whose output is
    /// there), and gives back the devices no longer driven; tells each which
    /// controls are mapped. `again`: set every device up anew (it may have
    /// been unplugged meanwhile).
    fn sync_drivers(&mut self, again: bool) {
        let mut wanted: HashMap<Arc<str>, (usize, Arc<str>)> = HashMap::new();
        for (index, surface) in self.surfaces.iter().enumerate() {
            if surface.driver.is_some()
                && !surface.options.one_port
                && let Some(Some(output)) = self.outputs.get(index)
            {
                wanted.insert(self.devices[index].clone(), (index, output.clone()));
            }
        }
        // Gone, or driven differently: given back.
        let stale: Vec<Arc<str>> = self
            .drivers
            .iter()
            .filter(|(device, active)| {
                wanted.get(*device).is_none_or(|(index, output)| {
                    *output != active.output || self.surfaces[*index].driver != active.surface.driver
                })
            })
            .map(|(device, _)| device.clone())
            .collect();
        for device in stale {
            if let Some(mut active) = self.drivers.remove(&device) {
                let messages = active.driver.disconnect(&active.surface);
                self.feedback.raw(&active.output, &messages);
                log::info!("{device}: driver stopped");
            }
        }
        for (device, (index, output)) in wanted {
            let surface = self.surfaces[index].clone();
            let connect;
            let active = match self.drivers.entry(device.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    connect = again;
                    entry.into_mut()
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let Some(driver) = surface.driver.as_deref().and_then(driver::open) else {
                        log::warn!("{device}: no driver {:?}", surface.driver);
                        continue;
                    };
                    log::info!("{device}: driver {:?} started", surface.driver);
                    connect = true;
                    entry.insert(Active { driver, surface: surface.clone(), output: output.clone() })
                }
            };
            active.surface = surface.clone();
            let mut messages = if connect { active.driver.connect(&surface) } else { Vec::new() };
            messages.extend(active.driver.mapped(&surface, &self.mappings.mapped(index)));
            self.feedback.raw(&output, &messages);
        }
    }

    /// Gives every driven device back.
    fn close_drivers(&mut self) {
        for (device, mut active) in self.drivers.drain() {
            let messages = active.driver.disconnect(&active.surface);
            self.feedback.raw(&active.output, &messages);
            log::info!("{device}: driver stopped");
        }
    }

    /// Sends what the mappings did.
    fn flush(&mut self) {
        let writes = self.mappings.take_writes();
        if !writes.is_empty() {
            let mut rack = self.rack.lock().ok();
            for write in writes {
                let Some(target) = self.targets.get_mut(write.target) else { continue };
                let value = (target.to_value)(write.position);
                let send = target.last != Some(value);
                target.last = Some(value);
                let command = match &target.output {
                    Output::Param { key, param } => {
                        Some(MapperCommand::Param { key: *key, change: ParamChange { param_id: *param, value } })
                    }
                    Output::Volume { slot } => Some(MapperCommand::Volume { slot: *slot, gain: value }),
                    Output::Setbfree { function, .. } => {
                        if send && let Some(sink) = &target.setbfree {
                            sink.send(function, value.round().clamp(0.0, 127.0) as u8);
                        }
                        None
                    }
                    Output::Gui => None,
                };
                // No stream, or the audio thread isn't draining: dropped.
                if send
                    && let Some(command) = command
                    && let Some(Some(rack)) = rack.as_deref_mut()
                {
                    let _ = rack.push(command);
                }
                let _ = self.reports.try_send(MapperReport::Applied {
                    generation: self.generation,
                    target: write.target,
                    output: target.output.clone(),
                    position: write.position,
                    base: write.base,
                    value,
                    for_gui: Vec::new(),
                });
            }
        }
        let now = Instant::now();
        for feedback in self.mappings.take_feedback() {
            let (Some(device), Some(surface)) = (self.devices.get(feedback.surface), self.surfaces.get(feedback.surface))
            else {
                continue;
            };
            let driven = self.drivers.get(device);
            let messages = driven
                .and_then(|active| active.driver.feedback(surface, feedback.control, feedback.shown))
                .or_else(|| surface.feedback(feedback.control, feedback.shown, feedback.lit).map(|message| vec![message.to_vec()]));
            if let Some(messages) = messages {
                if let Some(Some(output)) = self.outputs.get(feedback.surface) {
                    self.feedback.send(output, feedback.control, messages, feedback.again, now);
                }
                let _ = self.reports.try_send(MapperReport::Feedback {
                    device: device.clone(),
                    control: feedback.control,
                    shown: feedback.shown,
                });
            }
        }
        for pick_up in self.mappings.take_pick_ups() {
            let (Some(device), Some(surface)) = (self.devices.get(pick_up.surface), self.surfaces.get(pick_up.surface))
            else {
                continue;
            };
            if let Some(active) = self.drivers.get_mut(device) {
                let messages = active.driver.pick_up(surface, pick_up.control, pick_up.up);
                let output = active.output.clone();
                self.feedback.raw(&output, &messages);
            }
        }
        for waiting in self.mappings.take_waiting() {
            let Some(device) = self.devices.get(waiting.surface) else { continue };
            let _ = self.reports.try_send(MapperReport::Waiting {
                generation: self.generation,
                device: device.clone(),
                control: waiting.control,
                at: waiting.at,
            });
        }
    }
}
