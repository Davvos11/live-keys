mod preset;

use std::cell::OnceCell;
use std::sync::Arc;

use live_keys_core::{
    HostedPlugin, MidiEvent, ParamChoice, ParamInfo, ParamKind, PluginFormat, PluginInfo, PluginKind,
    PresetInfo,
};
use livi::event::LV2AtomSequence;

pub use livi;
pub use lv2_raw;
pub use preset::InstanceRef;
use preset::{PresetWorld, StateFeatures};

/// Capacity, in bytes, of the atom sequence buffer used to hand MIDI events to a
/// plugin each block. Generous enough for a dense chord/arpeggio burst.
const MIDI_EVENT_BUFFER_BYTES: usize = 4096;
/// Upper bound on the size of a single MIDI message written into the atom sequence.
const MAX_MIDI_MESSAGE_BYTES: usize = 32;
/// Most MIDI events read back from a plugin's MIDI output per block.
const MIDI_OUT_CAPACITY: usize = 512;

/// Classifies by class and ports. Atom-sequence ports usually carry MIDI, but not
/// always (a synth's UI-state messages), so a class saying "instrument" wins over
/// ports. Audio → MIDI plugins (onset triggers) count as audio effects: a MIDI
/// chain has no audio to give them.
fn plugin_kind(plugin: &livi::Plugin) -> PluginKind {
    let ports = plugin.port_counts();
    if ports.audio_outputs > 0
        && (plugin.is_instrument() || (ports.audio_inputs == 0 && ports.atom_sequence_inputs > 0))
    {
        PluginKind::Instrument
    } else if ports.audio_inputs == 0
        && ports.audio_outputs == 0
        && (ports.atom_sequence_outputs > 0 || plugin.classes().any(|class| class == "MIDI Plugin"))
    {
        PluginKind::MidiEffect
    } else {
        PluginKind::AudioEffect
    }
}

const LV2_CORE: &str = "http://lv2plug.in/ns/lv2core#";
const PORT_PROPS: &str = "http://lv2plug.in/ns/ext/port-props#";
const UNITS: &str = "http://lv2plug.in/ns/extensions/units#";

/// A `ParamInfo` for every control-input port, read from the plugin's metadata:
/// kind from `lv2:toggled`/`lv2:integer`/`lv2:enumeration` and the
/// `units:midiNote` unit, labels from scale points, the unit's symbol, and
/// `pprops:logarithmic`. Ports marked `pprops:notOnGUI` are left out, and
/// `lv2:sampleRate` ranges are scaled to `sample_rate`.
fn read_params(world: &livi::World, plugin: &livi::Plugin, sample_rate: f64) -> Vec<ParamInfo> {
    let lilv_world = world.raw();
    let uri = |base: &str, name: &str| lilv_world.new_uri(&format!("{base}{name}"));
    let (toggled, integer, enumeration, sample_rate_prop) = (
        uri(LV2_CORE, "toggled"),
        uri(LV2_CORE, "integer"),
        uri(LV2_CORE, "enumeration"),
        uri(LV2_CORE, "sampleRate"),
    );
    let (logarithmic, not_on_gui) = (uri(PORT_PROPS, "logarithmic"), uri(PORT_PROPS, "notOnGUI"));
    let (unit_pred, symbol_pred) = (uri(UNITS, "unit"), uri(UNITS, "symbol"));
    let midi_note = format!("{UNITS}midiNote");

    plugin
        .ports_with_type(livi::PortType::ControlInput)
        .filter_map(|port| {
            let raw = plugin.raw().port_by_index(port.index.0)?;
            if raw.has_property(&not_on_gui) {
                return None;
            }
            // `lv2:sampleRate` bounds and default are fractions of the sample rate.
            let scale = if raw.has_property(&sample_rate_prop) { sample_rate as f32 } else { 1.0 };
            let default_value = port.default_value * scale;
            // A control port with no declared range can't be given a usable
            // slider bound, so fall back to a range around its default.
            let min_value = port.min_value.map_or_else(|| default_value.min(0.0), |v| v * scale);
            let max_value = port.max_value.map_or(
                if default_value > min_value { default_value * 2.0 } else { min_value + 1.0 },
                |v| v * scale,
            );
            let mut info =
                ParamInfo::continuous(port.index.0 as u32, port.name, default_value, min_value, max_value);

            let unit = raw.get(&unit_pred);
            let is_note = unit.as_ref().and_then(|unit| unit.as_uri()) == Some(midi_note.as_str());
            if !is_note && let Some(unit) = &unit {
                // The units spec gives every standard unit a symbol; a plugin's own
                // unit (a blank node) usually has one too.
                info.unit = lilv_world
                    .get(Some(unit), Some(&symbol_pred), None)
                    .and_then(|symbol| symbol.as_str().map(str::to_string))
                    .unwrap_or_default();
            }

            let mut choices: Vec<ParamChoice> = raw
                .scale_points()
                .iter()
                .filter_map(|point| {
                    Some(ParamChoice {
                        value: point.value().as_float()?,
                        label: point.label().as_str()?.to_string(),
                    })
                })
                .collect();
            choices.sort_by(|a, b| a.value.total_cmp(&b.value));
            info.choices = choices;

            let whole_steps = (max_value - min_value).round().max(0.0) as u32;
            if raw.has_property(&toggled) {
                info.kind = ParamKind::Toggle;
            } else if raw.has_property(&enumeration) && !info.choices.is_empty() {
                info.kind = ParamKind::Choice;
            } else if is_note && min_value >= 0.0 {
                // A negative range with the note unit is a transposition (x42's
                // keysplit), where note names would be wrong; it falls through to
                // a stepped number below.
                info.kind = ParamKind::Note;
                info.steps = whole_steps;
            } else if raw.has_property(&integer) || is_note {
                info.kind = ParamKind::Stepped;
                info.steps = whole_steps;
            } else {
                info.logarithmic = raw.has_property(&logarithmic) && min_value > 0.0;
            }
            Some(info)
        })
        .collect()
}

/// Owns the LV2 plugin world and the real-time features (URID map, block size) that
/// every plugin instantiated from it shares.
pub struct Lv2Host {
    world: livi::World,
    features: Arc<livi::Features>,
    max_block_size: usize,
    /// Loaded on first use: it parses every bundle a second time.
    presets: OnceCell<PresetWorld>,
}

impl Lv2Host {
    /// `max_block_size` must be at least as large as the largest number of frames
    /// that will ever be passed to `Lv2Plugin::process` in one call.
    pub fn new(max_block_size: usize) -> Self {
        let world = livi::World::new();
        let features = world.build_features(livi::FeaturesBuilder {
            min_block_length: 1,
            max_block_length: max_block_size,
        });
        Self {
            world,
            features,
            max_block_size,
            presets: OnceCell::new(),
        }
    }

    fn preset_world(&self) -> &PresetWorld {
        self.presets.get_or_init(PresetWorld::new)
    }

    /// Every preset of the plugin at `plugin_uri` — shipped with it, or saved by
    /// the user from any host — sorted by name.
    pub fn presets(&self, plugin_uri: &str) -> Vec<PresetInfo> {
        self.preset_world().presets(plugin_uri)
    }

    /// Restores preset `preset_uri` onto `instance` (of the plugin at
    /// `plugin_uri`) and returns its control port values `(port index, value)`,
    /// which the caller has to apply — this only restores the plugin's own state.
    ///
    /// # Safety
    /// If `instance.has_state()`, the plugin mustn't be running on another
    /// thread meanwhile.
    pub unsafe fn restore_preset(
        &self,
        instance: InstanceRef,
        plugin_uri: &str,
        preset_uri: &str,
    ) -> anyhow::Result<Vec<(u32, f32)>> {
        let features = StateFeatures::new(&self.features);
        unsafe { self.preset_world().restore(&features, instance, plugin_uri, preset_uri) }
    }

    /// Saves `instance`'s current settings as a user preset called `name`
    /// (replacing one of the same name); `value` gives each control port's value
    /// by port index.
    ///
    /// # Safety
    /// As for [`Lv2Host::restore_preset`].
    pub unsafe fn save_preset(
        &self,
        instance: InstanceRef,
        plugin_uri: &str,
        plugin_name: &str,
        name: &str,
        value: &dyn Fn(u32) -> f32,
    ) -> anyhow::Result<PresetInfo> {
        let features = StateFeatures::new(&self.features);
        unsafe {
            self.preset_world()
                .save(&features, instance, plugin_uri, plugin_name, name, value)
        }
    }

    /// Deletes a user preset (see [`PresetInfo::user`]).
    pub fn delete_preset(&self, preset_uri: &str) -> anyhow::Result<()> {
        let features = StateFeatures::new(&self.features);
        self.preset_world().delete(&features, preset_uri)
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.world
            .iter_plugins()
            .map(|p| PluginInfo {
                id: p.uri(),
                name: p.name(),
                format: PluginFormat::Lv2,
                kind: plugin_kind(&p),
            })
            .collect()
    }

    /// Instantiate the plugin at `uri`. Plugins with audio input ports (effects)
    /// get one input buffer per port for the host to fill; CV inputs, which
    /// nothing routes yet, are fed silence. Plugins that write an atom-sequence or CV output port (e.g. an
    /// arpeggiator's MIDI-out, or a synth's combined MIDI/UI-state port, as seen
    /// on Surge XT and the LSP samplers) are also accepted — that output is
    /// wired to a discarded scratch buffer so the plugin runs without error,
    /// even though nothing downstream consumes it yet (no MIDI-out routing or
    /// CV-out routing exists).
    pub fn instantiate(&self, uri: &str, sample_rate: f64) -> anyhow::Result<Lv2Plugin> {
        let plugin = self
            .world
            .plugin_by_uri(uri)
            .ok_or_else(|| anyhow::anyhow!("LV2 plugin not found: {uri}"))?;
        let port_counts = *plugin.port_counts();

        let name = plugin.name();
        let instance = unsafe { plugin.instantiate(self.features.clone(), sample_rate)? };
        let output_buffers = (0..port_counts.audio_outputs)
            .map(|_| vec![0.0f32; self.max_block_size])
            .collect();
        let atom_sequence_out = (0..port_counts.atom_sequence_outputs)
            .map(|_| LV2AtomSequence::new(&self.features, MIDI_EVENT_BUFFER_BYTES))
            .collect();
        let cv_out_buffers = (0..port_counts.cv_outputs)
            .map(|_| vec![0.0f32; self.max_block_size])
            .collect();

        let params = read_params(&self.world, &plugin, sample_rate);
        Ok(Lv2Plugin {
            params,
            name,
            plugin,
            features: self.features.clone(),
            instance,
            midi_in: LV2AtomSequence::new(&self.features, MIDI_EVENT_BUFFER_BYTES),
            midi_urid: self.features.midi_urid(),
            atom_sequence_input_count: port_counts.atom_sequence_inputs,
            output_buffers,
            frames_rendered: 0,
            input_buffers: (0..port_counts.audio_inputs)
                .map(|_| vec![0.0f32; self.max_block_size])
                .collect(),
            cv_silence: vec![0.0f32; self.max_block_size],
            cv_input_count: port_counts.cv_inputs,
            atom_sequence_out,
            midi_out: Vec::with_capacity(MIDI_OUT_CAPACITY),
            cv_out_buffers,
        })
    }
}

/// A running instance of an LV2 instrument plugin.
pub struct Lv2Plugin {
    params: Vec<ParamInfo>,
    name: String,
    plugin: livi::Plugin,
    features: Arc<livi::Features>,
    instance: livi::Instance,
    midi_in: LV2AtomSequence,
    midi_urid: u32,
    /// Number of atom-sequence input ports the plugin actually declares — a
    /// pure effect plugin commonly has zero, unlike an instrument. `midi_in` is
    /// duplicated across all of them (see `process`); LV2 plugins with more
    /// than one atom-sequence input are not a case seen in practice.
    atom_sequence_input_count: usize,
    output_buffers: Vec<Vec<f32>>,
    frames_rendered: usize,
    /// One per audio input port, filled by the host (see
    /// `HostedPlugin::audio_input_mut`).
    input_buffers: Vec<Vec<f32>>,
    /// Always-zero buffer fed to every CV input port — never written to; nothing
    /// routes CV yet.
    cv_silence: Vec<f32>,
    cv_input_count: usize,
    /// Scratch buffer(s) connected to the plugin's atom-sequence output
    /// port(s), if any — written by the plugin each block (e.g. MIDI-out, or a
    /// synth's UI-state messages) and then discarded; nothing downstream
    /// consumes it yet.
    atom_sequence_out: Vec<LV2AtomSequence>,
    /// MIDI events found in `atom_sequence_out` after the last block — a MIDI
    /// effect's output (see `HostedPlugin::midi_output`).
    midi_out: Vec<MidiEvent>,
    /// Scratch buffer(s) connected to the plugin's CV output port(s), if any —
    /// same discard-after-render treatment as `atom_sequence_out`.
    cv_out_buffers: Vec<Vec<f32>>,
}

impl Lv2Plugin {
    /// The plugin's metadata handle (URI, port list, `uis()`, ...) — everything
    /// needed to discover and launch this plugin's own GUI without needing a
    /// reference back to the [`Lv2Host`] it came from.
    pub fn plugin(&self) -> &livi::Plugin {
        &self.plugin
    }

    /// The real-time features (URID map, block size) this instance was created
    /// with. A plugin's own GUI must be instantiated with the *same* URID map, or
    /// port/atom types won't match between the DSP instance and its UI.
    pub fn features(&self) -> &Arc<livi::Features> {
        &self.features
    }

    /// The raw handle to the running DSP instance, for the LV2 `instance-access`
    /// feature — some plugin UIs (e.g. Helm's) require direct access to it.
    /// What saving and restoring presets needs of this instance.
    pub fn instance_ref(&self) -> InstanceRef {
        let instance = self.instance.raw().instance();
        InstanceRef {
            descriptor: instance.descriptor().map_or(std::ptr::null(), |d| d as *const _),
            handle: instance.handle(),
        }
    }

    pub fn raw_handle(&self) -> lv2_raw::core::LV2Handle {
        self.instance.raw().instance().handle()
    }

    /// Metadata for every control-input port meant for a UI, for a parameter
    /// editor, with the port index as the param ID. Order matches the plugin's own
    /// port order, not necessarily display priority.
    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }
}

impl HostedPlugin for Lv2Plugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn audio_output_count(&self) -> usize {
        self.output_buffers.len()
    }

    fn audio_input_count(&self) -> usize {
        self.input_buffers.len()
    }

    fn audio_input_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.input_buffers[channel]
    }

    fn process(&mut self, midi_in: &[MidiEvent], frames: usize) {
        self.midi_in.clear();
        for event in midi_in {
            // Dropping an event that doesn't fit the sequence buffer is preferable
            // to blocking or growing it on the audio thread.
            let _ = self.midi_in.push_midi_event::<MAX_MIDI_MESSAGE_BYTES>(
                i64::from(event.frame_offset),
                self.midi_urid,
                event.as_slice(),
            );
        }
        self.frames_rendered = frames;

        // An output sequence goes in as an empty chunk whose size says how much the
        // plugin may write; the plugin turns it into a sequence.
        for sequence in &mut self.atom_sequence_out {
            sequence.clear_as_chunk();
        }

        let midi_in = &self.midi_in;
        // Audio inputs read what the host wrote into `input_buffers`; every CV
        // input gets the same silent buffer. Every atom-sequence input port gets
        // the same MIDI, if the plugin has any — most effects have none, unlike
        // instruments.
        let cv_silence = &self.cv_silence[..frames];
        let ports = livi::EmptyPortConnections::new()
            .with_audio_inputs(self.input_buffers.iter().map(|buf| &buf[..frames]))
            .with_cv_inputs(std::iter::repeat_n(cv_silence, self.cv_input_count))
            .with_atom_sequence_inputs(std::iter::repeat_n(midi_in, self.atom_sequence_input_count))
            .with_audio_outputs(self.output_buffers.iter_mut().map(|buf| &mut buf[..frames]))
            .with_atom_sequence_outputs(self.atom_sequence_out.iter_mut())
            .with_cv_outputs(self.cv_out_buffers.iter_mut().map(|buf| &mut buf[..frames]));

        if let Err(err) = unsafe { self.instance.run(frames, ports) } {
            log::error!("LV2 plugin '{}' failed to run: {err:?}", self.name);
        }

        self.midi_out.clear();
        for sequence in &self.atom_sequence_out {
            for event in sequence.iter() {
                if event.event.body.mytype != self.midi_urid {
                    continue;
                }
                if self.midi_out.len() == self.midi_out.capacity() {
                    break;
                }
                let offset = event.event.time_in_frames.clamp(0, frames as i64) as u32;
                if let Some(midi) = MidiEvent::from_raw(offset, event.data) {
                    self.midi_out.push(midi);
                }
            }
        }
    }

    fn midi_output(&self) -> &[MidiEvent] {
        &self.midi_out
    }

    fn audio_output(&self, channel: usize) -> &[f32] {
        &self.output_buffers[channel][..self.frames_rendered]
    }

    fn set_param(&mut self, id: u32, value: f32) {
        self.instance
            .set_control_input(livi::PortIndex(id as usize), value);
    }
}
