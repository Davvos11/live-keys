use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use live_keys_core::{
    ALL_INPUTS, Consumer, HostedPlugin, MidiEvent, MidiFilter, ParamChange, ParamInfo, ParamKind, PluginFormat,
    PluginInfo, PluginKind, PresetInfo, Producer, PushError, ring_buffer,
};
use live_keys_lv2_host::{GuiLeases, InstanceRef, Lv2Host, Lv2Scan, PortEvent, UiChannel, livi, lv2_raw};
use live_keys_vst3_host::{Vst3Controller, Vst3Host, Vst3Scan};
use live_keys_clap_host::{ClapController, ClapHost, ClapScan};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::find_output_device;
use crate::midi::{MidiInputs, RoutedMidi};
use crate::paths::build_lv2_path;
use crate::rack::{
    BoxedPlugin, ChainPosition, MAX_EFFECTS, MAX_MIDI_EFFECTS, Rack, RackCommand, Section, SlotPeaks,
};

/// Number of plugin slots in the rack. Every slot sees the same MIDI input (each
/// through its own [`MidiFilter`]), runs it through its chain (up to
/// [`MAX_MIDI_EFFECTS`] MIDI effects, an instrument, then up to [`MAX_EFFECTS`]
/// audio effects), and the slots' outputs are summed, each at its own gain, into
/// the one output stream.
pub const SLOT_COUNT: usize = 6;

const MIDI_RING_CAPACITY: usize = 1024;
const COMMAND_RING_CAPACITY: usize = 256;
/// Plugins taken out of a chain, waiting to be dropped off the audio thread.
/// Drained on every structural change, so it only needs room for a few in flight;
/// sized so even clearing every chain at once fits.
const RETIRED_RING_CAPACITY: usize = SLOT_COUNT * (MAX_MIDI_EFFECTS + 1 + MAX_EFFECTS) * 2;
/// Plugins lent to the control thread at once — one at a time in practice.
const LENT_RING_CAPACITY: usize = 4;
/// How long to wait for the audio thread to hand over a lent plugin: a few
/// callbacks normally; longer means it has stalled.
const LEND_TIMEOUT: Duration = Duration::from_secs(2);
/// How long [`Engine::switch_output`] waits for the old stream to hand the rack over.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);

/// Identifies one loaded plugin instance for as long as it stays loaded, however
/// its position in a chain changes. Never reused within a process.
pub type PluginKey = u64;

/// Per-slot mixer/routing settings. They belong to the slot rather than to the
/// plugins in it, so they survive loading different plugins and restarting the
/// stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotSettings {
    pub filter: MidiFilter,
    /// Linear output gain (1.0 = unity).
    pub gain: f32,
}

impl Default for SlotSettings {
    fn default() -> Self {
        Self {
            filter: MidiFilter::default(),
            gain: 1.0,
        }
    }
}

/// What it takes to open a loaded plugin's own GUI or work with its presets, per
/// format.
pub enum PluginEditor {
    Lv2 {
        plugin: livi::Plugin,
        features: Arc<livi::Features>,
        raw_handle: lv2_raw::core::LV2Handle,
        /// For saving and restoring its state (presets).
        instance: InstanceRef,
        /// Atoms between the plugin's atom ports and its GUI, if it has any (see
        /// [`Engine::send_ui_atom`], [`Engine::drain_ui_events`]).
        ui_channel: Option<UiChannel>,
        /// A GUI using `raw_handle` holds one of these while open (see
        /// [`GuiLease`](live_keys_lv2_host::GuiLease)).
        gui_leases: GuiLeases,
    },
    /// The plugin's main-thread half. Its editor is created from it.
    Vst3(Vst3Controller),
    /// The plugin's main-thread half, which [`Engine::idle_plugins`] pumps. Its
    /// GUI is created from it.
    Clap(ClapController),
}

/// The main-thread view of one loaded plugin, captured at load time since the
/// running plugin itself gets moved into the audio callback and isn't reachable
/// from `Engine` afterwards.
pub struct LoadedPluginMeta {
    pub key: PluginKey,
    /// [`PluginInfo::id`] of the plugin.
    pub id: String,
    pub name: String,
    pub format: PluginFormat,
    /// Parameter metadata, for a parameter-editor UI (macro knobs, a fallback UI
    /// for GUI-less plugins, future MIDI binding).
    pub params: Vec<ParamInfo>,
    pub editor: PluginEditor,
    /// The last value sent per parameter, for reading parameters back (the
    /// running plugin is out of reach) — see [`Engine::param_value`].
    values: HashMap<u32, f32>,
}

/// A slot's chain as the control thread sees it — a mirror of the audio thread's,
/// kept in step by applying every change to both.
#[derive(Default)]
pub struct SlotChain {
    /// In processing order, in front of the instrument.
    pub midi_effects: Vec<LoadedPluginMeta>,
    pub instrument: Option<LoadedPluginMeta>,
    /// In processing order, after the instrument.
    pub audio_effects: Vec<LoadedPluginMeta>,
}

impl SlotChain {
    pub fn effects(&self, section: Section) -> &[LoadedPluginMeta] {
        match section {
            Section::Midi => &self.midi_effects,
            Section::Audio => &self.audio_effects,
        }
    }

    fn effects_mut(&mut self, section: Section) -> &mut Vec<LoadedPluginMeta> {
        match section {
            Section::Midi => &mut self.midi_effects,
            Section::Audio => &mut self.audio_effects,
        }
    }

    /// Every plugin in processing order.
    pub fn plugins(&self) -> impl Iterator<Item = &LoadedPluginMeta> {
        self.midi_effects
            .iter()
            .chain(&self.instrument)
            .chain(&self.audio_effects)
    }

    fn plugins_mut(&mut self) -> impl Iterator<Item = &mut LoadedPluginMeta> {
        self.midi_effects
            .iter_mut()
            .chain(&mut self.instrument)
            .chain(&mut self.audio_effects)
    }

    /// The plugin at `position`, if any.
    pub fn at(&self, position: ChainPosition) -> Option<&LoadedPluginMeta> {
        match position {
            ChainPosition::Instrument => self.instrument.as_ref(),
            ChainPosition::MidiEffect(index) => self.midi_effects.get(index),
            ChainPosition::Effect(index) => self.audio_effects.get(index),
        }
    }

    /// Where the plugin with `key` is, if it's in this chain.
    pub fn position_of(&self, key: PluginKey) -> Option<ChainPosition> {
        if self.instrument.as_ref().is_some_and(|meta| meta.key == key) {
            return Some(ChainPosition::Instrument);
        }
        let find = |effects: &[LoadedPluginMeta]| effects.iter().position(|meta| meta.key == key);
        find(&self.midi_effects)
            .map(ChainPosition::MidiEffect)
            .or_else(|| find(&self.audio_effects).map(ChainPosition::Effect))
    }
}

/// What the audio callback owns: the rack and the queues it drains. Handed from
/// one stream to the next when the output device changes, so plugins keep
/// running (see [`Engine::switch_output`]).
struct AudioState {
    rack: Rack,
    command_rx: Consumer<RackCommand>,
    midi_rx: Consumer<RoutedMidi>,
}

/// The running output stream, and the control-thread ends of the queues
/// into/out of its audio callback (but MIDI's, which is in [`MidiInputs`]).
struct RunningStream {
    stream: cpal::Stream,
    /// Where the callback hands its [`AudioState`] on [`RackCommand::Detach`].
    handoff_rx: Consumer<Box<AudioState>>,
    sample_rate: f64,
    command_tx: Producer<RackCommand>,
    peaks: SlotPeaks,
    retired_rx: Consumer<BoxedPlugin>,
    lent_rx: Consumer<BoxedPlugin>,
    /// Where a lent plugin goes back to that arrived too late (see
    /// [`Engine::with_lent`]); it's returned as soon as it shows up.
    overdue_lend: Option<(usize, ChainPosition)>,
}

/// Where [`Engine::restore`] reads a plugin's settings from.
#[derive(Clone, Copy)]
enum StateSource<'a> {
    /// A [`PresetInfo::id`].
    Preset(&'a str),
    /// A file from [`Engine::save_state_file`].
    File(&'a Path),
}

impl StateSource<'_> {
    /// # Safety
    /// As for [`Lv2Host::restore_preset`].
    unsafe fn restore_lv2(
        self,
        lv2: &Lv2Host,
        instance: InstanceRef,
        plugin_uri: &str,
    ) -> Result<Vec<(u32, f32)>> {
        unsafe {
            match self {
                StateSource::Preset(uri) => lv2.restore_preset(instance, plugin_uri, uri),
                StateSource::File(path) => lv2.restore_file(instance, plugin_uri, path),
            }
        }
    }
}

/// Owns the plugin hosts, the running audio/MIDI stream (if any), and the chain of
/// plugins loaded into each rack slot. Real-time wiring (ring buffers, no
/// allocation in the audio callback — see `rack.rs`) lives here so every host
/// (CLI, GUI) shares one implementation instead of drifting apart.
const NOT_SCANNED: &str = "plugins haven't been scanned yet";

/// Every format's host, from one [`PluginScan`].
struct Hosts {
    lv2: Lv2Host,
    vst3: Vst3Host,
    clap: ClapHost,
}

/// A plugin scan in progress: LV2 on a thread of its own, VST3 and CLAP a step
/// at a time in [`PluginScan::step`] on the thread that will use the plugins (see
/// [`Vst3Scan`], [`ClapScan`]), so a GUI can keep drawing while it runs.
/// Installed with [`Engine::install_plugins`].
pub struct PluginScan {
    lv2: std::thread::JoinHandle<Lv2Scan>,
    vst3: Vst3Scan,
    clap: ClapScan,
}

impl PluginScan {
    /// Starts scanning the default plugin folders plus `extra_dirs`. Sets
    /// `LV2_PATH` (for this scan and the LV2 preset world), so call it from the
    /// control thread, with no other scan running.
    pub fn start(extra_dirs: &[PathBuf]) -> Self {
        // SAFETY: only ever called from the control thread; nothing else in this
        // process reads or writes environment variables concurrently.
        unsafe { std::env::set_var("LV2_PATH", build_lv2_path(extra_dirs)) };
        Self {
            lv2: std::thread::Builder::new()
                .name("lv2-scan".into())
                .spawn(Lv2Scan::run)
                .expect("failed to spawn the LV2 scan thread"),
            vst3: Vst3Scan::new(extra_dirs),
            clap: ClapScan::new(extra_dirs),
        }
    }

    /// Does one step of the VST3 scan, or once that's done the CLAP one; `true`
    /// once the whole scan is done.
    pub fn step(&mut self) -> bool {
        !self.vst3.step() && !self.clap.step() && self.lv2.is_finished()
    }

    fn finish(self, max_block_size: usize) -> Hosts {
        let vst3 = self.vst3.finish(max_block_size);
        let clap = self.clap.finish(max_block_size);
        let lv2 = match self.lv2.join() {
            Ok(scan) => scan,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        Hosts { lv2: Lv2Host::from_scan(lv2, max_block_size), vst3, clap }
    }
}

pub struct Engine {
    // Declared (so dropped) before the hosts: the stream's callback and retired
    // queue can still hold plugins, and an LV2 plugin must not outlive its world.
    running: Option<RunningStream>,
    /// An [`AudioState`] taken from a stream that couldn't be replaced; dropped
    /// with the next stream teardown (never on the audio thread).
    orphaned: Option<Box<AudioState>>,
    midi_inputs: MidiInputs,
    audio_host: cpal::Host,
    /// `None` until the first [`PluginScan`] is installed.
    hosts: Option<Hosts>,
    max_block_size: usize,
    slots: [SlotChain; SLOT_COUNT],
    settings: [SlotSettings; SLOT_COUNT],
    next_key: PluginKey,
}

impl Engine {
    /// `max_block_size` must be at least as large as the largest number of frames the
    /// audio backend will ever hand us in one callback.
    /// Scans the default plugin folders first; see [`Engine::unscanned`] to show
    /// something meanwhile.
    pub fn new(max_block_size: usize) -> Self {
        let mut engine = Self::unscanned(max_block_size);
        engine.install_plugins(PluginScan::start(&[]));
        engine
    }

    /// An engine that knows no plugins until a [`PluginScan`] is installed
    /// ([`Engine::install_plugins`]).
    pub fn unscanned(max_block_size: usize) -> Self {
        Self {
            audio_host: cpal::default_host(),
            hosts: None,
            max_block_size,
            running: None,
            orphaned: None,
            midi_inputs: MidiInputs::new(),
            slots: Default::default(),
            settings: Default::default(),
            next_key: 0,
        }
    }

    /// Every plugin found by the last scan, LV2 first, then VST3, then CLAP.
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        let Some(hosts) = &self.hosts else { return Vec::new() };
        let mut plugins = hosts.lv2.list_plugins();
        plugins.extend(hosts.vst3.list_plugins());
        plugins.extend(hosts.clap.list_plugins());
        plugins
    }

    /// Which chain section a plugin goes into by default: MIDI effects in front
    /// of the instrument, everything else after it. Unknown ids count as audio.
    pub fn section_for(&self, plugin_id: &str) -> Section {
        let is_midi = self
            .list_plugins()
            .iter()
            .any(|info| info.id == plugin_id && info.kind == PluginKind::MidiEffect);
        if is_midi { Section::Midi } else { Section::Audio }
    }

    fn hosts(&self) -> Result<&Hosts> {
        self.hosts.as_ref().context(NOT_SCANNED)
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The chain loaded into `slot`. Out-of-range slots read as `None`.
    pub fn slot(&self, slot: usize) -> Option<&SlotChain> {
        self.slots.get(slot)
    }

    /// The loaded plugin with `key`, in whichever slot and position it is.
    pub fn plugin(&self, key: PluginKey) -> Option<&LoadedPluginMeta> {
        self.slots
            .iter()
            .flat_map(SlotChain::plugins)
            .find(|meta| meta.key == key)
    }

    fn plugin_mut(&mut self, key: PluginKey) -> Option<&mut LoadedPluginMeta> {
        self.slots
            .iter_mut()
            .find_map(|chain| chain.plugins_mut().find(|meta| meta.key == key))
    }

    /// Where parameter `param_id` of the plugin with `key` is: the last value
    /// sent to it (from any source, presets included), else its default. `None`
    /// if there's no such plugin or parameter.
    pub fn param_value(&self, key: PluginKey, param_id: u32) -> Option<f32> {
        let meta = self.plugin(key)?;
        meta.values.get(&param_id).copied().or_else(|| {
            meta.params
                .iter()
                .find(|param| param.id == param_id)
                .map(|param| param.default_value)
        })
    }

    fn locate(&self, key: PluginKey) -> Option<(usize, ChainPosition)> {
        self.slots
            .iter()
            .enumerate()
            .find_map(|(slot, chain)| Some((slot, chain.position_of(key)?)))
    }

    /// The MIDI filter and gain of `slot`. Out-of-range slots read as defaults.
    pub fn slot_settings(&self, slot: usize) -> SlotSettings {
        self.settings.get(slot).copied().unwrap_or_default()
    }

    /// Sets which MIDI `slot` responds to. Takes effect on the next audio block if
    /// running, and is kept for whenever the stream (re)starts.
    pub fn set_slot_filter(&mut self, slot: usize, filter: MidiFilter) {
        if slot < SLOT_COUNT {
            self.settings[slot].filter = filter;
            self.push_command(RackCommand::Filter { slot, filter });
        }
    }

    /// Sets `slot`'s linear output gain (1.0 = unity). Like
    /// [`Engine::set_slot_filter`], kept across loads and restarts.
    pub fn set_slot_gain(&mut self, slot: usize, gain: f32) {
        if slot < SLOT_COUNT {
            self.settings[slot].gain = gain;
            self.push_command(RackCommand::Gain { slot, gain });
        }
    }

    /// Plays a MIDI event as if it came from every MIDI input (an on-screen
    /// keyboard, tests). Fails if the stream isn't running or the queue is full.
    pub fn send_midi(&mut self, event: MidiEvent) -> Result<()> {
        let mut midi_tx = self.midi_inputs.tx.lock().map_err(|_| anyhow::anyhow!("MIDI queue poisoned"))?;
        let midi_tx = midi_tx.as_mut().context("audio stream isn't running")?;
        midi_tx
            .push(RoutedMidi { inputs: ALL_INPUTS, event })
            .map_err(|_| anyhow::anyhow!("MIDI queue full"))
    }

    /// Sets which MIDI devices (by [`midi_device_name`](crate::midi_device_name))
    /// feed each MIDI input — input `n` is bit `n` of a slot's
    /// [`MidiFilter::inputs`] — connecting those plugged in now. Returns every
    /// MIDI input port present. Kept across stream restarts.
    pub fn set_midi_inputs(&mut self, devices: Vec<Vec<String>>) -> Vec<String> {
        self.midi_inputs.set_devices(devices)
    }

    /// Connects assigned MIDI devices that were plugged in since the last call
    /// and lets go of ones that went away; call it periodically. Returns every
    /// MIDI input port present.
    pub fn sync_midi_inputs(&mut self) -> Vec<String> {
        self.midi_inputs.sync()
    }

    /// The loudest sample (absolute, after the slot's gain) slot `slot` played
    /// since the last call, for a level meter; 0 when stopped.
    pub fn take_slot_peak(&mut self, slot: usize) -> f32 {
        match (&self.running, slot < SLOT_COUNT) {
            (Some(running), true) => f32::from_bits(running.peaks[slot].swap(0, Ordering::Relaxed)),
            _ => 0.0,
        }
    }

    /// Sends a command whose loss is tolerable (a param or setting change) to the
    /// audio callback, if running. Dropped (and logged) if the queue is full — that
    /// means the audio thread isn't draining it, which is worth knowing about even
    /// though there's little to do but drop.
    fn push_command(&mut self, command: RackCommand) {
        if let Some(running) = &mut self.running
            && running.command_tx.push(command).is_err()
        {
            log::warn!("dropped rack command, queue full");
        }
    }

    /// Sends a command that changes a chain's structure. Unlike
    /// [`Engine::push_command`] it reports failure, since the caller must then leave
    /// its mirror of the chain unchanged to stay in step with the audio thread.
    fn push_structural(&mut self, command: RackCommand) -> Result<()> {
        self.collect_retired();
        let running = self.running.as_mut().context("audio stream isn't running")?;
        running
            .command_tx
            .push(command)
            .map_err(|_| anyhow::anyhow!("rack command queue full, audio thread isn't draining it"))
    }

    /// Replaces the plugins known with what `scan` found, waiting for it to finish
    /// if it hasn't. Stops the stream and empties every slot first: an LV2 plugin
    /// must not outlive the world it came from.
    pub fn install_plugins(&mut self, scan: PluginScan) {
        self.stop();
        self.collect_retired();
        self.hosts = None;
        self.hosts = Some(scan.finish(self.max_block_size));
    }

    /// Stops the running stream, if any, and empties every slot. MIDI devices stay
    /// connected (their events are dropped until a stream runs again).
    pub fn stop(&mut self) {
        self.drop_stream_async();
        self.slots = Default::default();
    }

    /// Moves any currently running stream to a throwaway thread for
    /// teardown, rather than dropping them in place. `cpal`'s ALSA `Stream::drop()`
    /// joins its own internal audio thread; if that thread is stuck for any reason
    /// (observed after a previous session's plugin UI wedged internally — see
    /// `live-keys-lv2-ui`'s docs), dropping in place would block the caller —
    /// typically the GUI thread, e.g. when switching devices — waiting on it
    /// forever. A stuck old thread is abandoned (leaked until the app exits) rather
    /// than allowed to freeze anything new.
    fn drop_stream_async(&mut self) {
        if let Ok(mut midi_tx) = self.midi_inputs.tx.lock() {
            *midi_tx = None;
        }
        let orphaned = self.orphaned.take();
        if let Some(running) = self.running.take() {
            std::thread::spawn(move || drop((running, orphaned)));
        } else if orphaned.is_some() {
            std::thread::spawn(move || drop(orphaned));
        }
    }

    /// Drops plugins the audio callback has taken out of a chain. They're handed
    /// back here rather than dropped on the audio thread, since a plugin's teardown
    /// can deallocate or block. Called on every structural change; hosts may also
    /// call it periodically so a removed plugin doesn't linger until the next one.
    pub fn collect_retired(&mut self) {
        // CLAP plugins let go of on other threads are torn down here.
        live_keys_clap_host::destroy_pending();
        if let Some(running) = &mut self.running {
            while let Ok(plugin) = running.retired_rx.pop() {
                drop(plugin);
            }
            if let Some((slot, position)) = running.overdue_lend
                && let Ok(plugin) = running.lent_rx.pop()
            {
                running.overdue_lend = None;
                Self::return_lent(running, slot, position, plugin);
            }
        }
    }

    /// Runs `work` on the plugin at `position` in `slot` while it's off the audio
    /// thread: the audio callback hands it over and leaves its place empty (the
    /// slot goes silent for an instrument, an effect is skipped) until it's put
    /// back. For LV2 state save/restore, which mustn't overlap processing. Waits
    /// up to [`LEND_TIMEOUT`] for the handover; a plugin that turns up later is
    /// put back by [`Engine::collect_retired`].
    fn with_lent<T>(
        &mut self,
        slot: usize,
        position: ChainPosition,
        work: impl FnOnce(&Lv2Host, &mut dyn HostedPlugin) -> T,
    ) -> Result<T> {
        self.push_structural(RackCommand::Lend { slot, position })?;
        let running = self.running.as_mut().context("audio stream isn't running")?;
        let deadline = Instant::now() + LEND_TIMEOUT;
        let mut plugin = loop {
            if let Ok(plugin) = running.lent_rx.pop() {
                break plugin;
            }
            if Instant::now() > deadline {
                running.overdue_lend = Some((slot, position));
                anyhow::bail!("the audio thread didn't hand the plugin over");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let result = match &self.hosts {
            Some(hosts) => Ok(work(&hosts.lv2, plugin.as_mut())),
            None => Err(anyhow::anyhow!(NOT_SCANNED)),
        };
        let running = self.running.as_mut().context("audio stream isn't running")?;
        Self::return_lent(running, slot, position, plugin);
        result
    }

    fn return_lent(running: &mut RunningStream, slot: usize, position: ChainPosition, plugin: BoxedPlugin) {
        let mut command = RackCommand::Return {
            slot,
            position,
            plugin,
        };
        // The queue only stays full if the audio thread has stalled; rather than
        // block forever, the plugin is then given up (dropped here).
        for _ in 0..1000 {
            match running.command_tx.push(command) {
                Ok(()) => return,
                Err(PushError::Full(back)) => command = back,
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        log::error!("couldn't return a lent plugin to slot {slot}: command queue full");
    }

    /// Forwards a parameter change that came from a plugin's own GUI to the plugin
    /// with `key`, applied on the next audio block. A no-op if that plugin is gone
    /// or nothing is running. A CLAP plugin has applied its GUI's changes itself
    /// (they come from [`Engine::idle_plugins`]), so for one they're only recorded.
    pub fn send_param_change(&mut self, key: PluginKey, param_id: u32, value: f32) {
        if let Some(meta) = self.plugin_mut(key) {
            meta.values.insert(param_id, value);
            if meta.format == PluginFormat::Clap {
                return;
            }
        }
        self.queue_param(key, param_id, value);
    }

    fn queue_param(&mut self, key: PluginKey, param_id: u32, value: f32) {
        if let Some((slot, position)) = self.locate(key) {
            self.push_command(RackCommand::Param {
                slot,
                position,
                change: ParamChange { param_id, value },
            });
        }
    }

    /// Sets a parameter of the plugin with `key` from the host side (e.g. a
    /// parameter slider): like [`Engine::send_param_change`], but a VST3 plugin's
    /// controller is told too, so its own GUI follows. (An LV2 plugin's GUI reads
    /// port values itself; a CLAP plugin tells its GUI.)
    pub fn set_param(&mut self, key: PluginKey, param_id: u32, value: f32) {
        let Some(meta) = self.plugin_mut(key) else { return };
        if let PluginEditor::Vst3(controller) = &meta.editor {
            controller.set_param_normalized(param_id, value);
        }
        meta.values.insert(param_id, value);
        self.queue_param(key, param_id, value);
    }

    /// Does what CLAP plugins asked of the main thread (their callbacks, timers
    /// and fds, which their GUIs run on), and hands `on_edit` the parameter
    /// changes they made themselves — in their own GUIs — for
    /// [`Engine::send_param_change`]. Returns the plugins whose values changed
    /// otherwise (a loaded state that took effect late), re-read into
    /// [`Engine::param_value`] already. Call regularly (every ~16 ms) from the
    /// main thread.
    pub fn idle_plugins(&mut self, mut on_edit: impl FnMut(PluginKey, u32, f32)) -> Vec<PluginKey> {
        let mut changed = Vec::new();
        for meta in self.slots.iter_mut().flat_map(SlotChain::plugins_mut) {
            let PluginEditor::Clap(controller) = &meta.editor else { continue };
            if controller.idle(|param_id, value| on_edit(meta.key, param_id, value)) {
                for param in &meta.params {
                    if let Some(value) = controller.param_value(param.id) {
                        meta.values.insert(param.id, value);
                    }
                }
                changed.push(meta.key);
            }
        }
        changed
    }

    /// Passes an atom its GUI wrote (`LV2_Atom` header, then body) to atom input
    /// `port` of the LV2 plugin with `key`, from its next block. Dropped if
    /// malformed, too big, or the channel is full.
    pub fn send_ui_atom(&mut self, key: PluginKey, port: u32, atom: &[u8]) {
        if let Some(LoadedPluginMeta {
            editor: PluginEditor::Lv2 { ui_channel: Some(channel), .. },
            ..
        }) = self.plugin_mut(key)
            && let Some(event) = PortEvent::from_atom(port, atom)
        {
            let _ = channel.to_plugin.push(event);
        }
    }

    /// Hands every event LV2 plugins wrote to their atom outputs since the last
    /// call to `forward` (as the plugin's key, the port and the atom for its GUI),
    /// emptying their channels. Call regularly, GUI or not: a full channel drops
    /// newer events.
    pub fn drain_ui_events(&mut self, mut forward: impl FnMut(PluginKey, u32, &PortEvent)) {
        for chain in &mut self.slots {
            for meta in chain.plugins_mut() {
                if let PluginEditor::Lv2 { ui_channel: Some(channel), .. } = &mut meta.editor {
                    while let Ok(event) = channel.from_plugin.pop() {
                        forward(meta.key, event.port, &event);
                    }
                }
            }
        }
    }

    /// Parameter `param_id` of the plugin with `key` at `value`, as text for display
    /// ("-6.0 dB", "Saw", "C#3"). VST3 and CLAP plugins format their own values;
    /// otherwise it's formatted from the parameter's metadata. Empty if there's no
    /// such parameter.
    pub fn param_text(&self, key: PluginKey, param_id: u32, value: f32) -> String {
        let Some(meta) = self.plugin(key) else {
            return String::new();
        };
        let Some(info) = meta.params.iter().find(|param| param.id == param_id) else {
            return String::new();
        };
        if let PluginEditor::Clap(controller) = &meta.editor
            && info.kind != ParamKind::Choice
            && let Some(text) = controller.value_text(param_id, value)
        {
            return text;
        }
        if let PluginEditor::Vst3(controller) = &meta.editor
            && info.kind != ParamKind::Choice
            && let Some(text) = controller.value_text(param_id, value)
        {
            return if info.unit.is_empty() || text.ends_with(info.unit.as_str()) {
                text
            } else {
                format!("{text} {}", info.unit)
            };
        }
        info.format_value(value)
    }

    /// The presets of the plugin with `key`: shipped with it, or saved (here or by
    /// another host) in the format's standard user folder.
    pub fn presets(&self, key: PluginKey) -> Vec<PresetInfo> {
        match self.plugin(key) {
            Some(meta) => match (&self.hosts, meta.format) {
                (Some(hosts), PluginFormat::Lv2) => hosts.lv2.presets(&meta.id),
                (Some(hosts), PluginFormat::Vst3) => hosts.vst3.presets(&meta.id),
                (Some(hosts), PluginFormat::Clap) => hosts.clap.presets(&meta.id),
                (None, _) => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Applies preset `preset_id` (a [`PresetInfo::id`]) to the plugin with
    /// `key`. Its parameters' new values can be read back with
    /// [`Engine::param_value`] afterwards. An LV2 plugin with state of its own is
    /// briefly taken off the audio thread for this (see [`Engine::with_lent`]).
    pub fn load_preset(&mut self, key: PluginKey, preset_id: &str) -> Result<()> {
        self.restore(key, StateSource::Preset(preset_id))
    }

    /// Like [`Engine::load_preset`], from a state file written by
    /// [`Engine::save_state_file`].
    pub fn load_state_file(&mut self, key: PluginKey, path: &Path) -> Result<()> {
        self.restore(key, StateSource::File(path))
    }

    fn restore(&mut self, key: PluginKey, source: StateSource<'_>) -> Result<()> {
        let (slot, position) = self.locate(key).context("plugin isn't loaded")?;
        let meta = self.plugin(key).context("plugin isn't loaded")?;
        let plugin_id = meta.id.clone();
        match &meta.editor {
            PluginEditor::Lv2 { instance, .. } => {
                let instance = *instance;
                if instance.has_state() {
                    let values = self.with_lent(slot, position, |lv2, plugin| {
                        // SAFETY: the plugin is lent, so it isn't running.
                        let values = unsafe { source.restore_lv2(lv2, instance, &plugin_id) }?;
                        for &(index, value) in &values {
                            plugin.set_param(index, value);
                        }
                        anyhow::Ok(values)
                    })??;
                    let meta = self.plugin_mut(key).context("plugin isn't loaded")?;
                    meta.values.extend(values);
                } else {
                    // SAFETY: without state, restoring only reports port values.
                    let values = unsafe { source.restore_lv2(&self.hosts()?.lv2, instance, &plugin_id) }?;
                    for (index, value) in values {
                        self.send_param_change(key, index, value);
                    }
                }
            }
            PluginEditor::Vst3(controller) => {
                let path = match source {
                    StateSource::Preset(path) => path.to_string(),
                    StateSource::File(path) => path.display().to_string(),
                };
                self.hosts()?.vst3.load_preset(controller, &plugin_id, &path)?;
                // The processor has the preset's values now; only our record of
                // them needs updating, from the controller.
                let values: Vec<(u32, f32)> = meta
                    .params
                    .iter()
                    .map(|param| (param.id, controller.param_normalized(param.id)))
                    .collect();
                let meta = self.plugin_mut(key).context("plugin isn't loaded")?;
                meta.values.extend(values);
            }
            PluginEditor::Clap(controller) => {
                let clap = &self.hosts()?.clap;
                match source {
                    StateSource::Preset(id) => clap.load_preset(controller, &plugin_id, id)?,
                    StateSource::File(path) => clap.load_file(controller, &plugin_id, path)?,
                }
                // As for VST3: only our record of the values needs updating.
                let values: Vec<(u32, f32)> = meta
                    .params
                    .iter()
                    .filter_map(|param| Some((param.id, controller.param_value(param.id)?)))
                    .collect();
                let meta = self.plugin_mut(key).context("plugin isn't loaded")?;
                meta.values.extend(values);
            }
        }
        Ok(())
    }

    /// Saves the current settings of the plugin with `key` as user preset `name`,
    /// replacing one of the same name.
    pub fn save_preset(&mut self, key: PluginKey, name: &str) -> Result<PresetInfo> {
        let meta = self.plugin(key).context("plugin isn't loaded")?;
        let (plugin_id, plugin_name) = (meta.id.clone(), meta.name.clone());
        match &meta.editor {
            PluginEditor::Lv2 { .. } => self.save_lv2(key, |lv2, instance, value| {
                // SAFETY: as `save_lv2` guarantees.
                unsafe { lv2.save_preset(instance, &plugin_id, &plugin_name, name, value) }
            }),
            PluginEditor::Vst3(controller) => self.hosts()?.vst3.save_preset(controller, &plugin_id, name),
            PluginEditor::Clap(controller) => self.hosts()?.clap.save_preset(controller, &plugin_id, name),
        }
    }

    /// Saves the current settings of the plugin with `key` into folder `dir`, as a
    /// state file (or bundle) named after `stem` that isn't one of the plugin's
    /// presets, for a bigger preset of the caller's. Returns the path to pass to
    /// [`Engine::load_state_file`].
    pub fn save_state_file(&mut self, key: PluginKey, dir: &Path, stem: &str) -> Result<PathBuf> {
        let meta = self.plugin(key).context("plugin isn't loaded")?;
        let plugin_id = meta.id.clone();
        match &meta.editor {
            PluginEditor::Lv2 { .. } => {
                let bundle = dir.join(format!("{stem}.lv2"));
                self.save_lv2(key, |lv2, instance, value| {
                    // SAFETY: as `save_lv2` guarantees.
                    unsafe { lv2.save_file(instance, &plugin_id, &bundle, value) }
                })
            }
            PluginEditor::Vst3(controller) => {
                let path = dir.join(format!("{stem}.vstpreset"));
                self.hosts()?.vst3.save_file(controller, &plugin_id, &path)?;
                Ok(path)
            }
            PluginEditor::Clap(controller) => {
                let path = dir.join(format!("{stem}.{}", live_keys_clap_host::STATE_FILE_EXTENSION));
                self.hosts()?.clap.save_file(controller, &plugin_id, &path)?;
                Ok(path)
            }
        }
    }

    /// Runs `save` for the LV2 plugin with `key` with its instance and every
    /// control port's value (as last sent, else its default) — lent off the audio
    /// thread if it has state, so `save` may capture it.
    fn save_lv2<T>(
        &mut self,
        key: PluginKey,
        save: impl FnOnce(&Lv2Host, InstanceRef, &dyn Fn(u32) -> f32) -> Result<T>,
    ) -> Result<T> {
        let (slot, position) = self.locate(key).context("plugin isn't loaded")?;
        let meta = self.plugin(key).context("plugin isn't loaded")?;
        let PluginEditor::Lv2 { plugin, instance, .. } = &meta.editor else {
            bail!("not an LV2 plugin");
        };
        let instance = *instance;
        let values: HashMap<u32, f32> = plugin
            .ports_with_type(livi::PortType::ControlInput)
            .map(|port| {
                let index = port.index.0 as u32;
                (index, meta.values.get(&index).copied().unwrap_or(port.default_value))
            })
            .collect();
        let value = |index: u32| values.get(&index).copied().unwrap_or_default();
        if instance.has_state() {
            self.with_lent(slot, position, |lv2, _| save(lv2, instance, &value))?
        } else {
            // Without state, saving only reads port values.
            save(&self.hosts()?.lv2, instance, &value)
        }
    }

    /// Deletes user preset `preset_id` of the plugin with `key`.
    pub fn delete_preset(&mut self, key: PluginKey, preset_id: &str) -> Result<()> {
        let meta = self.plugin(key).context("plugin isn't loaded")?;
        match meta.format {
            PluginFormat::Lv2 => self.hosts()?.lv2.delete_preset(preset_id),
            PluginFormat::Vst3 => self.hosts()?.vst3.delete_preset(preset_id),
            PluginFormat::Clap => self.hosts()?.clap.delete_preset(&meta.id, preset_id),
        }
    }

    /// Instantiates the plugin with [`PluginInfo::id`] `plugin_id` at the running
    /// stream's sample rate.
    fn instantiate(&mut self, plugin_id: &str) -> Result<(BoxedPlugin, LoadedPluginMeta)> {
        let sample_rate = self
            .running
            .as_ref()
            .context("audio stream isn't running")?
            .sample_rate;
        let key = self.next_key;
        self.next_key += 1;

        if plugin_id.starts_with(live_keys_vst3_host::ID_PREFIX) {
            let (plugin, controller) = self
                .hosts()?
                .vst3
                .instantiate(plugin_id, sample_rate)
                .with_context(|| format!("failed to instantiate VST3 plugin '{plugin_id}'"))?;
            let meta = LoadedPluginMeta {
                key,
                id: plugin_id.to_string(),
                name: live_keys_core::HostedPlugin::name(&plugin).to_string(),
                format: PluginFormat::Vst3,
                params: controller.params().to_vec(),
                editor: PluginEditor::Vst3(controller),
                values: HashMap::new(),
            };
            Ok((Box::new(plugin), meta))
        } else if plugin_id.starts_with(live_keys_clap_host::ID_PREFIX) {
            let (plugin, controller) = self
                .hosts()?
                .clap
                .instantiate(plugin_id, sample_rate)
                .with_context(|| format!("failed to instantiate CLAP plugin '{plugin_id}'"))?;
            // A CLAP plugin can be asked for its values, and its defaults aren't
            // always right (Surge XT reports 0 for every one).
            let values = controller
                .params()
                .iter()
                .filter_map(|param| Some((param.id, controller.param_value(param.id)?)))
                .collect();
            let meta = LoadedPluginMeta {
                key,
                id: plugin_id.to_string(),
                name: live_keys_core::HostedPlugin::name(&plugin).to_string(),
                format: PluginFormat::Clap,
                params: controller.params().to_vec(),
                editor: PluginEditor::Clap(controller),
                values,
            };
            Ok((Box::new(plugin), meta))
        } else {
            let mut plugin = self
                .hosts()?
                .lv2
                .instantiate(plugin_id, sample_rate)
                .with_context(|| format!("failed to instantiate LV2 plugin '{plugin_id}'"))?;
            let ui_channel = plugin.take_ui_channel();
            let meta = LoadedPluginMeta {
                key,
                id: plugin_id.to_string(),
                name: live_keys_core::HostedPlugin::name(&plugin).to_string(),
                format: PluginFormat::Lv2,
                params: plugin.params().to_vec(),
                editor: PluginEditor::Lv2 {
                    plugin: plugin.plugin().clone(),
                    features: plugin.features().clone(),
                    raw_handle: plugin.raw_handle(),
                    instance: plugin.instance_ref(),
                    ui_channel,
                    gui_leases: plugin.gui_leases(),
                },
                values: HashMap::new(),
            };
            Ok((Box::new(plugin), meta))
        }
    }

    /// Instantiates `plugin_id` as `slot`'s instrument, replacing any previous one;
    /// the slot's effects stay. The stream must already be running (see
    /// [`Engine::start`]), since plugins are instantiated at its sample rate.
    pub fn load_instrument(&mut self, slot: usize, plugin_id: &str) -> Result<PluginKey> {
        anyhow::ensure!(slot < SLOT_COUNT, "slot {slot} out of range");
        let (plugin, meta) = self.instantiate(plugin_id)?;
        self.push_structural(RackCommand::SetInstrument {
            slot,
            plugin: Some(plugin),
        })?;
        log::info!("Loaded {} instrument into slot {slot}: {}", meta.format.label(), meta.name);
        let key = meta.key;
        self.slots[slot].instrument = Some(meta);
        Ok(key)
    }

    /// Removes `slot`'s instrument, if any; its effects stay.
    pub fn unload_instrument(&mut self, slot: usize) {
        if self.slots.get(slot).is_none_or(|chain| chain.instrument.is_none()) {
            return;
        }
        // Without a running stream there's no audio-side chain to keep in step.
        if self.running.is_some()
            && let Err(err) = self.push_structural(RackCommand::SetInstrument { slot, plugin: None })
        {
            log::warn!("failed to unload slot {slot}'s instrument: {err:#}");
            return;
        }
        self.slots[slot].instrument = None;
    }

    /// Instantiates `plugin_id` and appends it to the end of `slot`'s `section`
    /// chain.
    pub fn add_effect(&mut self, slot: usize, section: Section, plugin_id: &str) -> Result<PluginKey> {
        anyhow::ensure!(slot < SLOT_COUNT, "slot {slot} out of range");
        let index = self.slots[slot].effects(section).len();
        let capacity = section.capacity();
        anyhow::ensure!(index < capacity, "slot already has {capacity} effects there");
        let (plugin, meta) = self.instantiate(plugin_id)?;
        self.push_structural(RackCommand::InsertEffect {
            slot,
            section,
            index,
            plugin,
        })?;
        log::info!(
            "Added {} {section:?} effect to slot {slot}: {}",
            meta.format.label(),
            meta.name
        );
        let key = meta.key;
        self.slots[slot].effects_mut(section).push(meta);
        Ok(key)
    }

    /// Removes effect `index` from `slot`'s `section` chain; later ones move up.
    pub fn remove_effect(&mut self, slot: usize, section: Section, index: usize) {
        if self.slots.get(slot).is_none_or(|chain| index >= chain.effects(section).len()) {
            return;
        }
        if self.running.is_some()
            && let Err(err) =
                self.push_structural(RackCommand::RemoveEffect { slot, section, index })
        {
            log::warn!("failed to remove effect {index} from slot {slot}: {err:#}");
            return;
        }
        self.slots[slot].effects_mut(section).remove(index);
    }

    /// Moves effect `from` in `slot`'s `section` chain to position `to`, shifting
    /// the ones in between.
    pub fn move_effect(&mut self, slot: usize, section: Section, from: usize, to: usize) {
        let Some(chain) = self.slots.get(slot) else {
            return;
        };
        let len = chain.effects(section).len();
        if from == to || from >= len || to >= len {
            return;
        }
        if self.running.is_some()
            && let Err(err) = self.push_structural(RackCommand::MoveEffect {
                slot,
                section,
                from,
                to,
            })
        {
            log::warn!("failed to move effect in slot {slot}: {err:#}");
            return;
        }
        let effects = self.slots[slot].effects_mut(section);
        let meta = effects.remove(from);
        effects.insert(to, meta);
    }

    /// (Re)starts the output stream on `audio_filter` (or the default output device),
    /// driven by the MIDI inputs (see [`Engine::set_midi_inputs`]). Any plugins already
    /// loaded are re-instantiated (at default parameter values, with new keys)
    /// against the new stream, since its sample rate may differ; one that fails to
    /// reload is left out and its error logged.
    pub fn start(&mut self, audio_filter: Option<&str>) -> Result<()> {
        let output_device = find_output_device(&self.audio_host, audio_filter)?;
        let output_config = output_device.default_output_config()?;
        if output_config.sample_format() != cpal::SampleFormat::F32 {
            anyhow::bail!(
                "output device uses sample format {:?}, only F32 is supported right now",
                output_config.sample_format()
            );
        }
        let sample_rate = f64::from(output_config.sample_rate());
        let channels = output_config.channels() as usize;
        log::info!("Output device: {output_device} ({channels} ch @ {sample_rate} Hz)");

        let (midi_tx, midi_rx) = ring_buffer::<RoutedMidi>(MIDI_RING_CAPACITY);
        let peaks = SlotPeaks::default();
        let (command_tx, command_rx) = ring_buffer::<RackCommand>(COMMAND_RING_CAPACITY);
        let (retired_tx, retired_rx) = ring_buffer::<BoxedPlugin>(RETIRED_RING_CAPACITY);
        let (lent_tx, lent_rx) = ring_buffer::<BoxedPlugin>(LENT_RING_CAPACITY);

        let rack = Rack::new(
            &self.settings,
            retired_tx,
            lent_tx,
            channels,
            self.max_block_size,
            MIDI_RING_CAPACITY,
            peaks.clone(),
        );
        let state = Box::new(AudioState {
            rack,
            command_rx,
            midi_rx,
        });
        let (stream, handoff_rx) = build_stream(&output_device, output_config.into(), state)
            .map_err(|(err, _state)| err)?;

        // Tear down any previously running stream/connection asynchronously — see
        // `drop_stream_async`'s docs for why this must never block here.
        self.drop_stream_async();
        if let Ok(mut shared) = self.midi_inputs.tx.lock() {
            *shared = Some(midi_tx);
        }
        self.running = Some(RunningStream {
            stream,
            handoff_rx,
            sample_rate,
            command_tx,
            peaks,
            retired_rx,
            lent_rx,
            overdue_lend: None,
        });

        let previous_slots = std::mem::take(&mut self.slots);
        for (slot, chain) in previous_slots.into_iter().enumerate() {
            for meta in chain.midi_effects {
                if let Err(err) = self.add_effect(slot, Section::Midi, &meta.id) {
                    log::error!("failed to reload slot {slot}'s effect ('{}'): {err:#}", meta.id);
                }
            }
            if let Some(meta) = chain.instrument
                && let Err(err) = self.load_instrument(slot, &meta.id)
            {
                log::error!("failed to reload slot {slot}'s instrument ('{}'): {err:#}", meta.id);
            }
            for meta in chain.audio_effects {
                if let Err(err) = self.add_effect(slot, Section::Audio, &meta.id) {
                    log::error!("failed to reload slot {slot}'s effect ('{}'): {err:#}", meta.id);
                }
            }
        }
        Ok(())
    }

    /// Moves the running rack to output `audio_filter` (or the default output
    /// device) without reloading any plugin: the device is opened at the
    /// current sample rate, which the plugins were set up for, and the old
    /// stream's callback hands its rack — plugins, queues and all — to the new
    /// one. `Ok(false)` if the device can't run at that rate: nothing was
    /// changed, and [`Engine::start`] (which reloads every plugin) is the way
    /// to switch. An error means the hand-over failed midway; the plugins can't
    /// be reached any more, and [`Engine::start`] must follow.
    pub fn switch_output(&mut self, audio_filter: Option<&str>) -> Result<bool> {
        let running = self.running.as_mut().context("audio stream isn't running")?;
        let output_device = find_output_device(&self.audio_host, audio_filter)?;
        let sample_rate = running.sample_rate as cpal::SampleRate;
        let Some(output_config) = output_config_at(&output_device, sample_rate)? else {
            return Ok(false);
        };
        log::info!(
            "Output device: {output_device} ({} ch @ {sample_rate} Hz), plugins kept",
            output_config.channels()
        );

        if running.command_tx.push(RackCommand::Detach).is_err() {
            bail!("the audio command queue is full");
        }
        let deadline = Instant::now() + HANDOFF_TIMEOUT;
        let mut state = loop {
            if let Ok(state) = running.handoff_rx.pop() {
                break state;
            }
            if Instant::now() >= deadline {
                bail!("the old output stream didn't hand its plugins over");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        state.rack.set_channels(output_config.channels() as usize);
        let (stream, handoff_rx) = match build_stream(&output_device, output_config.into(), state) {
            Ok(built) => built,
            Err((err, state)) => {
                self.orphaned = state;
                return Err(err);
            }
        };
        let old = std::mem::replace(&mut running.stream, stream);
        running.handoff_rx = handoff_rx;
        // As in `drop_stream_async`: the old stream's teardown may block.
        std::thread::spawn(move || drop(old));
        Ok(true)
    }
}

/// `device`'s F32 output config at `sample_rate`, preferring its default
/// channel count; `None` if it can't run at that rate.
fn output_config_at(
    device: &cpal::Device,
    sample_rate: cpal::SampleRate,
) -> Result<Option<cpal::SupportedStreamConfig>> {
    let default_channels = device.default_output_config()?.channels();
    let mut configs: Vec<cpal::SupportedStreamConfig> = device
        .supported_output_configs()?
        .filter(|range| range.sample_format() == cpal::SampleFormat::F32)
        .filter_map(|range| range.try_with_sample_rate(sample_rate))
        .collect();
    configs.sort_by_key(|config| config.channels() != default_channels);
    Ok(configs.into_iter().next())
}

/// A started output stream, and where its callback hands its state over.
type BuiltStream = (cpal::Stream, Consumer<Box<AudioState>>);

/// Starts an output stream on `device` whose callback owns `state`: it applies
/// queued commands and renders the rack, until [`RackCommand::Detach`] makes it
/// hand `state` over on the returned queue and play silence from then on. On
/// failure, `state` comes back with the error — unless the callback had already
/// taken it, in which case it was dropped with the stream, on this thread.
fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    state: Box<AudioState>,
) -> Result<BuiltStream, (anyhow::Error, Option<Box<AudioState>>)> {
    let (mut handoff_tx, handoff_rx) = ring_buffer::<Box<AudioState>>(1);
    // The callback gets the state through this slot, so that a failed build can
    // give it back (the closure, and anything moved into it, is dropped then).
    let slot = Arc::new(std::sync::Mutex::new(Some(state)));
    let mut current: Option<Box<AudioState>> = None;
    let callback_slot = slot.clone();
    let result = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if current.is_none() {
                // Until the state is taken (the first callback, normally): a
                // `try_lock`, which never waits, on a lock nobody else holds
                // once the stream runs.
                if let Ok(mut slot) = callback_slot.try_lock() {
                    current = slot.take();
                }
            }
            let Some(state) = current.as_mut() else {
                data.fill(0.0);
                return;
            };
            let mut detach = false;
            while let Ok(command) = state.command_rx.pop() {
                if let RackCommand::Detach = command {
                    detach = true;
                    break;
                }
                state.rack.apply(command);
            }
            if detach {
                data.fill(0.0);
                if let Some(state) = current.take()
                    && let Err(PushError::Full(state)) = handoff_tx.push(state)
                {
                    // Can't happen (one detach per stream); leaking beats dropping
                    // plugins here.
                    std::mem::forget(state);
                }
                return;
            }
            state.rack.render(&mut state.midi_rx, data);
        },
        |err| log::error!("audio stream error: {err}"),
        None,
    );
    let taken_back = |slot: &std::sync::Mutex<Option<Box<AudioState>>>| slot.lock().ok().and_then(|mut slot| slot.take());
    let stream = match result {
        Ok(stream) => stream,
        Err(err) => return Err((err.into(), taken_back(&slot))),
    };
    if let Err(err) = stream.play() {
        drop(stream);
        return Err((err.into(), taken_back(&slot)));
    }
    Ok((stream, handoff_rx))
}

