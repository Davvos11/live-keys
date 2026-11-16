//! A region: one sample (or generator) with the conditions that play it and how
//! it sounds, built from the opcodes of its `<global>`, `<master>`, `<group>` and
//! `<region>` headers, in that order.

/// What makes a region play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// On note-on (the default).
    Attack,
    /// On note-off (or when the sustain pedal lets the note go), with `rt_decay`.
    Release,
    /// On note-on when no other key is held.
    First,
    /// On note-on while another key is held.
    Legato,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Plays to the end, stops on note-off after its release.
    NoLoop,
    /// Plays to the end whatever happens; note-offs are ignored.
    OneShot,
    /// Loops between `loop_start` and `loop_end` until the voice ends.
    Continuous,
    /// Loops while the note is held, then plays on to the end.
    Sustain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffMode {
    /// A few milliseconds' fade.
    Fast,
    /// The region's own release.
    Normal,
    /// `off_time`.
    Time,
}

/// What a controller (`*_onccN`) modulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Multiplies the gain by depth/100 × curve (so `amplitude_oncc7=100` with
    /// CC 7 at 0 is silent).
    Amplitude,
    /// dB, added.
    Volume,
    /// −100..100, added.
    Pan,
    /// −100..100, added.
    Width,
    /// −100..100, added (to pan, after width).
    Position,
    /// Cents, added.
    Pitch,
    /// %, added to `amp_veltrack` at note-on.
    AmpVeltrack,
    /// Frames, added to `offset` at note-on.
    Offset,
    /// Seconds (sustain: %), added to the amplitude envelope at note-on.
    EgDelay,
    EgAttack,
    EgHold,
    EgDecay,
    EgSustain,
    EgRelease,
}

impl Target {
    /// Whether a change is followed while the voice plays, rather than read once
    /// at note-on.
    pub fn is_live(self) -> bool {
        matches!(self, Target::Amplitude | Target::Volume | Target::Pan | Target::Width | Target::Position | Target::Pitch)
    }

    fn from_name(name: &str) -> Option<Target> {
        Some(match name {
            "amplitude" => Target::Amplitude,
            "volume" | "gain" => Target::Volume,
            "pan" => Target::Pan,
            "width" => Target::Width,
            "position" => Target::Position,
            "pitch" | "tune" => Target::Pitch,
            "amp_veltrack" => Target::AmpVeltrack,
            "offset" => Target::Offset,
            "ampeg_delay" => Target::EgDelay,
            "ampeg_attack" => Target::EgAttack,
            "ampeg_hold" => Target::EgHold,
            "ampeg_decay" => Target::EgDecay,
            "ampeg_sustain" => Target::EgSustain,
            "ampeg_release" => Target::EgRelease,
            _ => return None,
        })
    }
}

/// One controller modulating one target: `depth × curve(cc)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CcMod {
    pub target: Target,
    pub cc: u16,
    pub depth: f32,
    pub curve: u16,
}

/// A controller's range, in MIDI values 0..=127 (`locc64=64 hicc64=127`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CcRange {
    pub cc: u16,
    pub lo: f32,
    pub hi: f32,
}

impl CcRange {
    pub fn contains(&self, value: f32) -> bool {
        (self.lo..=self.hi).contains(&value)
    }
}

/// The amplitude envelope (DAHDSR), in seconds; sustain 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeTimes {
    pub delay: f32,
    pub attack: f32,
    pub hold: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Default for EnvelopeTimes {
    fn default() -> Self {
        EnvelopeTimes { delay: 0.0, attack: 0.0, hold: 0.0, decay: 0.0, sustain: 1.0, release: 0.001 }
    }
}

/// A sound source that isn't a sample file: SFZ's `*sine`, `*saw`… The hook for
/// modelled sounds layered with samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    Sine,
    Triangle,
    Saw,
    Square,
    Noise,
    Silence,
}

impl Generator {
    pub fn from_name(name: &str) -> Option<Generator> {
        Some(match name {
            "*sine" => Generator::Sine,
            "*tri" | "*triangle" => Generator::Triangle,
            "*saw" => Generator::Saw,
            "*square" => Generator::Square,
            "*noise" => Generator::Noise,
            "*silence" => Generator::Silence,
            _ => return None,
        })
    }
}

/// Where a region's sound comes from, once the instrument is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Not resolved yet (or failed to load).
    None,
    /// Index into the instrument's samples.
    Sample(u32),
    Generator(Generator),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// `sample=` as written, and the `default_path` in effect for it.
    pub sample: String,
    pub default_path: String,
    pub source: Source,

    // Sample playback, in frames.
    pub offset: u32,
    pub end: Option<u32>,
    pub loop_mode: Option<LoopMode>,
    pub loop_start: Option<u32>,
    pub loop_end: Option<u32>,

    // Conditions.
    /// Inclusive; `None` = the region has no key (it's triggered by a controller).
    pub keys: Option<(u8, u8)>,
    pub lovel: u8,
    pub hivel: u8,
    /// 1..=16, inclusive.
    pub lochan: u8,
    pub hichan: u8,
    pub cc_conditions: Vec<CcRange>,
    /// `on_loccN`/`on_hiccN`: played when controller N moves into the range.
    pub cc_triggers: Vec<CcRange>,
    pub lorand: f32,
    pub hirand: f32,
    pub seq_length: u32,
    pub seq_position: u32,
    pub trigger: Trigger,
    pub sw_last: Option<u8>,
    pub sw_lokey: Option<u8>,
    pub sw_hikey: Option<u8>,
    pub sw_default: Option<u8>,

    // Voices.
    pub group: i64,
    pub off_by: Option<i64>,
    pub off_mode: OffMode,
    pub off_time: f32,
    /// Voices of this region's group at once.
    pub polyphony: Option<u32>,
    /// Voices of one key in this region's group at once.
    pub note_polyphony: Option<u32>,

    // Pitch.
    pub pitch_keycenter: u8,
    /// Cents per key.
    pub pitch_keytrack: f32,
    /// Cents at full velocity.
    pub pitch_veltrack: f32,
    pub transpose: i32,
    /// Cents.
    pub tune: f32,
    /// Cents at full bend.
    pub bend_up: f32,
    pub bend_down: f32,

    // Amplitude.
    /// dB.
    pub volume: f32,
    /// %.
    pub amplitude: f32,
    pub pan: f32,
    pub width: f32,
    pub position: f32,
    /// %.
    pub amp_veltrack: f32,
    /// `amp_velcurve_N` points (velocity, gain).
    pub amp_velcurve: Vec<(u8, f32)>,
    /// dB per key from `amp_keycenter`.
    pub amp_keytrack: f32,
    pub amp_keycenter: u8,
    /// dB per second the note was held, for release triggers.
    pub rt_decay: f32,
    pub xfin_lovel: u8,
    pub xfin_hivel: u8,
    pub xfout_lovel: u8,
    pub xfout_hivel: u8,
    /// Equal-power (the default) rather than linear velocity crossfades.
    pub xf_power: bool,
    pub ampeg: EnvelopeTimes,

    pub mods: Vec<CcMod>,
}

impl Default for Region {
    fn default() -> Self {
        Region {
            sample: String::new(),
            default_path: String::new(),
            source: Source::None,
            offset: 0,
            end: None,
            loop_mode: None,
            loop_start: None,
            loop_end: None,
            keys: Some((0, 127)),
            lovel: 1,
            hivel: 127,
            lochan: 1,
            hichan: 16,
            cc_conditions: Vec::new(),
            cc_triggers: Vec::new(),
            lorand: 0.0,
            hirand: 1.0,
            seq_length: 1,
            seq_position: 1,
            trigger: Trigger::Attack,
            sw_last: None,
            sw_lokey: None,
            sw_hikey: None,
            sw_default: None,
            group: 0,
            off_by: None,
            off_mode: OffMode::Fast,
            off_time: 0.006,
            polyphony: None,
            note_polyphony: None,
            pitch_keycenter: 60,
            pitch_keytrack: 100.0,
            pitch_veltrack: 0.0,
            transpose: 0,
            tune: 0.0,
            bend_up: 200.0,
            bend_down: -200.0,
            volume: 0.0,
            amplitude: 100.0,
            pan: 0.0,
            width: 100.0,
            position: 0.0,
            amp_veltrack: 100.0,
            amp_velcurve: Vec::new(),
            amp_keytrack: 0.0,
            amp_keycenter: 60,
            rt_decay: 0.0,
            xfin_lovel: 0,
            xfin_hivel: 0,
            xfout_lovel: 127,
            xfout_hivel: 127,
            xf_power: true,
            ampeg: EnvelopeTimes::default(),
            mods: Vec::new(),
        }
    }
}

/// Settings from `<control>` that affect how opcodes are read.
#[derive(Debug, Clone, Default)]
pub struct ParseContext {
    /// Added to every note number (`note_offset`, `octave_offset` × 12).
    pub note_offset: i32,
}

/// Whether an opcode was understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Yes,
    /// Known name, but its value couldn't be read.
    BadValue,
    Unknown,
}

impl Region {
    /// The effective loop mode: `loop_mode` if given, else looping if loop points
    /// are, else (`one_shot` for generators, which never end) no loop.
    pub fn effective_loop_mode(&self) -> LoopMode {
        if let Some(mode) = self.loop_mode {
            return mode;
        }
        if self.loop_start.is_some() && self.loop_end.is_some() {
            LoopMode::Continuous
        } else {
            LoopMode::NoLoop
        }
    }

    pub fn applies_to_key(&self, key: u8) -> bool {
        self.keys.is_some_and(|(lo, hi)| (lo..=hi).contains(&key))
    }

    /// Applies one opcode. Unknown and unreadable ones are reported, not fatal:
    /// instruments are often written for another player's extensions.
    pub fn apply(&mut self, name: &str, value: &str, ctx: &ParseContext) -> Applied {
        let (base, number) = split_number(name);
        let ok = |o: Option<()>| if o.is_some() { Applied::Yes } else { Applied::BadValue };
        if base.ends_with("_label") {
            return Applied::Yes; // names for a player's UI (`group_label`…)
        }
        match (base, number) {
            ("sample", None) => {
                self.sample = value.to_string();
                Applied::Yes
            }
            ("offset", None) => ok(int(value).map(|v| self.offset = v.max(0) as u32)),
            ("end", None) => ok(int(value).map(|v| self.end = Some(v.max(0) as u32))),
            ("loop_mode" | "loopmode", None) => ok(match value {
                "no_loop" => Some(LoopMode::NoLoop),
                "one_shot" => Some(LoopMode::OneShot),
                "loop_continuous" => Some(LoopMode::Continuous),
                "loop_sustain" => Some(LoopMode::Sustain),
                _ => None,
            }
            .map(|m| self.loop_mode = Some(m))),
            ("loop_start" | "loopstart", None) => ok(int(value).map(|v| self.loop_start = Some(v.max(0) as u32))),
            ("loop_end" | "loopend", None) => ok(int(value).map(|v| self.loop_end = Some(v.max(0) as u32))),

            ("key", None) => ok(self.note_or_none(value, ctx).map(|k| {
                self.keys = k.map(|k| (k, k));
                if let Some(k) = k {
                    self.pitch_keycenter = k;
                }
            })),
            ("lokey", None) => ok(self.note_or_none(value, ctx).map(|k| {
                self.keys = k.map(|k| (k, self.keys.map_or(127, |(_, hi)| hi)));
            })),
            ("hikey", None) => ok(self.note_or_none(value, ctx).map(|k| {
                self.keys = k.map(|k| (self.keys.map_or(0, |(lo, _)| lo), k));
            })),
            ("lovel", None) => ok(midi_value(value).map(|v| self.lovel = v)),
            ("hivel", None) => ok(midi_value(value).map(|v| self.hivel = v)),
            ("lochan", None) => ok(int(value).map(|v| self.lochan = v.clamp(1, 16) as u8)),
            ("hichan", None) => ok(int(value).map(|v| self.hichan = v.clamp(1, 16) as u8)),
            ("locc", Some(cc)) => ok(float(value).map(|v| cc_range(&mut self.cc_conditions, cc).lo = v)),
            ("hicc", Some(cc)) => ok(float(value).map(|v| cc_range(&mut self.cc_conditions, cc).hi = v)),
            ("on_locc" | "start_locc", Some(cc)) => ok(float(value).map(|v| cc_range(&mut self.cc_triggers, cc).lo = v)),
            ("on_hicc" | "start_hicc", Some(cc)) => ok(float(value).map(|v| cc_range(&mut self.cc_triggers, cc).hi = v)),
            ("lorand", None) => ok(float(value).map(|v| self.lorand = v)),
            ("hirand", None) => ok(float(value).map(|v| self.hirand = v)),
            ("seq_length", None) => ok(int(value).map(|v| self.seq_length = v.max(1) as u32)),
            ("seq_position", None) => ok(int(value).map(|v| self.seq_position = v.max(1) as u32)),
            ("trigger", None) => ok(match value {
                "attack" => Some(Trigger::Attack),
                "release" | "release_key" => Some(Trigger::Release),
                "first" => Some(Trigger::First),
                "legato" => Some(Trigger::Legato),
                _ => None,
            }
            .map(|t| self.trigger = t)),
            ("sw_last", None) => ok(note(value, ctx).map(|k| self.sw_last = Some(k))),
            ("sw_lokey", None) => ok(note(value, ctx).map(|k| self.sw_lokey = Some(k))),
            ("sw_hikey", None) => ok(note(value, ctx).map(|k| self.sw_hikey = Some(k))),
            ("sw_default", None) => ok(note(value, ctx).map(|k| self.sw_default = Some(k))),

            ("group", None) => ok(int(value).map(|v| self.group = v)),
            ("off_by", None) => ok(int(value).map(|v| self.off_by = Some(v))),
            ("off_mode", None) => ok(match value {
                "fast" => Some(OffMode::Fast),
                "normal" => Some(OffMode::Normal),
                "time" => Some(OffMode::Time),
                _ => None,
            }
            .map(|m| self.off_mode = m)),
            ("off_time", None) => ok(float(value).map(|v| {
                self.off_time = v.max(0.0);
                self.off_mode = OffMode::Time;
            })),
            ("polyphony", None) => ok(int(value).map(|v| self.polyphony = Some(v.max(1) as u32))),
            ("note_polyphony", None) => ok(int(value).map(|v| self.note_polyphony = Some(v.max(1) as u32))),

            ("pitch_keycenter", None) => ok(note(value, ctx).map(|k| self.pitch_keycenter = k)),
            ("pitch_keytrack", None) => ok(float(value).map(|v| self.pitch_keytrack = v)),
            ("pitch_veltrack", None) => ok(float(value).map(|v| self.pitch_veltrack = v)),
            ("transpose", None) => ok(int(value).map(|v| self.transpose = v as i32)),
            ("tune" | "pitch", None) => ok(float(value).map(|v| self.tune = v)),
            ("bend_up" | "bendup", None) => ok(float(value).map(|v| self.bend_up = v)),
            ("bend_down" | "benddown", None) => ok(float(value).map(|v| self.bend_down = v)),

            ("volume" | "gain", None) => ok(float(value).map(|v| self.volume = v)),
            ("amplitude", None) => ok(float(value).map(|v| self.amplitude = v)),
            ("pan", None) => ok(float(value).map(|v| self.pan = v)),
            ("width", None) => ok(float(value).map(|v| self.width = v)),
            ("position", None) => ok(float(value).map(|v| self.position = v)),
            ("amp_veltrack", None) => ok(float(value).map(|v| self.amp_veltrack = v)),
            ("amp_velcurve_", Some(vel)) if vel < 128 => ok(float(value).map(|v| {
                self.amp_velcurve.retain(|&(p, _)| p as u16 != vel);
                self.amp_velcurve.push((vel as u8, v));
            })),
            ("amp_keytrack", None) => ok(float(value).map(|v| self.amp_keytrack = v)),
            ("amp_keycenter", None) => ok(note(value, ctx).map(|k| self.amp_keycenter = k)),
            ("rt_decay", None) => ok(float(value).map(|v| self.rt_decay = v)),
            ("xfin_lovel", None) => ok(midi_value(value).map(|v| self.xfin_lovel = v)),
            ("xfin_hivel", None) => ok(midi_value(value).map(|v| self.xfin_hivel = v)),
            ("xfout_lovel", None) => ok(midi_value(value).map(|v| self.xfout_lovel = v)),
            ("xfout_hivel", None) => ok(midi_value(value).map(|v| self.xfout_hivel = v)),
            ("xf_velcurve", None) => ok(match value {
                "power" => Some(true),
                "gain" => Some(false),
                _ => None,
            }
            .map(|p| self.xf_power = p)),
            ("ampeg_delay", None) => ok(float(value).map(|v| self.ampeg.delay = v.max(0.0))),
            ("ampeg_attack", None) => ok(float(value).map(|v| self.ampeg.attack = v.max(0.0))),
            ("ampeg_hold", None) => ok(float(value).map(|v| self.ampeg.hold = v.max(0.0))),
            ("ampeg_decay", None) => ok(float(value).map(|v| self.ampeg.decay = v.max(0.0))),
            ("ampeg_sustain", None) => ok(float(value).map(|v| self.ampeg.sustain = (v / 100.0).clamp(0.0, 1.0))),
            ("ampeg_release", None) => ok(float(value).map(|v| self.ampeg.release = v.max(0.0))),

            (base, Some(cc)) => self.apply_modulation(base, cc, value),
            _ => Applied::Unknown,
        }
    }

    /// `<target>_onccN`, `<target>_ccN`, `<target>ccN` (SFZ v1) and
    /// `<target>_curveccN`.
    fn apply_modulation(&mut self, base: &str, cc: u16, value: &str) -> Applied {
        let (target_name, is_curve) = if let Some(t) = base.strip_suffix("_curvecc") {
            (t, true)
        } else if let Some(t) = base.strip_suffix("_oncc").or_else(|| base.strip_suffix("_cc")).or_else(|| base.strip_suffix("cc")) {
            (t, false)
        } else {
            return Applied::Unknown;
        };
        let Some(target) = Target::from_name(target_name) else {
            return Applied::Unknown;
        };
        let modulation = match self.mods.iter_mut().position(|m| m.target == target && m.cc == cc) {
            Some(i) => &mut self.mods[i],
            None => {
                self.mods.push(CcMod { target, cc, depth: 0.0, curve: 0 });
                self.mods.last_mut().unwrap()
            }
        };
        if is_curve {
            match int(value) {
                Some(curve) if curve >= 0 => modulation.curve = curve as u16,
                _ => return Applied::BadValue,
            }
        } else {
            match float(value) {
                Some(depth) => modulation.depth = depth,
                None => return Applied::BadValue,
            }
        }
        Applied::Yes
    }

    /// A note, or `-1` for "no key".
    fn note_or_none(&self, value: &str, ctx: &ParseContext) -> Option<Option<u8>> {
        if int(value) == Some(-1) {
            return Some(None);
        }
        note(value, ctx).map(Some)
    }
}

fn cc_range(ranges: &mut Vec<CcRange>, cc: u16) -> &mut CcRange {
    match ranges.iter().position(|r| r.cc == cc) {
        Some(i) => &mut ranges[i],
        None => {
            ranges.push(CcRange { cc, lo: 0.0, hi: 127.0 });
            ranges.last_mut().unwrap()
        }
    }
}

/// Splits a trailing number off an opcode name: `amplitude_oncc7` →
/// (`amplitude_oncc`, 7). Names that end in a digit as part of the name aren't
/// opcodes we know, so this never gets in the way.
pub fn split_number(name: &str) -> (&str, Option<u16>) {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits == name.len() {
        return (name, None);
    }
    let (base, number) = name.split_at(name.len() - digits);
    match number.parse() {
        Ok(n) => (base, Some(n)),
        Err(_) => (name, None),
    }
}

pub fn float(value: &str) -> Option<f32> {
    value.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

pub fn int(value: &str) -> Option<i64> {
    let value = value.trim();
    // Some instruments write integers as "60.0".
    value.parse::<i64>().ok().or_else(|| float(value).map(|v| v.round() as i64))
}

fn midi_value(value: &str) -> Option<u8> {
    int(value).map(|v| v.clamp(0, 127) as u8)
}

/// A note number or name (`c4` = 60, `c#4`, `db4`), plus the note offset.
pub fn note(value: &str, ctx: &ParseContext) -> Option<u8> {
    let value = value.trim();
    let number = match int(value) {
        Some(n) => n,
        None => {
            let lower = value.to_ascii_lowercase();
            let mut chars = lower.chars();
            let base = match chars.next()? {
                'c' => 0,
                'd' => 2,
                'e' => 4,
                'f' => 5,
                'g' => 7,
                'a' => 9,
                'b' => 11,
                _ => return None,
            };
            let rest = chars.as_str();
            let (accidental, octave) = if let Some(o) = rest.strip_prefix('#') {
                (1, o)
            } else if let Some(o) = rest.strip_prefix('b') {
                (-1, o)
            } else {
                (0, rest)
            };
            let octave: i64 = octave.parse().ok()?;
            (octave + 1) * 12 + base + accidental
        }
    };
    let number = number + ctx.note_offset as i64;
    (0..=127).contains(&number).then_some(number as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes() {
        let ctx = ParseContext::default();
        assert_eq!(note("c4", &ctx), Some(60));
        assert_eq!(note("C#0", &ctx), Some(13));
        assert_eq!(note("db4", &ctx), Some(61));
        assert_eq!(note("a-1", &ctx), Some(9));
        assert_eq!(note("108", &ctx), Some(108));
        assert_eq!(note("128", &ctx), None);
        assert_eq!(note("c4", &ParseContext { note_offset: 12 }), Some(72));
    }

    #[test]
    fn modulation_opcodes_combine_depth_and_curve() {
        let mut r = Region::default();
        let ctx = ParseContext::default();
        assert_eq!(r.apply("amplitude_curvecc7", "4", &ctx), Applied::Yes);
        assert_eq!(r.apply("amplitude_oncc7", "100", &ctx), Applied::Yes);
        assert_eq!(r.apply("ampeg_releasecc72", "2", &ctx), Applied::Yes);
        assert_eq!(r.apply("pan_cc10", "100", &ctx), Applied::Yes);
        assert_eq!(r.apply("cutoff_oncc74", "1200", &ctx), Applied::Unknown);
        assert_eq!(
            r.mods,
            [
                CcMod { target: Target::Amplitude, cc: 7, depth: 100.0, curve: 4 },
                CcMod { target: Target::EgRelease, cc: 72, depth: 2.0, curve: 0 },
                CcMod { target: Target::Pan, cc: 10, depth: 100.0, curve: 0 },
            ]
        );
    }

    #[test]
    fn key_ranges() {
        let mut r = Region::default();
        let ctx = ParseContext::default();
        r.apply("key", "62", &ctx);
        assert_eq!((r.keys, r.pitch_keycenter), (Some((62, 62)), 62));
        r.apply("lokey", "-1", &ctx);
        assert_eq!(r.keys, None);
        r.apply("lokey", "21", &ctx);
        r.apply("hikey", "22", &ctx);
        assert_eq!(r.keys, Some((21, 22)));
    }
}
