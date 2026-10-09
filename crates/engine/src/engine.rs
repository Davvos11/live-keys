use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use live_keys_core::{
    Consumer, HostedPlugin, MidiEvent, MidiFilter, MidiRouter, ParamChange, ParamInfo,
    PluginFormat, PluginInfo, Producer, PushError, midi_ring_buffer, ring_buffer,
};
use live_keys_lv2_host::{Lv2Host, livi, lv2_raw};
use live_keys_vst3_host::{Vst3Controller, Vst3Host};

use std::path::PathBuf;
use std::sync::Arc;

use crate::audio::find_output_device;
use crate::midi::find_midi_port;
use crate::paths::build_lv2_path;

/// Number of plugin slots in the rack. Every slot sees the same MIDI input (each
/// through its own [`MidiFilter`]), and their outputs are summed, each at its own
/// gain, into the one output stream.
pub const SLOT_COUNT: usize = 3;

const MIDI_RING_CAPACITY: usize = 1024;
const COMMAND_RING_CAPACITY: usize = 256;
/// Plugins swapped out of the rack waiting to be dropped off the audio thread.
/// Drained on every load/unload, so it only needs room for a few in flight.
const RETIRED_RING_CAPACITY: usize = SLOT_COUNT * 4;

type BoxedPlugin = Box<dyn HostedPlugin>;

/// A request from the control thread to the audio callback. Plugin swaps and
/// param changes share one queue so they're applied in the order they were sent —
/// a param change queued for a slot's old plugin can never land on its replacement.
enum RackCommand {
    Plugin { slot: usize, plugin: Option<BoxedPlugin> },
    Param { slot: usize, change: ParamChange },
    Filter { slot: usize, filter: MidiFilter },
    Gain { slot: usize, gain: f32 },
}

/// Per-slot mixer/routing settings. They belong to the slot rather than to the
/// plugin in it, so they survive loading a different plugin and restarting the
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

/// A slot as the audio callback sees it. Everything here is allocated before the
/// stream starts.
struct SlotRuntime {
    plugin: Option<BoxedPlugin>,
    router: MidiRouter,
    /// This callback's MIDI after `router`, rebuilt every callback.
    midi: Vec<MidiEvent>,
    /// Gain applied at the end of the last rendered block; each block ramps from
    /// here to `target_gain` so fader moves don't click.
    gain: f32,
    target_gain: f32,
}

/// What it takes to open a loaded plugin's own GUI, per format.
pub enum PluginEditor {
    Lv2 {
        plugin: livi::Plugin,
        features: Arc<livi::Features>,
        raw_handle: lv2_raw::core::LV2Handle,
    },
    /// The plugin's main-thread half. Its editor is created from it.
    Vst3(Vst3Controller),
}

/// The main-thread view of the plugin loaded into a slot, captured at load time
/// since the running plugin itself gets moved into the audio callback and isn't
/// reachable from `Engine` afterwards.
pub struct LoadedPluginMeta {
    /// [`PluginInfo::id`] of the plugin.
    pub id: String,
    pub name: String,
    pub format: PluginFormat,
    /// Parameter metadata, for a parameter-editor UI (macro knobs, a fallback UI
    /// for GUI-less plugins, future MIDI binding).
    pub params: Vec<ParamInfo>,
    pub editor: PluginEditor,
}

/// The running output stream + MIDI connection, and the control-thread ends of the
/// queues into/out of its audio callback.
struct RunningStream {
    _stream: cpal::Stream,
    _midi_connection: midir::MidiInputConnection<()>,
    sample_rate: f64,
    command_tx: Producer<RackCommand>,
    retired_rx: Consumer<BoxedPlugin>,
}

/// Owns the LV2 plugin world, the running audio/MIDI stream (if any), and the
/// plugin loaded into each rack slot. Real-time wiring (ring buffers, no allocation
/// in the audio callback) lives here so every host (CLI, GUI) shares one
/// implementation instead of drifting apart.
pub struct Engine {
    audio_host: cpal::Host,
    lv2: Lv2Host,
    vst3: Vst3Host,
    max_block_size: usize,
    running: Option<RunningStream>,
    slots: [Option<LoadedPluginMeta>; SLOT_COUNT],
    settings: [SlotSettings; SLOT_COUNT],
}

impl Engine {
    /// `max_block_size` must be at least as large as the largest number of frames the
    /// audio backend will ever hand us in one callback.
    pub fn new(max_block_size: usize) -> Self {
        Self {
            audio_host: cpal::default_host(),
            lv2: Lv2Host::new(max_block_size),
            vst3: Vst3Host::new(max_block_size, &[]),
            max_block_size,
            running: None,
            slots: Default::default(),
            settings: Default::default(),
        }
    }

    /// Every plugin found by the last scan, LV2 first, then VST3.
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        let mut plugins = self.lv2.list_plugins();
        plugins.extend(self.vst3.list_plugins());
        plugins
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The plugin loaded into `slot`, if any. Out-of-range slots read as empty.
    pub fn slot(&self, slot: usize) -> Option<&LoadedPluginMeta> {
        self.slots.get(slot)?.as_ref()
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

    /// Sends `command` to the audio callback, if running. Dropped (and logged) if the
    /// queue is full — that means the audio thread isn't draining it, which is worth
    /// knowing about even though there's little to do but drop.
    fn push_command(&mut self, command: RackCommand) {
        if let Some(running) = &mut self.running
            && running.command_tx.push(command).is_err()
        {
            log::warn!("dropped rack command, queue full");
        }
    }

    /// Rebuilds the plugin world so it also searches `extra_dirs`, in addition to the
    /// default LV2 search paths (see [`crate::default_plugin_search_paths`]). Stops the
    /// stream and empties every slot first — rescanning would otherwise leave loaded
    /// plugins depending on a `World` that's about to be dropped.
    pub fn rescan_plugins(&mut self, extra_dirs: &[PathBuf]) {
        self.stop();
        // SAFETY: only ever called from the control thread; nothing else in this
        // process reads or writes environment variables concurrently.
        unsafe { std::env::set_var("LV2_PATH", build_lv2_path(extra_dirs)) };
        self.lv2 = Lv2Host::new(self.max_block_size);
        self.vst3 = Vst3Host::new(self.max_block_size, extra_dirs);
    }

    /// Stops the running stream and MIDI connection, if any, and empties every slot.
    pub fn stop(&mut self) {
        self.drop_stream_async();
        self.slots = Default::default();
    }

    /// Moves any currently running stream/MIDI connection to a throwaway thread for
    /// teardown, rather than dropping them in place. `cpal`'s ALSA `Stream::drop()`
    /// joins its own internal audio thread; if that thread is stuck for any reason
    /// (observed after a previous session's plugin UI wedged internally — see
    /// `live-keys-lv2-ui`'s docs), dropping in place would block the caller —
    /// typically the GUI thread, e.g. when switching devices — waiting on it
    /// forever. A stuck old thread is abandoned (leaked until the app exits) rather
    /// than allowed to freeze anything new.
    fn drop_stream_async(&mut self) {
        if let Some(running) = self.running.take() {
            std::thread::spawn(move || drop(running));
        }
    }

    /// Drops plugins the audio callback has swapped out of the rack. They're handed
    /// back here rather than dropped on the audio thread, since a plugin's teardown
    /// can deallocate or block. Called on every load/unload; hosts may also call it
    /// periodically so an unloaded plugin doesn't linger until the next one.
    pub fn collect_retired(&mut self) {
        if let Some(running) = &mut self.running {
            while let Ok(plugin) = running.retired_rx.pop() {
                drop(plugin);
            }
        }
    }

    /// Forwards a parameter change that came from the plugin's own GUI to the
    /// plugin in `slot`, applied on the next audio block. A no-op if nothing is
    /// running.
    pub fn send_param_change(&mut self, slot: usize, param_id: u32, value: f32) {
        self.push_command(RackCommand::Param {
            slot,
            change: ParamChange { param_id, value },
        });
    }

    /// Sets a parameter of the plugin in `slot` from the host side (e.g. a
    /// parameter slider): like [`Engine::send_param_change`], but a VST3 plugin's
    /// controller is told too, so its own GUI follows. (An LV2 plugin's GUI reads
    /// port values itself.)
    pub fn set_param(&mut self, slot: usize, param_id: u32, value: f32) {
        if let Some(LoadedPluginMeta {
            editor: PluginEditor::Vst3(controller),
            ..
        }) = self.slot(slot)
        {
            controller.set_param_normalized(param_id, value);
        }
        self.send_param_change(slot, param_id, value);
    }

    /// Instantiates the plugin with [`PluginInfo::id`] `plugin_id` into `slot`,
    /// replacing whatever was there. The stream must already be running (see [`Engine::start`]), since
    /// plugins are instantiated at its sample rate.
    pub fn load_into_slot(&mut self, slot: usize, plugin_id: &str) -> Result<()> {
        anyhow::ensure!(slot < SLOT_COUNT, "slot {slot} out of range");
        self.collect_retired();
        let running = self
            .running
            .as_mut()
            .context("audio stream isn't running")?;

        let (plugin, meta): (BoxedPlugin, _) =
            if plugin_id.starts_with(live_keys_vst3_host::ID_PREFIX) {
                let (plugin, controller) = self
                    .vst3
                    .instantiate(plugin_id, running.sample_rate)
                    .with_context(|| format!("failed to instantiate VST3 plugin '{plugin_id}'"))?;
                let meta = LoadedPluginMeta {
                    id: plugin_id.to_string(),
                    name: plugin.name().to_string(),
                    format: PluginFormat::Vst3,
                    params: controller.params().to_vec(),
                    editor: PluginEditor::Vst3(controller),
                };
                (Box::new(plugin), meta)
            } else {
                let plugin = self
                    .lv2
                    .instantiate(plugin_id, running.sample_rate)
                    .with_context(|| format!("failed to instantiate LV2 plugin '{plugin_id}'"))?;
                let meta = LoadedPluginMeta {
                    id: plugin_id.to_string(),
                    name: plugin.name().to_string(),
                    format: PluginFormat::Lv2,
                    params: plugin.params(),
                    editor: PluginEditor::Lv2 {
                        plugin: plugin.plugin().clone(),
                        features: plugin.features().clone(),
                        raw_handle: plugin.raw_handle(),
                    },
                };
                (Box::new(plugin), meta)
            };
        log::info!("Loaded {} plugin into slot {slot}: {}", meta.format.label(), meta.name);

        if running
            .command_tx
            .push(RackCommand::Plugin {
                slot,
                plugin: Some(plugin),
            })
            .is_err()
        {
            anyhow::bail!("rack command queue full, audio thread isn't draining it");
        }
        self.slots[slot] = Some(meta);
        Ok(())
    }

    /// Removes the plugin from `slot`, if any.
    pub fn unload_slot(&mut self, slot: usize) {
        if slot >= SLOT_COUNT || self.slots[slot].is_none() {
            return;
        }
        self.collect_retired();
        if let Some(running) = &mut self.running
            && running
                .command_tx
                .push(RackCommand::Plugin { slot, plugin: None })
                .is_err()
        {
            log::warn!("failed to unload slot {slot}, rack command queue full");
            return;
        }
        self.slots[slot] = None;
    }

    /// (Re)starts the output stream on `audio_filter` (or the default output device),
    /// driven by `midi_filter` (or the first MIDI input port). Any plugins already
    /// loaded into slots are re-instantiated (at default parameter values) against
    /// the new stream, since its sample rate may differ; a slot whose plugin fails to
    /// reload is left empty and its error logged.
    pub fn start(&mut self, audio_filter: Option<&str>, midi_filter: Option<&str>) -> Result<()> {
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

        let (mut midi_tx, mut midi_rx) = midi_ring_buffer(MIDI_RING_CAPACITY);
        let (command_tx, mut command_rx) = ring_buffer::<RackCommand>(COMMAND_RING_CAPACITY);
        let (mut retired_tx, retired_rx) = ring_buffer::<BoxedPlugin>(RETIRED_RING_CAPACITY);

        let midi_input = midir::MidiInput::new("live-keys")?;
        let midi_ports = midi_input.ports();
        let midi_port = find_midi_port(&midi_input, &midi_ports, midi_filter)?;
        log::info!("MIDI input: {}", midi_input.port_name(midi_port)?);
        let midi_connection = midi_input
            .connect(
                midi_port,
                "live-keys-input",
                move |_stamp, bytes, _| {
                    // No sample-accurate timestamp is available here, so events are
                    // applied at the start of whichever audio block consumes them.
                    if let Some(event) = MidiEvent::from_raw(0, bytes)
                        && let Err(err) = midi_tx.push(event)
                    {
                        // This is midir's own callback thread, not the audio thread,
                        // so logging here is safe. A full queue means the audio
                        // thread isn't draining it — dropped notes are worth knowing
                        // about even though there's little to do but drop.
                        log::warn!("dropped MIDI event, queue full: {err}");
                    }
                },
                (),
            )
            .map_err(|err| anyhow::anyhow!("failed to connect to MIDI input: {err}"))?;

        let mut midi_batch: Vec<MidiEvent> = Vec::with_capacity(MIDI_RING_CAPACITY);
        let mut rack: [SlotRuntime; SLOT_COUNT] = std::array::from_fn(|slot| SlotRuntime {
            plugin: None,
            router: MidiRouter::new(self.settings[slot].filter),
            midi: Vec::with_capacity(MIDI_RING_CAPACITY),
            gain: self.settings[slot].gain,
            target_gain: self.settings[slot].gain,
        });
        let max_block_size = self.max_block_size;

        let stream_config: cpal::StreamConfig = output_config.into();
        let stream = output_device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                while let Ok(command) = command_rx.pop() {
                    match command {
                        RackCommand::Plugin { slot, plugin } => {
                            let slot = &mut rack[slot];
                            slot.router.clear_held();
                            let old = std::mem::replace(&mut slot.plugin, plugin);
                            // Handed back to the control thread to drop. The queue
                            // being full means the control thread hasn't collected
                            // in a long while; leaking the plugin then beats
                            // running its teardown here.
                            if let Some(old) = old
                                && let Err(PushError::Full(old)) = retired_tx.push(old)
                            {
                                log::warn!("retired plugin queue full, leaking a plugin");
                                std::mem::forget(old);
                            }
                        }
                        RackCommand::Param { slot, change } => {
                            if let Some(plugin) = &mut rack[slot].plugin {
                                plugin.set_param(change.param_id, change.value);
                            }
                        }
                        RackCommand::Filter { slot, filter } => {
                            rack[slot].router.set_filter(filter);
                        }
                        RackCommand::Gain { slot, gain } => {
                            rack[slot].target_gain = gain;
                        }
                    }
                }

                midi_batch.clear();
                while let Ok(event) = midi_rx.pop() {
                    midi_batch.push(event);
                }
                for slot in rack.iter_mut().filter(|slot| slot.plugin.is_some()) {
                    slot.midi.clear();
                    for event in &midi_batch {
                        if slot.router.accept(event) {
                            slot.midi.push(*event);
                        }
                    }
                }

                // The backend can hand back a buffer spanning more frames than
                // `max_block_size` in one callback (observed with a Bluetooth output
                // device, whose negotiated period can reach/exceed it) — render it as
                // multiple `process` calls instead of silently leaving the tail of
                // `data` unwritten. Only the first sub-block gets this callback's
                // freshly drained MIDI; later sub-blocks get none, since no further
                // real time has passed for them (see "MIDI events always get
                // frame_offset = 0" in CLAUDE.md).
                for (chunk_idx, data_chunk) in
                    data.chunks_mut(channels * max_block_size).enumerate()
                {
                    let frames = data_chunk.len() / channels;
                    data_chunk.fill(0.0);
                    for slot in &mut rack {
                        let Some(plugin) = &mut slot.plugin else {
                            continue;
                        };
                        let midi: &[MidiEvent] = if chunk_idx == 0 { &slot.midi } else { &[] };
                        plugin.process(midi, frames);
                        let plugin_channels = plugin.audio_output_count().max(1);
                        let gain_step = (slot.target_gain - slot.gain) / frames as f32;
                        for ch in 0..channels {
                            let rendered = plugin.audio_output(ch % plugin_channels);
                            for (frame_idx, (frame, sample)) in
                                data_chunk.chunks_mut(channels).zip(rendered).enumerate()
                            {
                                let gain = slot.gain + gain_step * frame_idx as f32;
                                frame[ch] += sample * gain;
                            }
                        }
                        slot.gain = slot.target_gain;
                    }
                }
            },
            |err| log::error!("audio stream error: {err}"),
            None,
        )?;
        stream.play()?;

        // Tear down any previously running stream/connection asynchronously — see
        // `drop_stream_async`'s docs for why this must never block here.
        self.drop_stream_async();
        self.running = Some(RunningStream {
            _stream: stream,
            _midi_connection: midi_connection,
            sample_rate,
            command_tx,
            retired_rx,
        });

        let previous_slots = std::mem::take(&mut self.slots);
        for (slot, meta) in previous_slots.into_iter().enumerate() {
            if let Some(meta) = meta
                && let Err(err) = self.load_into_slot(slot, &meta.id)
            {
                log::error!("failed to reload slot {slot} ('{}'): {err:#}", meta.id);
            }
        }
        Ok(())
    }
}
