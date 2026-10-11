//! Every class of plugin this host has met a quirk of, one representative each.
//! Add a case when a new kind of plugin turns up a new problem.

use std::time::Duration;

pub enum Role {
    Instrument,
    /// After [`crate::SOURCE`] in the audio section.
    AudioEffect,
    /// In front of [`crate::SOURCE`] in the MIDI section.
    MidiEffect,
}

/// What resizing the plugin's GUI window should do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    /// The plugin's widget fills the resized window.
    Follows,
    /// The widget keeps its size.
    Fixed,
    /// The plugin owns its window (External UI); the host can't resize it.
    NotByHost,
    /// No GUI the host can show.
    NoGui,
}

pub enum Target {
    Min,
    Max,
}

/// A parameter to set (by exact name) and read back.
pub struct ParamCheck {
    pub name: &'static str,
    pub target: Target,
    /// Setting it should make a note at least this many dB quieter (a volume
    /// at its minimum): proof the change reached the DSP, not just our records.
    pub quieter_db: Option<f32>,
}

pub struct Case {
    pub name: &'static str,
    pub plugin: &'static str,
    /// What's special about this class of plugin.
    pub quirks: &'static str,
    pub role: Role,
    /// A note should be heard.
    pub sound: bool,
    pub param: Option<ParamCheck>,
    pub resize: Resize,
    /// The GUI talks to the DSP through atoms, both ways.
    pub atoms: bool,
    /// Time to let the plugin get ready before playing.
    pub warmup: Duration,
}

const fn param(name: &'static str, target: Target, quieter_db: Option<f32>) -> Option<ParamCheck> {
    Some(ParamCheck { name, target, quieter_db })
}

const NO_WAIT: Duration = Duration::ZERO;

pub const CASES: &[Case] = &[
    Case {
        name: "lv2-plain",
        plugin: "http://drobilla.net/plugins/mda/EPiano",
        quirks: "LV2 baseline: control ports only, no state, no GUI",
        role: Role::Instrument,
        sound: true,
        param: param("Envelope Decay", Target::Max, None),
        resize: Resize::NoGui,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-x11-juce",
        plugin: "https://surge-synthesizer.github.io/lv2/surge-xt",
        quirks: "JUCE X11 UI with instance access: its ui:resize takes the UI handle, it reads the \
                 instance while closing, no control ports, an atom output",
        role: Role::Instrument,
        sound: true,
        param: None,
        resize: Resize::Follows,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-atom-gui",
        plugin: "http://gareus.org/oss/lv2/b_synth",
        quirks: "no control ports at all: GUI and DSP talk only through atoms (setBfree)",
        role: Role::Instrument,
        sound: true,
        param: None,
        resize: Resize::Follows,
        atoms: true,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-external-ui",
        plugin: "http://tytel.org/helm",
        quirks: "External UI owning its window (JUCE; wedges if closed while starting up)",
        role: Role::Instrument,
        sound: true,
        param: param("volume", Target::Min, Some(20.0)),
        resize: Resize::NotByHost,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-lsp",
        plugin: "http://lsp-plug.in/plugins/lv2/sampler_mono",
        quirks: "LSP toolkit X11 UI; a sampler, silent without samples",
        role: Role::Instrument,
        sound: false,
        param: param("Output gain", Target::Min, None),
        resize: Resize::Follows,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-audio-effect",
        plugin: "http://gareus.org/oss/lv2/fil4#stereo",
        quirks: "audio effect with a robtk X11 UI fed through its atom ports (x42-eq); its UI \
                 leaves GLib thread-local data behind, so UI binaries must never be unloaded",
        role: Role::AudioEffect,
        sound: true,
        param: param("Gain", Target::Min, Some(12.0)),
        resize: Resize::Follows,
        atoms: true,
        warmup: NO_WAIT,
    },
    Case {
        name: "lv2-midi-effect",
        plugin: "http://gareus.org/oss/lv2/midifilter#miditranspose",
        quirks: "LV2 MIDI effect: MIDI in, MIDI out to the instrument (transposing out of range \
                 silences it)",
        role: Role::MidiEffect,
        sound: true,
        param: param("Transpose", Target::Min, Some(20.0)),
        resize: Resize::NoGui,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "vst3-juce",
        plugin: "vst3:ABCDEF019182FAEB566D624153675854",
        quirks: "JUCE VST3 (Surge XT): editor on the main thread with its run loop, MIDI CCs as \
                 parameters, state applied asynchronously",
        role: Role::Instrument,
        sound: true,
        param: param("Global Volume", Target::Min, Some(20.0)),
        resize: Resize::Follows,
        atoms: false,
        warmup: NO_WAIT,
    },
    Case {
        name: "vst3-hise",
        plugin: "vst3:ABCDEF019182FAEB4162636441626364",
        quirks: "HISE VST3 (OT P1ANO S): loads its samples only while processing runs",
        role: Role::Instrument,
        sound: true,
        param: param("Bypass", Target::Max, None),
        resize: Resize::Fixed,
        atoms: false,
        warmup: Duration::from_secs(10),
    },
    Case {
        name: "vst3-midi-effect",
        plugin: "vst3:4B6579626F6172644D6972726F727272",
        quirks: "VST3 MIDI effect (JUCE): found by probing its buses, MIDI out as events, no editor",
        role: Role::MidiEffect,
        sound: true,
        param: None,
        resize: Resize::NoGui,
        atoms: false,
        warmup: NO_WAIT,
    },
];
