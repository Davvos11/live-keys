use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use live_keys_core::{
    Consumer, MidiEvent, MidiFilter, ParamChange, ParamInfo, ParamKind, PluginFormat, PluginInfo,
    PluginKind, Producer, midi_ring_buffer, ring_buffer,
};
use live_keys_lv2_host::{Lv2Host, livi, lv2_raw};
use live_keys_vst3_host::{Vst3Controller, Vst3Host};

use std::path::PathBuf;
use std::sync::Arc;

use crate::audio::find_output_device;
use crate::midi::find_midi_port;
use crate::paths::build_lv2_path;
use crate::rack::{
    BoxedPlugin, ChainPosition, MAX_EFFECTS, MAX_MIDI_EFFECTS, Rack, RackCommand, Section,
};

/// Number of plugin slots in the rack. Every slot sees the same MIDI input (each
/// through its own [`MidiFilter`]), runs it through its chain (up to
/// [`MAX_MIDI_EFFECTS`] MIDI effects, an instrument, then up to [`MAX_EFFECTS`]
/// audio effects), and the slots' outputs are summed, each at its own gain, into
/// the one output stream.
pub const SLOT_COUNT: usize = 3;

const MIDI_RING_CAPACITY: usize = 1024;
const COMMAND_RING_CAPACITY: usize = 256;
/// Plugins taken out of a chain, waiting to be dropped off the audio thread.
/// Drained on every structural change, so it only needs room for a few in flight;
/// sized so even clearing every chain at once fits.
const RETIRED_RING_CAPACITY: usize = SLOT_COUNT * (MAX_MIDI_EFFECTS + 1 + MAX_EFFECTS) * 2;

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

    fn position_of(&self, key: PluginKey) -> Option<ChainPosition> {
        if self.instrument.as_ref().is_some_and(|meta| meta.key == key) {
            return Some(ChainPosition::Instrument);
        }
        let find = |effects: &[LoadedPluginMeta]| effects.iter().position(|meta| meta.key == key);
        find(&self.midi_effects)
            .map(ChainPosition::MidiEffect)
            .or_else(|| find(&self.audio_effects).map(ChainPosition::Effect))
    }
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

/// Owns the plugin hosts, the running audio/MIDI stream (if any), and the chain of
/// plugins loaded into each rack slot. Real-time wiring (ring buffers, no
/// allocation in the audio callback — see `rack.rs`) lives here so every host
/// (CLI, GUI) shares one implementation instead of drifting apart.
pub struct Engine {
    audio_host: cpal::Host,
    lv2: Lv2Host,
    vst3: Vst3Host,
    max_block_size: usize,
    running: Option<RunningStream>,
    slots: [SlotChain; SLOT_COUNT],
    settings: [SlotSettings; SLOT_COUNT],
    next_key: PluginKey,
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
            next_key: 0,
        }
    }

    /// Every plugin found by the last scan, LV2 first, then VST3.
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        let mut plugins = self.lv2.list_plugins();
        plugins.extend(self.vst3.list_plugins());
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

    /// Rebuilds the plugin world so it also searches `extra_dirs`, in addition to the
    /// default search paths (see [`crate::default_plugin_search_paths`]). Stops the
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

    /// Drops plugins the audio callback has taken out of a chain. They're handed
    /// back here rather than dropped on the audio thread, since a plugin's teardown
    /// can deallocate or block. Called on every structural change; hosts may also
    /// call it periodically so a removed plugin doesn't linger until the next one.
    pub fn collect_retired(&mut self) {
        if let Some(running) = &mut self.running {
            while let Ok(plugin) = running.retired_rx.pop() {
                drop(plugin);
            }
        }
    }

    /// Forwards a parameter change that came from a plugin's own GUI to the plugin
    /// with `key`, applied on the next audio block. A no-op if that plugin is gone
    /// or nothing is running.
    pub fn send_param_change(&mut self, key: PluginKey, param_id: u32, value: f32) {
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
    /// port values itself.)
    pub fn set_param(&mut self, key: PluginKey, param_id: u32, value: f32) {
        if let Some(LoadedPluginMeta {
            editor: PluginEditor::Vst3(controller),
            ..
        }) = self.plugin(key)
        {
            controller.set_param_normalized(param_id, value);
        }
        self.send_param_change(key, param_id, value);
    }

    /// Parameter `param_id` of the plugin with `key` at `value`, as text for display
    /// ("-6.0 dB", "Saw", "C#3"). A VST3 plugin formats its own values; otherwise
    /// it's formatted from the parameter's metadata. Empty if there's no such
    /// parameter.
    pub fn param_text(&self, key: PluginKey, param_id: u32, value: f32) -> String {
        let Some(meta) = self.plugin(key) else {
            return String::new();
        };
        let Some(info) = meta.params.iter().find(|param| param.id == param_id) else {
            return String::new();
        };
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
            };
            Ok((Box::new(plugin), meta))
        } else {
            let plugin = self
                .lv2
                .instantiate(plugin_id, sample_rate)
                .with_context(|| format!("failed to instantiate LV2 plugin '{plugin_id}'"))?;
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
                },
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
    /// driven by `midi_filter` (or the first MIDI input port). Any plugins already
    /// loaded are re-instantiated (at default parameter values, with new keys)
    /// against the new stream, since its sample rate may differ; one that fails to
    /// reload is left out and its error logged.
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
        let (retired_tx, retired_rx) = ring_buffer::<BoxedPlugin>(RETIRED_RING_CAPACITY);

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

        let mut rack = Rack::new(
            &self.settings,
            retired_tx,
            channels,
            self.max_block_size,
            MIDI_RING_CAPACITY,
        );
        let stream_config: cpal::StreamConfig = output_config.into();
        let stream = output_device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                while let Ok(command) = command_rx.pop() {
                    rack.apply(command);
                }
                rack.render(&mut midi_rx, data);
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
}
