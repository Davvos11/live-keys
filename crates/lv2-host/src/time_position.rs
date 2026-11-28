//! The host's musical time for LV2 plugins: a `time:Position` atom object sent
//! to every atom input that says it supports one (`atom:supports
//! time:Position` — LSP's plugins, Helm, x42's step sequencer and MIDI filter),
//! at frame 0 of a block, when something changed: the first block, start or
//! stop, tempo or time signature. In between, plugins extrapolate from the
//! last one (its `time:speed` says whether time moves). Encoded by hand into a
//! fixed buffer, so nothing allocates on the audio thread.

use live_keys_core::Transport;

const TIME: &str = "http://lv2plug.in/ns/ext/time#";
const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";

/// The properties sent, each a 16-byte header and an 8-byte (padded) value.
const PROPERTIES: usize = 8;
/// An object body (id, otype) and its properties.
pub(crate) const BODY_BYTES: usize = 8 + PROPERTIES * 24;

/// The URIDs a `time:Position` needs.
#[derive(Clone, Copy)]
pub(crate) struct Urids {
    pub object: u32,
    position: u32,
    long: u32,
    float: u32,
    int: u32,
    double: u32,
    frame: u32,
    speed: u32,
    bar: u32,
    bar_beat: u32,
    beat: u32,
    beat_unit: u32,
    beats_per_bar: u32,
    beats_per_minute: u32,
}

impl Urids {
    pub fn new(mut map: impl FnMut(&str) -> u32) -> Self {
        let mut time = |name: &str| map(&format!("{TIME}{name}"));
        let (position, frame, speed, bar, bar_beat, beat, beat_unit, beats_per_bar, beats_per_minute) = (
            time("Position"),
            time("frame"),
            time("speed"),
            time("bar"),
            time("barBeat"),
            time("beat"),
            time("beatUnit"),
            time("beatsPerBar"),
            time("beatsPerMinute"),
        );
        let mut atom = |name: &str| map(&format!("{ATOM}{name}"));
        Self {
            object: atom("Object"),
            long: atom("Long"),
            float: atom("Float"),
            int: atom("Int"),
            double: atom("Double"),
            position,
            frame,
            speed,
            bar,
            bar_beat,
            beat,
            beat_unit,
            beats_per_bar,
            beats_per_minute,
        }
    }
}

/// What a plugin extrapolates from: a change here is sent.
#[derive(Clone, Copy, PartialEq)]
struct Basis {
    tempo: f64,
    playing: bool,
    beats_per_bar: u32,
    beat_unit: u32,
}

/// One plugin's time:Position state: its ports, what it was last told, and the
/// message for the next block.
pub(crate) struct TimePosition {
    urids: Urids,
    /// Indices into the plugin's atom inputs.
    pub ports: Vec<usize>,
    sent: Option<Basis>,
    /// A message waiting for the next block.
    pending: bool,
    body: [u8; BODY_BYTES],
}

impl TimePosition {
    pub fn new(urids: Urids, ports: Vec<usize>) -> Self {
        Self { urids, ports, sent: None, pending: false, body: [0; BODY_BYTES] }
    }

    /// Notes the transport for the next block, preparing a message if the
    /// plugin should hear about it.
    pub fn set(&mut self, transport: &Transport) {
        let basis = Basis {
            tempo: transport.tempo,
            playing: transport.playing,
            beats_per_bar: transport.beats_per_bar,
            beat_unit: transport.beat_unit,
        };
        if self.sent != Some(basis) {
            self.sent = Some(basis);
            encode(&self.urids, transport, &mut self.body);
            self.pending = true;
        }
    }

    /// The message for this block, if there is one: an atom object body of
    /// type [`Urids::object`].
    pub fn take(&mut self) -> Option<&[u8; BODY_BYTES]> {
        std::mem::take(&mut self.pending).then_some(&self.body)
    }

    pub fn object_type(&self) -> u32 {
        self.urids.object
    }
}

/// Writes `transport` as a time:Position object body into `body`.
fn encode(urids: &Urids, transport: &Transport, body: &mut [u8; BODY_BYTES]) {
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        body[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    // The object: no id, its type.
    put(&0u32.to_ne_bytes());
    put(&urids.position.to_ne_bytes());
    let in_bar = transport.beats - transport.bar_start;
    let properties: [(u32, u32, [u8; 8]); PROPERTIES] = [
        (urids.frame, urids.long, transport.frame.to_ne_bytes()),
        (urids.speed, urids.float, float(if transport.playing { 1.0 } else { 0.0 })),
        (urids.bar, urids.long, transport.bar.to_ne_bytes()),
        (urids.bar_beat, urids.float, float(in_bar as f32)),
        (urids.beat, urids.double, transport.beats.to_ne_bytes()),
        (urids.beat_unit, urids.int, int(transport.beat_unit as i32)),
        (urids.beats_per_bar, urids.float, float(transport.beats_per_bar as f32)),
        (urids.beats_per_minute, urids.float, float(transport.tempo as f32)),
    ];
    for (key, value_type, value) in properties {
        let size: u32 = if value_type == urids.long || value_type == urids.double { 8 } else { 4 };
        // key, context, then the value's atom header and body (padded to 8).
        put(&key.to_ne_bytes());
        put(&0u32.to_ne_bytes());
        put(&size.to_ne_bytes());
        put(&value_type.to_ne_bytes());
        put(&value);
    }
}

fn float(value: f32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&value.to_ne_bytes());
    bytes
}

fn int(value: i32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&value.to_ne_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urids() -> Urids {
        let mut names = Vec::new();
        Urids::new(|uri| {
            names.push(uri.to_string());
            names.len() as u32
        })
    }

    /// The property with `key` in an encoded body: its type and value bytes.
    fn property(body: &[u8], key: u32) -> (u32, [u8; 8]) {
        body[8..]
            .chunks(24)
            .map(|p| {
                let word = |at: usize| u32::from_ne_bytes(p[at..at + 4].try_into().unwrap());
                (word(0), word(12), p[16..24].try_into().unwrap())
            })
            .find(|(k, ..)| *k == key)
            .map(|(_, value_type, value)| (value_type, value))
            .unwrap()
    }

    #[test]
    fn sends_only_when_the_basis_changes() {
        let urids = urids();
        let mut position = TimePosition::new(urids, vec![0]);
        let playing = Transport { playing: true, beats_per_bar: 3, beats: 4.5, bar_start: 3.0, bar: 1, frame: 99, ..Transport::default() };
        position.set(&playing);
        let body = *position.take().unwrap();
        assert_eq!(u32::from_ne_bytes(body[4..8].try_into().unwrap()), urids.position);
        assert_eq!(property(&body, urids.frame), (urids.long, 99i64.to_ne_bytes()));
        assert_eq!(property(&body, urids.speed), (urids.float, float(1.0)));
        assert_eq!(property(&body, urids.bar), (urids.long, 1i64.to_ne_bytes()));
        assert_eq!(property(&body, urids.bar_beat), (urids.float, float(1.5)));
        assert_eq!(property(&body, urids.beats_per_bar), (urids.float, float(3.0)));
        assert_eq!(property(&body, urids.beat_unit), (urids.int, int(4)));
        assert_eq!(property(&body, urids.beats_per_minute), (urids.float, float(120.0)));
        // Time moving on isn't news; stopping, or a new tempo, is.
        position.set(&Transport { beats: 5.0, frame: 500, ..playing });
        assert!(position.take().is_none());
        position.set(&Transport { playing: false, ..playing });
        assert!(position.take().is_some());
        position.set(&Transport { playing: false, tempo: 90.0, ..playing });
        assert!(position.take().is_some());
        assert!(position.take().is_none());
    }
}
