use crate::MidiEvent;

/// A plugin instance that can be driven from the real-time audio thread.
///
/// Implementations must not allocate, lock, or perform blocking syscalls inside
/// `process`. Format-specific adapters (LV2, and later VST3/VST2) implement this
/// trait so the audio graph and MIDI routing stay format-agnostic.
pub trait HostedPlugin: Send {
    fn name(&self) -> &str;

    /// Number of audio output channels this plugin produces.
    fn audio_output_count(&self) -> usize;

    /// Render `frames` samples, consuming `midi_in` events for this block.
    fn process(&mut self, midi_in: &[MidiEvent], frames: usize);

    /// Read back the audio rendered by the most recent `process` call for output
    /// channel `channel`. The returned slice has length equal to the `frames`
    /// passed to that call.
    fn audio_output(&self, channel: usize) -> &[f32];

    /// Set a control-port input to a new value. `port_index` matches the plugin's
    /// own port indexing (as exposed by the format-specific adapter); an
    /// out-of-range or non-control-port index is a no-op.
    fn set_control(&mut self, port_index: u32, value: f32);
}
