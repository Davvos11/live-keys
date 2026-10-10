use crate::MidiEvent;

/// The plugin standards this host can load. Each has its own adapter crate
/// implementing [`HostedPlugin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    Lv2,
    Vst3,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Lv2 => "LV2",
            PluginFormat::Vst3 => "VST3",
        }
    }
}

/// What a plugin does, which decides where in a slot's chain it can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    /// MIDI in, audio out.
    Instrument,
    /// MIDI in, MIDI out, no audio out (arpeggiators, transposers, filters).
    MidiEffect,
    /// Audio in, audio out — and anything else that isn't one of the above.
    AudioEffect,
}

impl PluginKind {
    pub fn label(self) -> &'static str {
        match self {
            PluginKind::Instrument => "instrument",
            PluginKind::MidiEffect => "MIDI effect",
            PluginKind::AudioEffect => "audio effect",
        }
    }
}

/// Metadata for a discovered plugin, used to populate a plugin picker before
/// instantiating one.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Identifies the plugin across scans and app restarts, unique across formats:
    /// an LV2 plugin's URI, or `vst3:` + a VST3 class ID in hex.
    pub id: String,
    pub name: String,
    pub format: PluginFormat,
    pub kind: PluginKind,
}

/// Metadata for one automatable parameter, used to populate a parameter editor UI
/// (macro knobs, a fallback UI for GUI-less plugins, future MIDI binding).
///
/// `id` is what [`HostedPlugin::set_param`] takes, in the format's own terms: an
/// LV2 control-input port index, or a VST3 `ParamID`. Values are in the format's
/// own terms too — an LV2 port's real range, or VST3's normalized `0.0..=1.0`.
/// `min_value`/`max_value` are always finite, since a UI can't usefully expose an
/// infinite slider.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub default_value: f32,
    pub min_value: f32,
    pub max_value: f32,
}

/// A plugin instance that can be driven from the real-time audio thread.
///
/// Implementations must not allocate, lock, or perform blocking syscalls inside
/// `process`. Format-specific adapters (LV2, VST3, and later VST2) implement this
/// trait so the audio graph and MIDI routing stay format-agnostic.
pub trait HostedPlugin: Send {
    fn name(&self) -> &str;

    /// Number of audio output channels this plugin produces.
    fn audio_output_count(&self) -> usize;

    /// Number of audio input channels this plugin takes (0 for e.g. most
    /// instruments).
    fn audio_input_count(&self) -> usize;

    /// The plugin's own buffer for input channel `channel`, for the host to fill
    /// before the next `process` call — the input-side counterpart of
    /// `audio_output`. At least as long as the largest `frames` the plugin will be
    /// asked to process; only the first `frames` samples are read. Contents
    /// persist between calls, so a host with nothing to feed must zero it.
    fn audio_input_mut(&mut self, channel: usize) -> &mut [f32];

    /// Render `frames` samples from the current input buffers, consuming
    /// `midi_in` events for this block.
    fn process(&mut self, midi_in: &[MidiEvent], frames: usize);

    /// Read back the audio rendered by the most recent `process` call for output
    /// channel `channel`. The returned slice has length equal to the `frames`
    /// passed to that call.
    fn audio_output(&self, channel: usize) -> &[f32];

    /// MIDI the most recent `process` call produced, for a MIDI effect: the input
    /// of whatever comes next in the chain. Plugins without MIDI output return
    /// nothing.
    fn midi_output(&self) -> &[MidiEvent] {
        &[]
    }

    /// Set a parameter to a new value, taking effect from the next `process` call.
    /// `id` and `value` are as described on [`ParamInfo`]; an unknown `id` is a
    /// no-op.
    fn set_param(&mut self, id: u32, value: f32);
}
