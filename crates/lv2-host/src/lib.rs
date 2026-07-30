use std::sync::Arc;

use live_keys_core::{HostedPlugin, MidiEvent};
use livi::event::LV2AtomSequence;

pub use livi;
pub use lv2_raw;

/// Capacity, in bytes, of the atom sequence buffer used to hand MIDI events to a
/// plugin each block. Generous enough for a dense chord/arpeggio burst.
const MIDI_EVENT_BUFFER_BYTES: usize = 4096;
/// Upper bound on the size of a single MIDI message written into the atom sequence.
const MAX_MIDI_MESSAGE_BYTES: usize = 32;

/// Metadata for a discovered LV2 plugin, used to populate a plugin picker before
/// instantiating one.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub uri: String,
    pub name: String,
    pub is_instrument: bool,
}

/// Metadata for one control-input port, used to populate a parameter editor UI
/// (macro knobs, a fallback UI for GUI-less plugins, future MIDI binding).
/// `min_value`/`max_value` are always finite: an LV2 port that declares no bound
/// falls back to a range around `default_value` since a UI can't usefully expose
/// an infinite slider.
#[derive(Debug, Clone)]
pub struct ControlPortInfo {
    pub port_index: u32,
    pub name: String,
    pub default_value: f32,
    pub min_value: f32,
    pub max_value: f32,
}

/// Owns the LV2 plugin world and the real-time features (URID map, block size) that
/// every plugin instantiated from it shares.
pub struct Lv2Host {
    world: livi::World,
    features: Arc<livi::Features>,
    max_block_size: usize,
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
        }
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.world
            .iter_plugins()
            .map(|p| PluginInfo {
                uri: p.uri(),
                name: p.name(),
                is_instrument: p.is_instrument(),
            })
            .collect()
    }

    /// Instantiate the plugin at `uri`. Plugins with audio input ports are
    /// accepted, but since there's no real audio graph/routing yet (see
    /// `Lv2Plugin::process`), those inputs (and any CV inputs) are always fed
    /// silence — this makes e.g. effect plugins loadable and playable via
    /// MIDI-driven control ports today, ahead of real audio-in routing landing
    /// later. Plugins that write an atom-sequence or CV output port (e.g. an
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

        Ok(Lv2Plugin {
            name,
            plugin,
            features: self.features.clone(),
            instance,
            midi_in: LV2AtomSequence::new(&self.features, MIDI_EVENT_BUFFER_BYTES),
            midi_urid: self.features.midi_urid(),
            atom_sequence_input_count: port_counts.atom_sequence_inputs,
            output_buffers,
            frames_rendered: 0,
            input_silence: vec![0.0f32; self.max_block_size],
            audio_input_count: port_counts.audio_inputs,
            cv_input_count: port_counts.cv_inputs,
            atom_sequence_out,
            cv_out_buffers,
        })
    }
}

/// A running instance of an LV2 instrument plugin.
pub struct Lv2Plugin {
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
    /// Always-zero buffer fed to every audio and CV input port (see
    /// `process`) — never written to.
    input_silence: Vec<f32>,
    audio_input_count: usize,
    cv_input_count: usize,
    /// Scratch buffer(s) connected to the plugin's atom-sequence output
    /// port(s), if any — written by the plugin each block (e.g. MIDI-out, or a
    /// synth's UI-state messages) and then discarded; nothing downstream
    /// consumes it yet.
    atom_sequence_out: Vec<LV2AtomSequence>,
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
    pub fn raw_handle(&self) -> lv2_raw::core::LV2Handle {
        self.instance.raw().instance().handle()
    }

    /// Metadata for every control-input port, for a parameter-editor UI. Order
    /// matches the plugin's own port order, not necessarily display priority.
    pub fn control_ports(&self) -> Vec<ControlPortInfo> {
        self.plugin
            .ports_with_type(livi::PortType::ControlInput)
            .map(|port| {
                // A control port with no declared range can't be given a usable
                // slider bound, so fall back to a range around its default.
                let min_value = port.min_value.unwrap_or_else(|| port.default_value.min(0.0));
                let max_value = port.max_value.unwrap_or(if port.default_value > min_value {
                    port.default_value * 2.0
                } else {
                    min_value + 1.0
                });
                ControlPortInfo {
                    port_index: port.index.0 as u32,
                    name: port.name,
                    default_value: port.default_value,
                    min_value,
                    max_value,
                }
            })
            .collect()
    }
}

impl HostedPlugin for Lv2Plugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn audio_output_count(&self) -> usize {
        self.output_buffers.len()
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

        let midi_in = &self.midi_in;
        // Every audio/CV input port gets the same silent buffer — there's no
        // real audio-in routing yet (see `Lv2Host::instantiate`'s docs). Every
        // atom-sequence input port gets the same MIDI, if the plugin has any —
        // most effects have none, unlike instruments.
        let silence = &self.input_silence[..frames];
        let ports = livi::EmptyPortConnections::new()
            .with_audio_inputs(std::iter::repeat_n(silence, self.audio_input_count))
            .with_cv_inputs(std::iter::repeat_n(silence, self.cv_input_count))
            .with_atom_sequence_inputs(std::iter::repeat_n(midi_in, self.atom_sequence_input_count))
            .with_audio_outputs(self.output_buffers.iter_mut().map(|buf| &mut buf[..frames]))
            .with_atom_sequence_outputs(self.atom_sequence_out.iter_mut())
            .with_cv_outputs(self.cv_out_buffers.iter_mut().map(|buf| &mut buf[..frames]));

        if let Err(err) = unsafe { self.instance.run(frames, ports) } {
            log::error!("LV2 plugin '{}' failed to run: {err:?}", self.name);
        }
    }

    fn audio_output(&self, channel: usize) -> &[f32] {
        &self.output_buffers[channel][..self.frames_rendered]
    }

    fn set_control(&mut self, port_index: u32, value: f32) {
        self.instance
            .set_control_input(livi::PortIndex(port_index as usize), value);
    }
}
