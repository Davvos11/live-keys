use crate::MidiEvent;

/// The plugin standards this host can load. Each has its own adapter crate
/// implementing [`HostedPlugin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    Lv2,
    Vst3,
    Clap,
    /// live-keys' own instruments (`live-keys-builtin`).
    Builtin,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Lv2 => "LV2",
            PluginFormat::Vst3 => "VST3",
            PluginFormat::Clap => "CLAP",
            PluginFormat::Builtin => "Built-in",
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
    /// an LV2 plugin's URI, `vst3:` + a VST3 class ID in hex, `clap:` + a CLAP
    /// plugin id, or `builtin:` + one of our own instruments' id.
    pub id: String,
    pub name: String,
    pub format: PluginFormat,
    pub kind: PluginKind,
}

/// A saved set of a plugin's settings, as the plugin's format stores them: an LV2
/// preset (`pset:Preset`), a VST3 `.vstpreset` file, or a CLAP preset found by
/// its preset discovery — so presets made by other hosts (Ardour, Carla, a DAW)
/// show up here and the other way around. (CLAP has no shared format for user
/// presets; saved ones are this host's state files.)
#[derive(Debug, Clone, PartialEq)]
pub struct PresetInfo {
    /// Identifies the preset within its plugin: an LV2 preset URI, a
    /// `.vstpreset` file's path, or a CLAP preset's location (see
    /// `live-keys-clap-host`).
    pub id: String,
    pub name: String,
    /// Saved by the user (and so deletable), as opposed to shipped with the
    /// plugin.
    pub user: bool,
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
    /// A reference to the parameter that stays the same across plugin versions,
    /// for profiles to match on: the LV2 port symbol (an LV2 `id` is a port
    /// index, which moves when a plugin adds a port), or the VST3/CLAP id in
    /// decimal.
    pub symbol: String,
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
    /// A continuous parameter with no unit, labels or log scale, its symbol the
    /// id in decimal.
    pub fn continuous(id: u32, name: String, default_value: f32, min_value: f32, max_value: f32) -> Self {
        Self {
            id,
            symbol: id.to_string(),
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
/// A plugin's own value text with an over-long number at its start shortened
/// like [`ParamInfo::format_value`]'s ("94.4444656" → "94.4", "0.0012345 s" →
/// "0.001 s"); anything else is left as it is.
pub fn tidy_value_text(text: &str) -> String {
    let end = text
        .char_indices()
        .find(|&(index, c)| !(c.is_ascii_digit() || c == '.' || (index == 0 && (c == '-' || c == '+'))))
        .map_or(text.len(), |(index, _)| index);
    let (number, rest) = text.split_at(end);
    let decimals = number.split_once('.').map_or(0, |(_, decimals)| decimals.len());
    match number.parse::<f32>() {
        Ok(value) if decimals > 3 => format!("{}{rest}", format_number(value)),
        _ => text.to_string(),
    }
}

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

/// The musical time plugins are told about (tempo-synced delays, LFOs,
/// arpeggiators): the metronome's tempo and time signature, and while it plays,
/// where it is. Positions are at the first frame of the block about to be
/// processed. Beats are the time signature's beat unit (an eighth in 6/8);
/// formats that count quarter notes convert with [`Transport::quarters`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    /// Beats per minute.
    pub tempo: f64,
    pub playing: bool,
    pub beats_per_bar: u32,
    /// The note value of a beat: 4 a quarter, 8 an eighth.
    pub beat_unit: u32,
    /// Beats since the transport started.
    pub beats: f64,
    /// Where the bar it's in started, in beats since the transport started.
    pub bar_start: f64,
    /// The bar it's in, counting from 0.
    pub bar: i64,
    /// Frames since the transport started.
    pub frame: i64,
}

impl Default for Transport {
    fn default() -> Self {
        Self { tempo: 120.0, playing: false, beats_per_bar: 4, beat_unit: 4, beats: 0.0, bar_start: 0.0, bar: 0, frame: 0 }
    }
}

impl Transport {
    /// `beats` (in this transport's beat unit) as quarter notes.
    pub fn quarters(&self, beats: f64) -> f64 {
        beats * 4.0 / f64::from(self.beat_unit.max(1))
    }

    /// The tempo in quarter notes per minute.
    pub fn quarter_tempo(&self) -> f64 {
        self.quarters(self.tempo)
    }

    /// Seconds since the transport started (the tempo has been constant as far
    /// as plugins know).
    pub fn seconds(&self) -> f64 {
        self.beats * 60.0 / self.tempo.max(1.0)
    }
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

    /// The musical time for the next `process` call; called before every
    /// block. Plugins that don't follow tempo ignore it (the default).
    fn set_transport(&mut self, _transport: &Transport) {}
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

    #[test]
    fn tidy_value_text_shortens_long_numbers_only() {
        assert_eq!(tidy_value_text("94.4444656"), "94.4");
        assert_eq!(tidy_value_text("-0.0012345 s"), "-0.001 s");
        assert_eq!(tidy_value_text("12.50 dB"), "12.50 dB");
        assert_eq!(tidy_value_text("Soft"), "Soft");
        assert_eq!(tidy_value_text("1.2.3456"), "1.2.3456");
        assert_eq!(tidy_value_text(""), "");
    }
}
