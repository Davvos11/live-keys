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

    /// Instantiate the plugin at `uri`. Only plugins with no audio input ports are
    /// supported for now (i.e. instruments driven purely by MIDI) — effects with
    /// audio inputs are a later phase once the host has a real audio graph.
    pub fn instantiate(&self, uri: &str, sample_rate: f64) -> anyhow::Result<Lv2Plugin> {
        let plugin = self
            .world
            .plugin_by_uri(uri)
            .ok_or_else(|| anyhow::anyhow!("LV2 plugin not found: {uri}"))?;
        let port_counts = *plugin.port_counts();
        if port_counts.audio_inputs != 0 {
            anyhow::bail!(
                "plugin '{uri}' has {} audio input port(s); only instrument plugins \
                 (no audio inputs) are supported right now",
                port_counts.audio_inputs
            );
        }

        let name = plugin.name();
        let instance = unsafe { plugin.instantiate(self.features.clone(), sample_rate)? };
        let output_buffers = (0..port_counts.audio_outputs)
            .map(|_| vec![0.0f32; self.max_block_size])
            .collect();

        Ok(Lv2Plugin {
            name,
            plugin,
            features: self.features.clone(),
            instance,
            midi_in: LV2AtomSequence::new(&self.features, MIDI_EVENT_BUFFER_BYTES),
            midi_urid: self.features.midi_urid(),
            output_buffers,
            frames_rendered: 0,
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
    output_buffers: Vec<Vec<f32>>,
    frames_rendered: usize,
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
        let ports = livi::EmptyPortConnections::new()
            .with_atom_sequence_inputs(std::iter::once(midi_in))
            .with_audio_outputs(self.output_buffers.iter_mut().map(|buf| &mut buf[..frames]));

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
