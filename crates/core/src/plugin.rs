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

/// What kind of control suits a parameter. Both formats say this in their
/// metadata: LV2 through port properties (`lv2:toggled`, `lv2:integer`,
/// `lv2:enumeration`, the `units:midiNote` unit), VST3 through
/// `ParameterInfo::stepCount` and the `kIsList` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// Any value in range.
    Continuous,
    /// Off (`min_value`) or on (`max_value`).
    Toggle,
    /// One of `steps + 1` evenly spaced values, e.g. whole numbers.
    Stepped,
    /// One of [`ParamInfo::choices`].
    Choice,
    /// A MIDI note number.
    Note,
}

/// A named value of a parameter: an option of a [`ParamKind::Choice`], or a label
/// for one particular value of another kind (LV2 scale points).
#[derive(Debug, Clone, PartialEq)]
pub struct ParamChoice {
    pub value: f32,
    pub label: String,
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
    pub kind: ParamKind,
    /// For [`ParamKind::Stepped`] and [`ParamKind::Note`]: the number of steps
    /// between `min_value` and `max_value`. 0 otherwise.
    pub steps: u32,
    /// Whether a slider should move logarithmically (e.g. frequencies). Only set
    /// when `min_value` > 0.
    pub logarithmic: bool,
    /// Unit symbol to show after a value ("dB", "Hz"); empty if none.
    pub unit: String,
    /// The options of a [`ParamKind::Choice`] in order; for other kinds, labels
    /// for particular values (may be empty).
    pub choices: Vec<ParamChoice>,
}

impl ParamInfo {
    /// A continuous parameter with no unit, labels or log scale.
    pub fn continuous(id: u32, name: String, default_value: f32, min_value: f32, max_value: f32) -> Self {
        Self {
            id,
            name,
            default_value,
            min_value,
            max_value,
            kind: ParamKind::Continuous,
            steps: 0,
            logarithmic: false,
            unit: String::new(),
            choices: Vec::new(),
        }
    }

    /// `value` moved onto the nearest value this parameter can take.
    pub fn snap(&self, value: f32) -> f32 {
        let value = value.clamp(self.min_value, self.max_value);
        let span = self.max_value - self.min_value;
        match self.kind {
            ParamKind::Continuous => value,
            ParamKind::Toggle => {
                if value > self.min_value + span / 2.0 { self.max_value } else { self.min_value }
            }
            ParamKind::Stepped | ParamKind::Note if self.steps > 0 => {
                let step = span / self.steps as f32;
                self.min_value + ((value - self.min_value) / step).round() * step
            }
            ParamKind::Stepped | ParamKind::Note => value,
            ParamKind::Choice => self.choice_index(value).map_or(value, |index| self.choices[index].value),
        }
    }

    /// Index of the choice closest to `value`, if there are choices.
    pub fn choice_index(&self, value: f32) -> Option<usize> {
        self.choices
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (a.value - value).abs().total_cmp(&(b.value - value).abs()))
            .map(|(index, _)| index)
    }

    /// `value` as text for display, formatted by kind: a choice's or labelled
    /// value's label, "On"/"Off", a note name, or a number with the unit. Formats
    /// whose plugins format their own values (VST3) prefer those.
    pub fn format_value(&self, value: f32) -> String {
        if let Some(choice) = self.choices.iter().find(|choice| (choice.value - value).abs() < 1e-4)
        {
            return choice.label.clone();
        }
        let number = match self.kind {
            ParamKind::Toggle => {
                let on = value > self.min_value + (self.max_value - self.min_value) / 2.0;
                return if on { "On" } else { "Off" }.to_string();
            }
            ParamKind::Note => return note_name(value.round() as i32),
            ParamKind::Choice => {
                return self
                    .choice_index(value)
                    .map(|index| self.choices[index].label.clone())
                    .unwrap_or_default();
            }
            ParamKind::Stepped if self.span_is_whole_steps() => format!("{}", value.round()),
            ParamKind::Stepped | ParamKind::Continuous => format_number(value),
        };
        if self.unit.is_empty() {
            number
        } else {
            format!("{number} {}", self.unit)
        }
    }

    fn span_is_whole_steps(&self) -> bool {
        self.steps > 0 && ((self.max_value - self.min_value) / self.steps as f32 - 1.0).abs() < 1e-4
    }
}

/// MIDI note number as a name, with middle C (60) as C4.
pub fn note_name(note: i32) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[note.rem_euclid(12) as usize], note.div_euclid(12) - 1)
}

/// About three significant digits, without trailing zeros.
fn format_number(value: f32) -> String {
    let magnitude = value.abs();
    let decimals = if magnitude >= 100.0 {
        0
    } else if magnitude >= 10.0 {
        1
    } else if magnitude >= 1.0 {
        2
    } else {
        3
    };
    let text = format!("{value:.decimals$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    };
    if text == "-0" { "0".to_string() } else { text }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn param(kind: ParamKind, min: f32, max: f32, steps: u32) -> ParamInfo {
        ParamInfo {
            kind,
            steps,
            ..ParamInfo::continuous(0, String::new(), min, min, max)
        }
    }

    #[test]
    fn snap_moves_to_the_nearest_allowed_value() {
        // A VST3-style stepped parameter: 4 steps over normalized 0..1.
        let stepped = param(ParamKind::Stepped, 0.0, 1.0, 4);
        assert_eq!(stepped.snap(0.3), 0.25);
        assert_eq!(stepped.snap(0.4), 0.5);
        assert_eq!(stepped.snap(7.0), 1.0);

        assert_eq!(param(ParamKind::Toggle, 0.0, 1.0, 0).snap(0.6), 1.0);

        let mut choice = param(ParamKind::Choice, 0.0, 10.0, 0);
        choice.choices = [0.0, 2.0, 10.0]
            .map(|value| ParamChoice { value, label: format!("v{value}") })
            .to_vec();
        assert_eq!(choice.snap(5.0), 2.0);
        assert_eq!(choice.format_value(9.0), "v10");
    }

    #[test]
    fn format_value_by_kind() {
        let mut gain = param(ParamKind::Continuous, -18.0, 18.0, 0);
        gain.unit = "dB".to_string();
        assert_eq!(gain.format_value(-6.0), "-6 dB");
        assert_eq!(gain.format_value(0.123_456), "0.123 dB");
        assert_eq!(gain.format_value(1250.4), "1250 dB");
        assert_eq!(param(ParamKind::Note, 0.0, 127.0, 127).format_value(61.0), "C#4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(param(ParamKind::Stepped, 1.0, 16.0, 15).format_value(3.0), "3");
    }
}
