use crate::MidiEvent;

pub type MidiProducer = rtrb::Producer<MidiEvent>;
pub type MidiConsumer = rtrb::Consumer<MidiEvent>;

/// Creates a lock-free, single-producer/single-consumer ring buffer for passing MIDI
/// events from the MIDI input thread into the real-time audio thread.
pub fn midi_ring_buffer(capacity: usize) -> (MidiProducer, MidiConsumer) {
    rtrb::RingBuffer::new(capacity)
}
