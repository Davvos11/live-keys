/// A single MIDI channel-voice message (note on/off, CC, pitch bend, ...) with its
/// position within the current audio block.
///
/// Limited to 3 data bytes: covers all standard channel-voice messages, which is all
/// a MIDI keyboard sends. Sysex and other variable-length messages are dropped by
/// [`MidiEvent::from_raw`] since a fixed-size event is required to move these through
/// a lock-free ring buffer without allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    pub frame_offset: u32,
    data: [u8; 3],
    len: u8,
}

impl MidiEvent {
    pub fn from_raw(frame_offset: u32, bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 3 {
            return None;
        }
        let mut data = [0u8; 3];
        data[..bytes.len()].copy_from_slice(bytes);
        Some(MidiEvent {
            frame_offset,
            data,
            len: bytes.len() as u8,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}
