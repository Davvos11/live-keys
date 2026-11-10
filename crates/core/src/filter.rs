use crate::MidiEvent;

/// Which incoming MIDI a rack slot responds to: a keyboard split/layer zone.
///
/// The note and velocity ranges are inclusive. A range whose low bound is above its
/// high bound matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiFilter {
    /// Which of the host's MIDI inputs to listen to: bit `n` for input `n`.
    pub inputs: u32,
    /// 0-based MIDI channel to accept, or `None` to accept every channel (omni).
    pub channel: Option<u8>,
    pub note_low: u8,
    pub note_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    /// Semitones added to every note it lets through (after the note range,
    /// which is about the keys played).
    pub transpose: i8,
}

impl Default for MidiFilter {
    /// Lets everything through.
    fn default() -> Self {
        Self {
            inputs: ALL_INPUTS,
            channel: None,
            note_low: 0,
            note_high: 127,
            velocity_low: 1,
            velocity_high: 127,
            transpose: 0,
        }
    }
}

/// Every MIDI input, as a [`MidiFilter::inputs`] mask.
pub const ALL_INPUTS: u32 = u32::MAX;

/// Applies a [`MidiFilter`] to a stream of events for one slot, while tracking which
/// notes it has let through.
///
/// Note-offs are routed by that tracking, not by the filter: a note-off passes if and
/// only if this router passed the matching note-on, transposed as that note-on was.
/// So changing the filter (transposition included) while keys are held never leaves
/// a note stuck, and a note-off for a key that went to a different slot (same note
/// number, other channel/zone) never cuts this slot's voice short. Fixed-size and
/// allocation-free, so it runs on the audio thread.
#[derive(Debug, Clone)]
pub struct MidiRouter {
    filter: MidiFilter,
    /// One bit per (channel, note) currently held in this slot, as played.
    held: [u128; 16],
    /// Per (channel, note) held: the transposition its note-on got.
    shift: [[i8; 128]; 16],
}

impl MidiRouter {
    pub fn new(filter: MidiFilter) -> Self {
        Self {
            filter,
            held: [0; 16],
            shift: [[0; 128]; 16],
        }
    }

    pub fn filter(&self) -> MidiFilter {
        self.filter
    }

    /// Forgets which notes are held — for when the plugin behind this router is
    /// replaced, since the new one never received those note-ons.
    pub fn clear_held(&mut self) {
        self.held = [0; 16];
    }

    /// Replaces the filter. Notes already held keep their tracking, so their
    /// note-offs still get through.
    pub fn set_filter(&mut self, filter: MidiFilter) {
        self.filter = filter;
    }

    /// What to pass to this slot's plugin for `event`, which arrived on the MIDI
    /// inputs in mask `inputs`: the event (a note transposed), or `None`.
    pub fn route(&mut self, inputs: u32, event: &MidiEvent) -> Option<MidiEvent> {
        let bytes = event.as_slice();
        let status = bytes[0];
        if status >= 0xF0 {
            // System messages (clock, start/stop, ...) aren't channel-specific.
            return (self.filter.inputs & inputs != 0).then_some(*event);
        }
        let channel = usize::from(status & 0x0F);
        let kind = status & 0xF0;
        let note = bytes.get(1).copied().unwrap_or(0) & 0x7F;
        let velocity = bytes.get(2).copied().unwrap_or(0);
        let bit = 1u128 << note;
        let held = self.held[channel] & bit != 0;
        // A held note's events keep the transposition its note-on got.
        let shifted = |shift: i8| event.with_data1(note.saturating_add_signed(shift));

        let is_note_off = kind == 0x80 || (kind == 0x90 && velocity == 0);
        if is_note_off {
            self.held[channel] &= !bit;
            return held.then(|| shifted(self.shift[channel][usize::from(note)]));
        }

        let filter = &self.filter;
        if filter.inputs & inputs == 0 {
            return None;
        }
        if filter.channel.is_some_and(|wanted| usize::from(wanted) != channel) {
            return None;
        }
        match kind {
            0x90 => {
                let accepted = (filter.note_low..=filter.note_high).contains(&note)
                    && (filter.velocity_low..=filter.velocity_high).contains(&velocity);
                // Transposed off the end of the MIDI range: nothing to play.
                let target = i16::from(note) + i16::from(filter.transpose);
                if !accepted || !(0..=127).contains(&target) {
                    return None;
                }
                self.held[channel] |= bit;
                self.shift[channel][usize::from(note)] = filter.transpose;
                Some(shifted(filter.transpose))
            }
            // Polyphonic aftertouch belongs to a specific key.
            0xA0 => held.then(|| shifted(self.shift[channel][usize::from(note)])),
            // CCs (sustain, mod wheel, ...), program change, channel pressure, pitch
            // bend: not tied to a key, so only the channel filter applies.
            _ => Some(*event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bytes: &[u8]) -> MidiEvent {
        MidiEvent::from_raw(0, bytes).unwrap()
    }

    fn split(note_low: u8, note_high: u8) -> MidiFilter {
        MidiFilter {
            note_low,
            note_high,
            ..Default::default()
        }
    }

    #[test]
    fn note_and_velocity_ranges_are_inclusive() {
        let mut router = MidiRouter::new(MidiFilter {
            velocity_low: 10,
            velocity_high: 100,
            ..split(60, 72)
        });
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 60, 10])).is_some());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 72, 100])).is_some());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 59, 64])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 73, 64])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 65, 9])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 65, 101])).is_none());
    }

    #[test]
    fn channel_filter_applies_to_non_note_messages() {
        let mut router = MidiRouter::new(MidiFilter {
            channel: Some(1),
            ..Default::default()
        });
        assert!(router.route(ALL_INPUTS, &ev(&[0xB1, 64, 127])).is_some());
        assert!(router.route(ALL_INPUTS, &ev(&[0xB0, 64, 127])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 60, 64])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0xF8])).is_some());
    }

    #[test]
    fn note_off_follows_its_note_on_not_the_current_filter() {
        let mut router = MidiRouter::new(split(60, 72));
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 60, 64])).is_some());
        // Range moved away while the key is held: its release must still arrive.
        router.set_filter(split(0, 10));
        assert!(router.route(ALL_INPUTS, &ev(&[0x80, 60, 0])).is_some());
        // Only once, and never for a note this slot didn't play.
        assert!(router.route(ALL_INPUTS, &ev(&[0x80, 60, 0])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 61, 0])).is_none());
    }

    #[test]
    fn note_off_on_another_channel_does_not_release() {
        let mut router = MidiRouter::new(MidiFilter {
            channel: Some(0),
            ..Default::default()
        });
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 60, 64])).is_some());
        assert!(router.route(ALL_INPUTS, &ev(&[0x81, 60, 0])).is_none());
        assert!(router.route(ALL_INPUTS, &ev(&[0x80, 60, 0])).is_some());
    }

    #[test]
    fn inverted_range_matches_nothing() {
        let mut router = MidiRouter::new(split(72, 60));
        assert!(router.route(ALL_INPUTS, &ev(&[0x90, 65, 64])).is_none());
    }

    #[test]
    fn only_listens_to_its_inputs_but_note_offs_follow_tracking() {
        let mut router = MidiRouter::new(MidiFilter {
            inputs: 0b10,
            ..Default::default()
        });
        assert!(router.route(0b01, &ev(&[0x90, 60, 100])).is_none());
        assert!(router.route(0b11, &ev(&[0x90, 62, 100])).is_some());
        assert!(router.route(0b01, &ev(&[0xB0, 64, 127])).is_none());
        // Turning the input off while a key is held still lets its note-off through.
        router.set_filter(MidiFilter {
            inputs: 0,
            ..Default::default()
        });
        assert!(router.route(0b10, &ev(&[0x80, 62, 0])).is_some());
        assert!(router.route(0b10, &ev(&[0x90, 62, 100])).is_none());
    }

    fn note(event: Option<MidiEvent>) -> Option<u8> {
        event.map(|event| event.as_slice()[1])
    }

    #[test]
    fn transposes_notes_and_their_note_offs_keep_the_shift_they_started_with() {
        let mut router = MidiRouter::new(MidiFilter {
            transpose: 12,
            ..split(60, 72)
        });
        // The range is about the key played, not the note sent.
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0x90, 60, 64]))), Some(72));
        assert_eq!(router.route(ALL_INPUTS, &ev(&[0x90, 80, 64])), None);
        // Transposition changed while the key is held: its aftertouch and
        // release still go to the note that's sounding.
        router.set_filter(MidiFilter {
            transpose: -5,
            ..split(60, 72)
        });
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0xA0, 60, 30]))), Some(72));
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0x80, 60, 0]))), Some(72));
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0x90, 62, 64]))), Some(57));
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0x90, 62, 0]))), Some(57));
    }

    #[test]
    fn notes_transposed_out_of_range_are_dropped_with_their_note_offs() {
        let mut router = MidiRouter::new(MidiFilter {
            transpose: 24,
            ..Default::default()
        });
        assert_eq!(router.route(ALL_INPUTS, &ev(&[0x90, 120, 64])), None);
        assert_eq!(router.route(ALL_INPUTS, &ev(&[0x80, 120, 0])), None);
        assert_eq!(note(router.route(ALL_INPUTS, &ev(&[0x90, 103, 64]))), Some(127));
    }
}
