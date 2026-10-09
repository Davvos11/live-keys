use crate::{MidiEvent, ParamChange};

pub use rtrb::{Consumer, Producer, PushError};

/// Creates a lock-free, single-producer/single-consumer ring buffer of `capacity`
/// slots. Used to move data from a control thread (MIDI input, a plugin's own GUI)
/// into the real-time audio thread without allocating or locking there.
pub fn ring_buffer<T>(capacity: usize) -> (rtrb::Producer<T>, rtrb::Consumer<T>) {
    rtrb::RingBuffer::new(capacity)
}

pub type MidiProducer = rtrb::Producer<MidiEvent>;
pub type MidiConsumer = rtrb::Consumer<MidiEvent>;

/// Ring buffer for passing MIDI events from the MIDI input thread into the audio
/// thread.
pub fn midi_ring_buffer(capacity: usize) -> (MidiProducer, MidiConsumer) {
    ring_buffer(capacity)
}

pub type ParamProducer = rtrb::Producer<ParamChange>;
pub type ParamConsumer = rtrb::Consumer<ParamChange>;

/// Ring buffer for passing control-port changes (e.g. from a plugin's own GUI) into
/// the audio thread.
pub fn param_ring_buffer(capacity: usize) -> (ParamProducer, ParamConsumer) {
    ring_buffer(capacity)
}
