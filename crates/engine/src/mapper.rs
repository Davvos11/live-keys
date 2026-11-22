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
use live_keys_surfaces::{Shown, Surface};

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

/// Every mapping, as [`Engine::set_mappings`](crate::Engine::set_mappings)
/// takes them.
pub struct MapperSetup {
    /// The surfaces the controls are on: device name and surface.
    pub surfaces: Vec<(Arc<str>, Arc<Surface>)>,
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
    pub surfaces: Vec<(Arc<str>, Arc<Surface>)>,
    pub controls: Vec<ControlSetup>,
    /// With the setBfree sink of each `Output::Setbfree` target.
    pub targets: Vec<(TargetSpec, Option<SetbfreeSink>)>,
}

pub(crate) enum MapperInput {
    Event(SurfaceEvent),
    Setup(Box<Setup>),
    /// A target of setup `generation` was moved elsewhere.
    Position { generation: u64, target: TargetId, position: f32 },
    /// Devices may have been (re)connected: open outputs again and send every
    /// control that shows values its value.
    Resync,
}

/// A control, as (device, control index).
type ControlKey = (Arc<str>, usize);

/// Sending values back to controls: an output connection per device, and
/// per control when it was last sent one and what's waiting to be.
#[derive(Default)]
struct FeedbackOut {
    /// By device name; `None`: it has no output (or it failed), until a resync.
    outputs: HashMap<Arc<str>, Option<midir::MidiOutputConnection>>,
    sent: HashMap<ControlKey, Instant>,
    waiting: HashMap<ControlKey, [u8; 3]>,
    /// The message last sent, so the same isn't sent again (until a resync).
    last: HashMap<ControlKey, [u8; 3]>,
}

impl FeedbackOut {
    /// Sends `message` to `device`'s control `control`, or keeps it until
    /// [`FEEDBACK_INTERVAL`] after the last.
    fn send(&mut self, device: &Arc<str>, control: usize, message: [u8; 3], now: Instant) {
        let key = (device.clone(), control);
        if self.last.get(&key) == Some(&message) {
            self.waiting.remove(&key);
            return;
        }
        if self.sent.get(&key).is_some_and(|sent| now.duration_since(*sent) < FEEDBACK_INTERVAL) {
            self.waiting.insert(key, message);
            return;
        }
        self.waiting.remove(&key);
        self.sent.insert(key.clone(), now);
        self.last.insert(key, message);
        let output = self.outputs.entry(device.clone()).or_insert_with(|| open_output(device));
        if let Some(output) = output
            && let Err(err) = output.send(&message)
        {
            log::warn!("feedback to {device} failed: {err}");
        }
    }

    /// Sends what waited long enough.
    fn send_due(&mut self, now: Instant) {
        let due: Vec<(ControlKey, [u8; 3])> = self
            .waiting
            .iter()
            .filter(|(key, _)| self.sent.get(*key).is_none_or(|sent| now.duration_since(*sent) >= FEEDBACK_INTERVAL))
            .map(|(key, message)| (key.clone(), *message))
            .collect();
        for ((device, control), message) in due {
            self.send(&device, control, message, now);
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
    devices: Vec<Arc<str>>,
    surfaces: Vec<Arc<Surface>>,
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
        surfaces: Vec::new(),
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
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match inputs.recv() {
                Ok(input) => Some(input),
                Err(_) => return,
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
                if let Some(surface) = self.devices.iter().position(|device| *device == event.device) {
                    self.mappings.event(surface, &event.event, Instant::now());
                }
                // Full: the panel misses a move, nothing else.
                let _ = self.reports.try_send(MapperReport::Event(event));
            }
            MapperInput::Setup(setup) => {
                self.flush();
                let positions = setup.targets.iter().map(|(spec, _)| spec.position).collect();
                self.devices = setup.surfaces.iter().map(|(device, _)| device.clone()).collect();
                self.surfaces = setup.surfaces.into_iter().map(|(_, surface)| surface).collect();
                self.mappings = Mappings::new(self.surfaces.clone(), setup.controls, positions);
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
                self.mappings.resync_feedback();
            }
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
            if let Some(message) = surface.feedback(feedback.control, feedback.shown) {
                self.feedback.send(device, feedback.control, message, now);
                let _ = self.reports.try_send(MapperReport::Feedback {
                    device: device.clone(),
                    control: feedback.control,
                    shown: feedback.shown,
                });
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
